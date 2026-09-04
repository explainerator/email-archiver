//! Compare a message that archived as empty against its neighbours.
//!
//! Four messages out of ~170,000 archived as zero bytes, scattered across two
//! accounts and three folders. That distribution argues against a batching or
//! concurrency fault -- those cluster -- and for something about these
//! particular messages.
//!
//! So this speaks IMAP by hand and prints what the server actually puts on the
//! wire, alongside the same request for the UIDs either side. The neighbours
//! archived correctly, so any difference in how the middle one comes back is
//! the signal.
//!
//! Raw rather than through async-imap deliberately: if the bytes are on the
//! wire and `item.body()` is still empty, the fault is in the parsing, and a
//! diagnostic built on the same parser could not tell us that.
//!
//!     cargo test --test raw_fetch -- --nocapture --ignored

use email_archiver::{config::Config, ingest};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// address, folder, the suspect UID, and its neighbours as controls.
const TARGET: (&str, &str, u32) = ("jaqui@thebackroom420.ca", "INBOX", 231);

#[tokio::test]
#[ignore = "diagnostic; run with --ignored"]
async fn raw_fetch() {
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

    let (address, folder, uid) = TARGET;
    let source = ingest::source_for_address(&config, &pool, address)
        .await
        .expect("source");

    let tcp = tokio::net::TcpStream::connect((source.host.as_str(), source.port))
        .await
        .expect("tcp");
    let tls_config = ingest::tls_config_for(&source).expect("tls config");
    let domain = tokio_rustls::rustls::pki_types::ServerName::try_from(source.host.clone())
        .expect("server name");
    let tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls_config))
        .connect(domain, tcp)
        .await
        .expect("tls");
    let mut stream = BufReader::new(tls);

    line(&mut stream, "greeting").await;

    // Authenticate exactly as ingest does.
    match &source.auth {
        email_archiver::config::Auth::Password(password) => {
            send(
                &mut stream,
                &format!("a1 LOGIN {} {password}", source.username),
            )
            .await;
        }
        email_archiver::config::Auth::XOAuth2 { token } => {
            send(&mut stream, "a1 AUTHENTICATE XOAUTH2").await;
            let challenge = line(&mut stream, "continuation").await;
            assert!(challenge.starts_with('+'), "expected +, got {challenge:?}");
            let credential = format!("user={}\x01auth=Bearer {token}\x01\x01", source.username);
            use base64::Engine as _;
            send(
                &mut stream,
                &base64::engine::general_purpose::STANDARD.encode(credential),
            )
            .await;
        }
    }
    loop {
        let l = line(&mut stream, "auth reply").await;
        if l.starts_with("a1 OK") {
            break;
        }
        assert!(
            !l.starts_with("a1 NO") && !l.starts_with("a1 BAD"),
            "auth failed: {l}"
        );
    }

    send(&mut stream, &format!("a2 EXAMINE {folder}")).await;
    loop {
        let l = line(&mut stream, "examine").await;
        if l.starts_with("a2 ") {
            break;
        }
    }

    // The suspect and its neighbours, one request each so the responses cannot
    // be confused with one another.
    for probe in [uid - 1, uid, uid + 1] {
        println!("\n=== uid {probe} ===");
        send(
            &mut stream,
            &format!("a{probe} UID FETCH {probe} (UID RFC822.SIZE BODY.PEEK[])"),
        )
        .await;

        let mut literal_total = 0usize;
        loop {
            let l = line(&mut stream, "fetch").await;
            let trimmed = l.trim_end();

            if trimmed.starts_with(&format!("a{probe} ")) {
                println!("  tagged: {trimmed}");
                break;
            }

            // A literal is announced as {N} at the end of the line; the next N
            // bytes are payload rather than lines, and must be consumed as
            // such or everything after them is read as protocol.
            if let Some(open) = trimmed.rfind('{') {
                if trimmed.ends_with('}') {
                    if let Ok(n) = trimmed[open + 1..trimmed.len() - 1].parse::<usize>() {
                        println!("  header line: {trimmed}");
                        println!("  literal announced: {n} bytes");
                        let mut buf = vec![0u8; n];
                        stream.read_exact(&mut buf).await.expect("literal");
                        literal_total += n;
                        let head = String::from_utf8_lossy(&buf[..buf.len().min(120)]);
                        println!("  first line of payload: {:?}", head.lines().next());
                        continue;
                    }
                }
            }
            println!("  line: {trimmed}");
        }
        println!("  literal bytes received: {literal_total}");
    }

    let _ = stream.get_mut().write_all(b"a9 LOGOUT\r\n").await;
}

async fn send<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut BufReader<S>,
    text: &str,
) {
    stream
        .get_mut()
        .write_all(format!("{text}\r\n").as_bytes())
        .await
        .expect("write");
    stream.get_mut().flush().await.expect("flush");
}

async fn line<S: tokio::io::AsyncRead + Unpin>(stream: &mut BufReader<S>, what: &str) -> String {
    let mut out = String::new();
    tokio::time::timeout(READ_TIMEOUT, stream.read_line(&mut out))
        .await
        .unwrap_or_else(|_| panic!("timed out reading {what}"))
        .unwrap_or_else(|e| panic!("reading {what}: {e}"));
    out
}
