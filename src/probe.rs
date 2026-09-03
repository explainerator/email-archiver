//! Show the raw IMAP conversation with a source server.
//!
//! Exists because ingest hung with no output at all: TCP and TLS were up, the
//! OAuth token had been minted, and then nothing. `async-imap` gives back one
//! error at the end of a handshake and says nothing about what happened during
//! it, which is exactly the wrong shape of information when the handshake is
//! what stalled.
//!
//! So this speaks IMAP by hand and prints both sides. It mirrors what
//! `async-imap` does rather than doing whatever is easiest -- `AUTHENTICATE
//! XOAUTH2` with no initial response, then the credential in reply to the
//! server's continuation -- so that a stall reproduces here rather than being
//! diagnosed away by a different implementation.
//!
//! Every read has a deadline. A diagnostic that can itself hang is no use for
//! diagnosing a hang.

use anyhow::{Context, Result};
use base64::Engine as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::config::{Auth, Source};

/// How long to wait for any single line before giving up on it.
const READ_TIMEOUT: Duration = Duration::from_secs(20);

pub async fn run(config: &crate::config::Config, pool: &sqlx::PgPool, address: &str) -> Result<()> {
    let source = crate::ingest::source_for_address(config, pool, address).await?;

    println!("probing {address}");
    println!("  host      {}:{}", source.host, source.port);
    println!("  user      {}", source.username);
    println!(
        "  auth      {}",
        match &source.auth {
            Auth::Password(_) => "password".to_string(),
            Auth::XOAuth2 { token } => format!("XOAUTH2 (token {} chars)", token.len()),
        }
    );
    println!(
        "  certs     {}",
        if source.allow_invalid_certs {
            "NOT verified"
        } else {
            "verified"
        }
    );
    println!();

    let stream = connect(&source).await?;
    let mut stream = BufReader::new(stream);

    // Greeting. A server that accepts the connection and then says nothing is
    // already a finding, which is why this is timed separately.
    let greeting = read_line(&mut stream, "greeting").await?;
    println!("S: {}", greeting.trim_end());

    match &source.auth {
        Auth::Password(password) => {
            // The password is not printed, obviously; the point is the server's
            // reply, not our own credential.
            println!("C: a1 LOGIN {} <password>", source.username);
            let line = format!("a1 LOGIN {} {}\r\n", source.username, password);
            stream.get_mut().write_all(line.as_bytes()).await?;
        }
        Auth::XOAuth2 { token } => {
            println!("C: a1 AUTHENTICATE XOAUTH2");
            stream
                .get_mut()
                .write_all(b"a1 AUTHENTICATE XOAUTH2\r\n")
                .await?;

            // The server must now send a continuation. If this is where ingest
            // stalls, the deadline below is what proves it.
            let challenge = read_line(&mut stream, "continuation after AUTHENTICATE").await?;
            print_server(&challenge);

            anyhow::ensure!(
                challenge.starts_with('+'),
                "expected a continuation (+) before sending the credential, got: {}",
                challenge.trim_end()
            );

            let credential = format!("user={}\x01auth=Bearer {}\x01\x01", source.username, token);
            let encoded = base64::engine::general_purpose::STANDARD.encode(credential);
            println!("C: <credential, {} chars base64>", encoded.len());
            stream.get_mut().write_all(encoded.as_bytes()).await?;
            stream.get_mut().write_all(b"\r\n").await?;
        }
    }

    // Read until the tagged reply, printing everything. Gmail answers a bad
    // credential with another continuation carrying base64 JSON rather than a
    // refusal, and that is decoded below because it names the actual problem.
    let mut authenticated = false;
    for _ in 0..10 {
        let line = read_line(&mut stream, "authentication reply").await?;
        print_server(&line);

        if line.starts_with('+') {
            // An error challenge. The exchange must be closed with an empty
            // response or the server keeps waiting -- which is one candidate
            // for the original hang.
            println!("C: <empty line to end the exchange>");
            stream.get_mut().write_all(b"\r\n").await?;
            continue;
        }
        if line.starts_with("a1 OK") {
            authenticated = true;
            break;
        }
        if line.starts_with("a1 NO") || line.starts_with("a1 BAD") {
            anyhow::bail!("server rejected authentication: {}", line.trim_end());
        }
    }

    anyhow::ensure!(authenticated, "no tagged reply to the authentication");
    println!("\n  authenticated");

    // One cheap command, to prove the session is actually usable.
    println!("C: a2 LIST \"\" \"*\"");
    stream
        .get_mut()
        .write_all(b"a2 LIST \"\" \"*\"\r\n")
        .await?;

    let mut folders = 0;
    for _ in 0..500 {
        let line = read_line(&mut stream, "LIST reply").await?;
        if line.starts_with("a2 ") {
            print_server(&line);
            break;
        }
        folders += 1;
        if folders <= 5 {
            print_server(&line);
        }
    }
    println!("  {folders} mailboxes listed");

    let _ = stream.get_mut().write_all(b"a3 LOGOUT\r\n").await;
    println!("\nsource is reachable and usable");
    Ok(())
}

fn print_server(line: &str) {
    let trimmed = line.trim_end();
    println!("S: {trimmed}");

    // Gmail puts the real reason in a base64 JSON blob on the continuation,
    // which is unreadable at a glance and is usually the whole answer.
    if let Some(payload) = trimmed.strip_prefix("+ ") {
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(payload.trim()) {
            if let Ok(text) = String::from_utf8(decoded) {
                if !text.is_empty() {
                    println!("   decoded: {text}");
                }
            }
        }
    }
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    stream: &mut R,
    what: &str,
) -> Result<String> {
    let mut line = String::new();
    let read = tokio::time::timeout(READ_TIMEOUT, stream.read_line(&mut line))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out after {}s waiting for the {what}. The connection is open and the \
                 server is silent -- that is the hang, reproduced.",
                READ_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("reading the {what}"))?;

    anyhow::ensure!(
        read > 0,
        "server closed the connection while sending the {what}"
    );
    Ok(line)
}

async fn connect(source: &Source) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = tokio::time::timeout(
        Duration::from_secs(15),
        TcpStream::connect((source.host.as_str(), source.port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out connecting to {}:{}", source.host, source.port))?
    .with_context(|| format!("connecting to {}:{}", source.host, source.port))?;

    let tls_config = crate::ingest::tls_config_for(source)?;
    let domain = tokio_rustls::rustls::pki_types::ServerName::try_from(source.host.clone())
        .with_context(|| format!("{} is not a valid DNS name", source.host))?;

    let stream = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_rustls::TlsConnector::from(Arc::new(tls_config)).connect(domain, tcp),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out during the TLS handshake with {}", source.host))?
    .context("TLS handshake failed")?;

    Ok(stream)
}
