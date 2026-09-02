//! Read-only IMAP server — Phase 4 spike.
//!
//! Deliberately minimal: enough for Thunderbird to log in, list folders, select
//! one, and fetch a message. The point is to find out whether `imap-next` works
//! as a server foundation before building on it, and to learn what Thunderbird
//! actually asks for — every unhandled command is logged rather than guessed at.
//!
//! Passwords are verified against `users.password_hash` (Argon2id), and the
//! listener speaks TLS when a certificate is configured.
//!
//! **Without a certificate it serves plaintext**, which is fine on loopback and
//! unacceptable anywhere else: IMAP LOGIN sends the password in the clear, so a
//! plaintext listener on a public address hands it to anyone on the path. The
//! server refuses to bind a non-loopback address without TLS rather than
//! leaving that to be noticed later.
//!
//! Nothing here can mutate the archive: there is no APPEND, STORE, EXPUNGE or
//! DELETE, and the only writes in the whole program are ingest and read-state.

use anyhow::{Context, Result};
use imap_next::imap_types::core::{IString, Literal, NString};
use imap_next::imap_types::core::{Tag, Vec1};
use imap_next::imap_types::datetime::DateTime;
use imap_next::imap_types::fetch::{MessageDataItem, MessageDataItemName, Section};
use imap_next::imap_types::flag::FlagFetch;
use imap_next::imap_types::flag::{Flag, FlagPerm};
use imap_next::imap_types::mailbox::{ListMailbox, Mailbox};
use imap_next::imap_types::response::{Capability, Code, Data, Greeting, Status};
use imap_next::server::{Options, Server};
use imap_next::stream::Stream;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::db;
use crate::fetch as fetchlib;
use crate::listen;
use crate::naming;
use crate::store::Store;

/// Everything the connection knows once a user has logged in.
struct Session {
    user_id: i64,
    bucket: String,
    /// Currently selected folder, if any: (folder id, name).
    selected: Option<(i64, String)>,
}

pub async fn run(
    config: &Arc<Config>,
    pool: &PgPool,
    bind: &str,
    allow_plaintext: bool,
) -> Result<()> {
    // IMAP LOGIN puts the password on the wire in clear text. Serving plaintext
    // on anything but loopback would hand every password to whoever is on the
    // path, so that combination is refused rather than warned about. The rule
    // is shared with the web listener -- see listen.rs.
    let transport = listen::resolve(
        config,
        bind,
        allow_plaintext,
        "IMAP",
        "IMAP sends passwords in clear text",
        // Unchanged behaviour: a loopback listener still serves TLS when one is
        // configured, which is how the TLS path gets tested locally.
        listen::Loopback::MayUseTls,
    )?;
    if !transport.is_tls() && !listen::is_loopback(bind) {
        eprintln!("  Every IMAP password on this listener crosses the network in clear text.");
    }

    let tls = match &transport {
        listen::Transport::Tls(reloader) => Some(Arc::clone(reloader)),
        listen::Transport::Plaintext { .. } => None,
    };

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;

    match &transport {
        listen::Transport::Tls(_) => println!("IMAP listening on {bind} (TLS)"),
        listen::Transport::Plaintext { loopback: true } => {
            println!("IMAP listening on {bind} (plaintext, loopback)")
        }
        listen::Transport::Plaintext { loopback: false } => {
            println!("IMAP listening on {bind} (PLAINTEXT, NOT loopback)")
        }
    }
    println!("  Passwords verified against users.password_hash.");

    loop {
        // A failed accept must not take down the listener: one bad connection
        // should not end the service.
        let (socket, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        println!("\n--- connection from {peer} ---");

        let config = Arc::clone(config);
        let pool = pool.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let result = match &tls {
                Some(reloader) => match reloader.acceptor().await.accept(socket).await {
                    // accept() yields the server-side struct; Stream::tls wants the
                    // enum that covers both directions.
                    Ok(stream) => {
                        let stream = tokio_rustls::TlsStream::Server(stream);
                        serve(&config, &pool, Stream::tls(stream)).await
                    }
                    Err(e) => {
                        eprintln!("TLS handshake failed: {e}");
                        return;
                    }
                },
                None => serve(&config, &pool, Stream::insecure(socket)).await,
            };
            if let Err(e) = result {
                eprintln!("connection error: {e:#}");
            }
            println!("--- disconnected ---");
        });
    }
}

async fn serve(config: &Config, pool: &PgPool, mut stream: Stream) -> Result<()> {
    let greeting = Greeting::ok(None, "email-archiver (read-only archive)")
        .map_err(|e| anyhow::anyhow!("greeting: {e:?}"))?;
    let mut server = Server::new(Options::default(), greeting);
    let mut session: Option<Session> = None;

    loop {
        let event = stream.next(&mut server).await?;

        use imap_next::server::Event;
        match event {
            Event::CommandReceived { command } => {
                let tag = command.tag.clone();
                // Log every command: the point of the spike is learning what
                // Thunderbird actually sends.
                println!("  C: {:?}", command.body);

                if handle(&mut server, &mut session, config, pool, command).await? {
                    // LOGOUT
                    stream.flush().await?;
                    return Ok(());
                }
                let _ = tag;
            }
            Event::IdleCommandReceived { tag } => {
                // Accepted but never notified: an archive does not change
                // underneath the client, so IDLE has nothing to report.
                println!("  C: IDLE");
                // Accept the IDLE, then immediately let it sit: nothing ever
                // changes under a read-only archive, so there is nothing to
                // report until the client sends DONE.
                let ccr = imap_next::imap_types::response::CommandContinuationRequest::basic(
                    None, "idling",
                )
                .unwrap();
                let _ = tag;
                server.idle_accept(ccr).ok();
            }
            Event::IdleDoneReceived => {}
            Event::ResponseSent { .. } | Event::GreetingSent { .. } => {}
            other => println!("  (unhandled event: {other:?})"),
        }
    }
}

/// Returns Ok(true) when the connection should close.
async fn handle(
    server: &mut Server,
    session: &mut Option<Session>,
    config: &Config,
    pool: &PgPool,
    command: imap_next::imap_types::command::Command<'static>,
) -> Result<bool> {
    use imap_next::imap_types::command::CommandBody;

    let tag = command.tag;

    match command.body {
        CommandBody::Capability => {
            server.enqueue_data(Data::Capability(Vec1::from(Capability::Imap4Rev1)));
            ok(server, tag, "CAPABILITY done");
        }

        CommandBody::Login { username, password } => {
            let login = String::from_utf8_lossy(username.as_ref()).to_string();
            let secret = String::from_utf8_lossy(password.declassify().as_ref()).to_string();
            match db::authenticate(pool, &login, &secret).await? {
                Some((user_id, bucket)) => {
                    println!("  authenticated {login}");
                    *session = Some(Session {
                        user_id,
                        bucket,
                        selected: None,
                    });
                    ok(server, tag, "LOGIN done");
                }
                // Deliberately does not distinguish an unknown user from a
                // wrong password: telling an attacker which logins exist is
                // free information they should not get.
                None => {
                    println!("  failed login for {login:?}");
                    no(server, tag, "authentication failed");
                }
            }
        }

        CommandBody::Fetch {
            sequence_set,
            macro_or_item_names,
            uid,
        } => {
            let Some(sess) = session.as_ref() else {
                no(server, tag, "not authenticated");
                return Ok(false);
            };
            let Some((folder_id, _)) = sess.selected.clone() else {
                no(server, tag, "no mailbox selected");
                return Ok(false);
            };

            let rows = folder_messages(pool, folder_id).await?;
            let names = item_names(&macro_or_item_names);

            // Sequence numbers are positions in uid order; UID FETCH addresses
            // the same rows by uid instead.
            let max = rows.len() as u32;
            let wanted: Vec<usize> = (0..rows.len())
                .filter(|i| {
                    let seq = (i + 1) as u32;
                    let key = if uid { rows[*i].uid as u32 } else { seq };
                    in_set(&sequence_set, key, max)
                })
                .collect();

            println!("    fetch: {} of {} messages", wanted.len(), rows.len());

            // Only touch S3 when a body was actually asked for. Thunderbird
            // builds its message list from metadata alone, and fetching bodies
            // it never requested would make listing a folder pull gigabytes.
            // Distinguish "needs the header block" from "needs the whole
            // message". Thunderbird's folder listing only ever asks for header
            // fields, and answering those from the cache is what turns opening
            // a large folder from thousands of S3 round trips into one query.
            let needs_full_body = names.iter().any(|n| {
                matches!(
                    n,
                    MessageDataItemName::Rfc822 | MessageDataItemName::Rfc822Text
                ) || matches!(
                    n,
                    MessageDataItemName::BodyExt { section, .. }
                        if !matches!(
                            section,
                            Some(Section::Header(_)) | Some(Section::HeaderFields(_, _))
                        )
                )
            });
            let store = if needs_full_body {
                Some(Store::open(config, &sess.bucket).await?)
            } else {
                None
            };

            for i in wanted {
                let row = &rows[i];
                let seq = std::num::NonZeroU32::new((i + 1) as u32).unwrap();

                // Cached headers serve header-only requests; anything else
                // needs the real object. A row without a cached block simply
                // falls back, so a partial backfill is slow, never wrong.
                let raw = match &store {
                    Some(st) => Some(st.get_message(&row.blake3).await?),
                    None => match &row.headers {
                        Some(h) => Some(h.clone()),
                        None => Some(
                            Store::open(config, &sess.bucket)
                                .await?
                                .get_message(&row.blake3)
                                .await?,
                        ),
                    },
                };

                let mut items: Vec<MessageDataItem> = Vec::new();
                for name in &names {
                    if let Some(item) = build_item(name, row, raw.as_deref()) {
                        items.push(item);
                    }
                }
                // UID is always included for a UID FETCH, whether asked for or
                // not: clients rely on it to correlate the response.
                if uid && !names.contains(&MessageDataItemName::Uid) {
                    items.push(MessageDataItem::Uid(
                        std::num::NonZeroU32::new(row.uid as u32).unwrap(),
                    ));
                }
                if items.is_empty() {
                    continue;
                }
                server.enqueue_data(Data::Fetch {
                    seq,
                    items: Vec1::try_from(items).unwrap(),
                });
            }

            ok(server, tag, "FETCH done");
            return Ok(false);
        }

        CommandBody::Logout => {
            server.enqueue_status(Status::bye(None, "closing").unwrap());
            ok(server, tag, "LOGOUT done");
            return Ok(true);
        }

        CommandBody::Noop => ok(server, tag, "NOOP done"),

        CommandBody::List {
            mailbox_wildcard, ..
        }
        | CommandBody::Lsub {
            mailbox_wildcard, ..
        } => {
            let Some(sess) = session.as_ref() else {
                no(server, tag, "not authenticated");
                return Ok(false);
            };
            let pattern = wildcard_text(&mailbox_wildcard);

            // INBOX always exists and is always empty. Nothing is delivered to
            // this server — every archived message lives under an account
            // namespace — but Thunderbird requires an INBOX to exist, and an
            // empty one is the truthful answer rather than aliasing somebody's
            // mail into it.
            let mut names = vec!["INBOX".to_string()];
            names.extend(
                folders_for(pool, sess.user_id)
                    .await?
                    .into_iter()
                    .map(|(_, n)| n),
            );

            for name in names {
                if !matches_pattern(&pattern, &name) {
                    continue;
                }
                server.enqueue_data(Data::List {
                    // No attributes claimed. \Noinferiors would assert these
                    // folders cannot have children, which stops being true the
                    // moment source hierarchy is translated into ours.
                    items: vec![],
                    delimiter: Some(
                        imap_next::imap_types::core::QuotedChar::try_from('/').unwrap(),
                    ),
                    mailbox: Mailbox::try_from(name).unwrap(),
                });
            }
            ok(server, tag, "LIST done");
        }

        CommandBody::Select { mailbox } | CommandBody::Examine { mailbox } => {
            let Some(sess) = session.as_mut() else {
                no(server, tag, "not authenticated");
                return Ok(false);
            };
            let wanted = mailbox_text(&mailbox);

            let (folder_id, exists, uidvalidity, uidnext) = if wanted == "INBOX" {
                (None, 0i64, 1i64, 1i64)
            } else {
                match folder_meta(pool, sess.user_id, &wanted).await? {
                    Some(m) => (Some(m.0), m.1, m.2, m.3),
                    None => {
                        no(server, tag, "no such mailbox");
                        return Ok(false);
                    }
                }
            };

            server.enqueue_data(Data::Exists(exists as u32));
            server.enqueue_data(Data::Recent(0));
            server.enqueue_data(Data::Flags(vec![Flag::Seen]));
            server.enqueue_status(
                Status::ok(
                    None,
                    Some(Code::UidValidity(
                        std::num::NonZeroU32::new(uidvalidity.max(1) as u32).unwrap(),
                    )),
                    "uid validity",
                )
                .unwrap(),
            );
            server.enqueue_status(
                Status::ok(
                    None,
                    Some(Code::UidNext(
                        std::num::NonZeroU32::new(uidnext.max(1) as u32).unwrap(),
                    )),
                    "next uid",
                )
                .unwrap(),
            );
            // \Seen is the only flag a client may set, so it is the only one
            // listed as permanent. Everything else about a message is fixed.
            server.enqueue_status(
                Status::ok(
                    None,
                    Some(Code::PermanentFlags(vec![FlagPerm::Flag(Flag::Seen)])),
                    "limited",
                )
                .unwrap(),
            );

            sess.selected = folder_id.map(|id| (id, wanted.clone()));
            // READ-ONLY even for SELECT: this archive is never writable, and
            // saying so up front is better than refusing writes later.
            server.enqueue_status(
                Status::ok(Some(tag), Some(Code::ReadOnly), "selected (read-only)").unwrap(),
            );
            return Ok(false);
        }

        other => {
            println!("  !! not implemented: {other:?}");
            no(server, tag, "not implemented in this spike");
        }
    }

    let _ = (config, session);
    Ok(false)
}

/// Folders visible to a user, as `<account label>/<source folder name>`.
///
/// Namespaced by account because one archive user may hold several source
/// mailboxes, each with its own INBOX — they cannot all be called INBOX.
async fn folders_for(pool: &PgPool, user_id: i64) -> Result<Vec<(i64, String)>> {
    let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
        "SELECT f.id, a.label, f.name, a.hierarchy_delimiter
         FROM folders f JOIN accounts a ON a.id = f.account_id
         WHERE a.user_id = $1
         ORDER BY a.label, f.name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, label, name, delim)| {
            let shown = naming::to_display(&name, delim.and_then(|d| d.chars().next()));
            (id, format!("{label}/{shown}"))
        })
        .collect())
}

pub struct Row {
    pub uid: i64,
    pub blake3: String,
    pub size: i64,
    pub internaldate: chrono::DateTime<chrono::Utc>,
    pub seen: bool,
    /// Cached header block, when we have one. Absent means fall back to S3.
    pub headers: Option<Vec<u8>>,
}

async fn folder_messages(pool: &PgPool, folder_id: i64) -> Result<Vec<Row>> {
    let rows: Vec<(
        i64,
        String,
        i64,
        chrono::DateTime<chrono::Utc>,
        bool,
        Option<Vec<u8>>,
    )> = sqlx::query_as(
        "SELECT p.uid, m.blake3, m.size, m.internaldate, p.seen, m.headers
         FROM placements p JOIN messages m ON m.id = p.message_id
         WHERE p.folder_id = $1
         ORDER BY p.uid",
    )
    .bind(folder_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Row {
            uid: r.0,
            blake3: r.1,
            size: r.2,
            internaldate: r.3,
            seen: r.4,
            headers: r.5,
        })
        .collect())
}

fn item_names(
    m: &imap_next::imap_types::fetch::MacroOrMessageDataItemNames<'static>,
) -> Vec<MessageDataItemName<'static>> {
    use imap_next::imap_types::fetch::MacroOrMessageDataItemNames as M;
    use imap_next::imap_types::IntoStatic;
    match m {
        M::Macro(mac) => mac.expand().into_iter().map(|n| n.into_static()).collect(),
        M::MessageDataItemNames(names) => names.clone(),
    }
}

fn in_set(set: &imap_next::imap_types::sequence::SequenceSet, value: u32, max: u32) -> bool {
    set.iter(std::num::NonZeroU32::new(max.max(1)).unwrap())
        .any(|v| v.get() == value)
}

fn nstring(bytes: &[u8]) -> NString<'static> {
    NString(Some(IString::Literal(
        Literal::try_from(bytes.to_vec()).unwrap(),
    )))
}

fn build_item(
    name: &MessageDataItemName<'static>,
    row: &Row,
    raw: Option<&[u8]>,
) -> Option<MessageDataItem<'static>> {
    Some(match name {
        MessageDataItemName::Uid => {
            MessageDataItem::Uid(std::num::NonZeroU32::new(row.uid as u32)?)
        }
        MessageDataItemName::Flags => MessageDataItem::Flags(if row.seen {
            vec![FlagFetch::Flag(imap_next::imap_types::flag::Flag::Seen)]
        } else {
            vec![]
        }),
        MessageDataItemName::Rfc822Size => MessageDataItem::Rfc822Size(row.size as u32),
        MessageDataItemName::InternalDate => {
            MessageDataItem::InternalDate(DateTime::try_from(row.internaldate.fixed_offset()).ok()?)
        }
        MessageDataItemName::Rfc822 => MessageDataItem::Rfc822(nstring(raw?)),
        MessageDataItemName::Rfc822Header => {
            MessageDataItem::Rfc822Header(nstring(fetchlib::split_header_body(raw?).0))
        }
        MessageDataItemName::Rfc822Text => {
            MessageDataItem::Rfc822Text(nstring(fetchlib::split_header_body(raw?).1))
        }
        MessageDataItemName::BodyExt {
            section, partial, ..
        } => {
            let raw = raw?;
            let selected: Vec<u8> = match section {
                None => raw.to_vec(),
                Some(Section::Header(_)) => fetchlib::split_header_body(raw).0.to_vec(),
                Some(Section::Text(_)) => fetchlib::split_header_body(raw).1.to_vec(),
                Some(Section::HeaderFields(_, fields)) => {
                    let wanted: Vec<String> = fields
                        .as_ref()
                        .iter()
                        .map(|f| String::from_utf8_lossy(f.as_ref()).to_string())
                        .collect();
                    fetchlib::header_fields(raw, &wanted)
                }
                // MIME part addressing needs a generated body structure; not
                // implemented, so say nothing rather than return wrong bytes.
                Some(_) => return None,
            };
            let sliced = match partial {
                Some((offset, len)) => fetchlib::partial(&selected, *offset, len.get()).to_vec(),
                None => selected,
            };
            MessageDataItem::BodyExt {
                section: section.clone(),
                origin: partial.map(|(o, _)| o).filter(|o| *o > 0),
                data: nstring(&sliced),
            }
        }
        // ENVELOPE and BODYSTRUCTURE need generated structures; skipped for now
        // so a client that asks gets a response without them rather than a lie.
        _ => return None,
    })
}

/// (folder id, message count, our uidvalidity, our uidnext)
async fn folder_meta(
    pool: &PgPool,
    user_id: i64,
    display_name: &str,
) -> Result<Option<(i64, i64, i64, i64)>> {
    // Translate back to the source name before looking up. The stored name is
    // whatever the source called it; only the presentation uses our delimiter.
    let (label, rest) = match display_name.split_once('/') {
        Some(parts) => parts,
        None => return Ok(None),
    };

    let delim: Option<String> = sqlx::query_scalar(
        "SELECT hierarchy_delimiter FROM accounts WHERE user_id = $1 AND label = $2",
    )
    .bind(user_id)
    .bind(label)
    .fetch_optional(pool)
    .await?
    .flatten();
    let source_name = naming::from_display(rest, delim.and_then(|d| d.chars().next()));

    let row: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT f.id, f.uidvalidity, f.uidnext
         FROM folders f JOIN accounts a ON a.id = f.account_id
         WHERE a.user_id = $1 AND a.label = $2 AND f.name = $3",
    )
    .bind(user_id)
    .bind(label)
    .bind(&source_name)
    .fetch_optional(pool)
    .await?;

    let Some((id, uidvalidity, uidnext)) = row else {
        return Ok(None);
    };
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM placements WHERE folder_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(Some((id, count, uidvalidity, uidnext)))
}

fn mailbox_text(m: &Mailbox<'_>) -> String {
    match m {
        Mailbox::Inbox => "INBOX".to_string(),
        Mailbox::Other(o) => String::from_utf8_lossy(o.as_ref()).to_string(),
    }
}

fn wildcard_text(m: &ListMailbox<'_>) -> String {
    match m {
        ListMailbox::Token(t) => String::from_utf8_lossy(t.as_ref()).to_string(),
        ListMailbox::String(s) => String::from_utf8_lossy(s.as_ref()).to_string(),
    }
}

/// Minimal IMAP wildcard match: `*` spans hierarchy, `%` does not.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    if pattern == "*" || pattern.is_empty() {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    pattern == name
}

fn ok(server: &mut Server, tag: Tag<'static>, text: &str) {
    server.enqueue_status(Status::ok(Some(tag), None::<Code>, text.to_string()).unwrap());
}

fn no(server: &mut Server, tag: Tag<'static>, text: &str) {
    server.enqueue_status(Status::no(Some(tag), None::<Code>, text.to_string()).unwrap());
}
