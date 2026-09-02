//! Read-only IMAP server — Phase 4 spike.
//!
//! Deliberately minimal: enough for Thunderbird to log in, list folders, select
//! one, and fetch a message. The point is to find out whether `imap-next` works
//! as a server foundation before building on it, and to learn what Thunderbird
//! actually asks for — every unhandled command is logged rather than guessed at.
//!
//! **Binds to loopback and accepts any password.** This is a spike, not a
//! service. Real authentication against `users.password_hash` and TLS come
//! before it is reachable from anywhere else.
//!
//! Nothing here can mutate the archive: there is no APPEND, STORE, EXPUNGE or
//! DELETE, and the only writes in the whole program are ingest and read-state.

use anyhow::{Context, Result};
use imap_next::imap_types::core::{Tag, Vec1};
use imap_next::imap_types::flag::{Flag, FlagPerm};
use imap_next::imap_types::mailbox::{ListMailbox, Mailbox};
use imap_next::imap_types::response::{Capability, Code, Data, Greeting, Status};
use imap_next::server::{Options, Server};
use imap_next::stream::Stream;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

use crate::config::Config;

/// Everything the connection knows once a user has logged in.
struct Session {
    user_id: i64,
    bucket: String,
    /// Currently selected folder, if any: (folder id, name).
    selected: Option<(i64, String)>,
}

pub async fn run(config: &Arc<Config>, pool: &PgPool, bind: &str) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;

    println!("IMAP spike listening on {bind}");
    println!("  NOTE: plaintext, loopback only, and ANY password is accepted.");
    println!("  Point Thunderbird at this address with connection security = None.");

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
        tokio::spawn(async move {
            if let Err(e) = serve(&config, &pool, socket).await {
                eprintln!("connection error: {e:#}");
            }
            println!("--- disconnected ---");
        });
    }
}

async fn serve(config: &Config, pool: &PgPool, socket: TcpStream) -> Result<()> {
    let mut stream = Stream::insecure(socket);
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

        CommandBody::Login { username, .. } => {
            let login = String::from_utf8_lossy(username.as_ref()).to_string();
            match lookup_user(pool, &login).await? {
                Some((user_id, bucket)) => {
                    println!("  authenticated {login} (password NOT checked — spike)");
                    *session = Some(Session {
                        user_id,
                        bucket,
                        selected: None,
                    });
                    ok(server, tag, "LOGIN done");
                }
                None => no(server, tag, "no such archive user"),
            }
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
            names.extend(folders_for(pool, sess.user_id).await?.into_iter().map(|(_, n)| n));

            for name in names {
                if !matches_pattern(&pattern, &name) {
                    continue;
                }
                server.enqueue_data(Data::List {
                    // No attributes claimed. \Noinferiors would assert these
                    // folders cannot have children, which stops being true the
                    // moment source hierarchy is translated into ours.
                    items: vec![],
                    delimiter: Some(imap_next::imap_types::core::QuotedChar::try_from('/').unwrap()),
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
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT f.id, a.label, f.name
         FROM folders f JOIN accounts a ON a.id = f.account_id
         WHERE a.user_id = $1
         ORDER BY a.label, f.name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, label, name)| (id, format!("{label}/{name}")))
        .collect())
}

/// (folder id, message count, our uidvalidity, our uidnext)
async fn folder_meta(
    pool: &PgPool,
    user_id: i64,
    display_name: &str,
) -> Result<Option<(i64, i64, i64, i64)>> {
    let row: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT f.id, f.uidvalidity, f.uidnext
         FROM folders f JOIN accounts a ON a.id = f.account_id
         WHERE a.user_id = $1 AND (a.label || '/' || f.name) = $2",
    )
    .bind(user_id)
    .bind(display_name)
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

async fn lookup_user(pool: &PgPool, login: &str) -> Result<Option<(i64, String)>> {
    Ok(
        sqlx::query_as("SELECT id, bucket FROM users WHERE login = $1")
            .bind(login)
            .fetch_optional(pool)
            .await?,
    )
}

fn ok(server: &mut Server, tag: Tag<'static>, text: &str) {
    server.enqueue_status(Status::ok(Some(tag), None::<Code>, text.to_string()).unwrap());
}

fn no(server: &mut Server, tag: Tag<'static>, text: &str) {
    server.enqueue_status(Status::no(Some(tag), None::<Code>, text.to_string()).unwrap());
}
