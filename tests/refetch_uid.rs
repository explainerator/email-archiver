//! Ask the source server what a specific UID actually contains.
//!
//! Four messages archived as zero bytes. Either the server really returns
//! nothing for them -- in which case they are junk placements -- or the fetch
//! path dropped a body we should have kept, in which case real mail is missing
//! and an empty stand-in is making it look archived.
//!
//! The only way to tell is to ask the server again, through the SAME resolution
//! and connection path ingest uses, so that a difference here is a difference
//! in the message rather than in how it was requested.
//!
//!     cargo test --test refetch_uid -- --nocapture --ignored

use email_archiver::{config::Config, ingest};
use futures::StreamExt;

/// address, folder, uid -- the placements the diagnostic turned up.
const TARGETS: [(&str, &str, &str); 1] = [("kduck@twoducks.ca", "Sent.2009", "691")];

#[tokio::test]
#[ignore = "diagnostic; run with --ignored"]
async fn refetch_uid() {
    // main.rs does this at startup; a test binary has its own process and so
    // has to do it too, or the first TLS handshake panics.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let Ok(config) = Config::load() else {
        eprintln!("no EMAIL_ARCHIVER_CONFIG; skipping");
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&config.database.url)
        .await
        .expect("database");

    for (address, folder, uid) in TARGETS {
        println!("\n=== {address} {folder} uid {uid} ===");

        let source = match ingest::source_for_address(&config, &pool, address).await {
            Ok(s) => s,
            Err(e) => {
                println!("  cannot resolve source: {e:#}");
                continue;
            }
        };
        let mut session = match ingest::connect_session(&source).await {
            Ok(s) => s,
            Err(e) => {
                println!("  cannot connect: {e:#}");
                continue;
            }
        };

        let mailbox = session.examine(folder).await.expect("examine");
        println!("  folder holds {} messages", mailbox.exists);

        // Exactly what ingest asks for, so a difference is in the message and
        // not in the request.
        // The stream borrows the session, so it is scoped and dropped before
        // anything else touches it.
        {
            let mut stream = session
                .uid_fetch(uid, "(UID FLAGS RFC822.SIZE BODY.PEEK[])")
                .await
                .expect("fetch");

            let mut saw_any = false;
            while let Some(item) = stream.next().await {
                let item = item.expect("fetch item");
                saw_any = true;
                let body = item.body();
                println!(
                    "  uid {:?}  server size {:?}  body {} bytes",
                    item.uid,
                    item.size,
                    body.map(|b| b.len()).unwrap_or(0)
                );
                match body {
                    None => println!("  NO BODY SECTION in the response"),
                    Some(b) if b.is_empty() => println!("  body present but EMPTY"),
                    Some(b) => {
                        let head = String::from_utf8_lossy(&b[..b.len().min(400)]);
                        println!("  --- first 400 bytes ---");
                        for line in head.lines().take(12) {
                            println!("  | {line}");
                        }
                    }
                }
            }
            if !saw_any {
                println!("  server returned NOTHING for this uid -- it no longer exists");
            }
        }

        let _ = session.logout().await;
    }
}
