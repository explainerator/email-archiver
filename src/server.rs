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

        other => {
            println!("  !! not implemented: {other:?}");
            no(server, tag, "not implemented in this spike");
        }
    }

    let _ = (config, session);
    Ok(false)
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
