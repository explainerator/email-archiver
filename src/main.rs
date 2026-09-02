//! email-archiver — read-only email archive.
//!
//! Storage in S3 (one bucket per user), index in Postgres, IMAPS to clients.
//! See ARCHIVE-PLAN.md for the design and phasing.

use anyhow::{Context, Result};
use email_archiver::config::Config;
use email_archiver::{check, db, diagnose, ingest, secrets, server, web};
use email_archiver::{connect_db, EXPECTED_DATABASE};

const USAGE: &str = "\
email-archiver

USAGE:
    email-archiver migrate
        Show the current schema. Migrations run automatically on every
        command, so this is a report rather than a required step.

    email-archiver add-user <login> <bucket> [display name]
        Register an archive user and the S3 bucket they own.

    email-archiver add-account <login> <address> <label> <imap|gmail>
        Register a source mailbox feeding that user's archive. <label> becomes
        the IMAP namespace prefix, e.g. work -> work/INBOX.

    email-archiver generate-key
        Print a fresh encryption key for config.toml. Changing this key makes
        every stored source password unreadable, so generate it once.

    email-archiver set-password <login>
        Set an archive user's password (Argon2id), used by both IMAP and the web
        client. Prompts twice, without echoing. Until this is set the account
        cannot be logged into at all.

        Passing the password as an argument still works but is discouraged: the
        shell rewrites !, $, backticks and quotes first, so the stored hash may
        cover a string you cannot retype.

    email-archiver set-source <address> <host> <username> [--insecure-tls]
        Store source mailbox credentials, encrypted. Prompts for the password.
        --insecure-tls accepts any certificate for THIS source only.

    email-archiver check <login> [--deep]
        Verify Postgres and S3 agree for one user. Samples 5 blobs by default;
        --deep reads and hash-checks every message body.

    email-archiver rebuild-manifests <login>
        Regenerate all S3 manifests for a user from the database. Use when
        manifests are missing, or were written under an older key scheme.

    email-archiver serve [bind] [--allow-plaintext]
        Read-only IMAP server. Default 127.0.0.1:1143.
        Serves TLS when tls.cert_path and tls.key_path are set. Loopback may be
        plaintext; any other address needs TLS, or --allow-plaintext to
        override for local testing. IMAP LOGIN sends the password in the clear,
        so that override puts real passwords on the network.

    email-archiver serve-web [bind] [--allow-plaintext] [--assets <dir>]
        Read-only web client and HTTP API. Default 127.0.0.1:8000, plaintext.
        Loopback is always plaintext, whatever tls.* says -- a certificate for
        the public hostname cannot validate for 127.0.0.1, so TLS there could
        not work anyway. Any other address needs TLS (phase 7) or
        --allow-plaintext. --assets serves the built frontend; without it only
        the API responds.

    email-archiver backfill-headers <login>
        Cache header blocks for messages archived before that column existed.
        Purely an optimisation; safe to interrupt and resume.

    email-archiver diagnose <address> <folder>
        Explain a folder's completeness gap: which absent source UIDs are
        duplicates of content already archived, and which are genuinely missing.

    email-archiver ingest <address>
        Pull mail from one source mailbox. Resumable: re-running continues
        from where it stopped.

Configuration is read from $EMAIL_ARCHIVER_CONFIG, else /etc/email-archiver/config.toml.
Generate it with: cd terraform && terraform output -raw archiver_config > config.toml
";

#[tokio::main]
async fn main() -> Result<()> {
    // Both `ring` and `aws-lc-rs` end up in the dependency graph (sqlx and the
    // AWS SDK each pull one), so rustls refuses to guess which to use and
    // panics at the first TLS handshake. Choose explicitly, once, before any
    // connection is made. Failure here means a provider was already installed,
    // which is harmless.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Handled before the config is loaded. The config now REQUIRES an
    // encryption key, so needing a working config in order to generate that key
    // would deadlock first-time setup.
    if args.first().map(String::as_str) == Some("generate-key") {
        println!("{}", secrets::SecretKey::generate()?);
        eprintln!("\nAdd to config.toml as: encryption_key = \"...\"");
        eprintln!("Keep it. Losing it makes every stored source password unreadable.");
        return Ok(());
    }

    let config = Config::load()?;

    match args.first().map(String::as_str) {
        Some("migrate") => migrate(&config).await,

        Some("add-user") => {
            let (login, bucket) = (arg(&args, 1, "login")?, arg(&args, 2, "bucket")?);
            let display = args.get(3).cloned().unwrap_or_else(|| login.clone());
            let pool = connect_db(&config).await?;
            let id = db::create_user(&pool, &login, &bucket, &display).await?;
            println!("user {login} (id {id}) -> bucket {bucket}");
            Ok(())
        }

        Some("add-account") => {
            let login = arg(&args, 1, "login")?;
            let address = arg(&args, 2, "address")?;
            let label = arg(&args, 3, "label")?;
            let provider = arg(&args, 4, "provider (imap|gmail)")?;
            let pool = connect_db(&config).await?;
            let id = db::create_account(&pool, &login, &address, &label, &provider).await?;
            println!("account {address} (id {id}) -> user {login}, namespace {label}/");
            Ok(())
        }

        Some("set-password") => {
            let login = arg(&args, 1, "login")?;
            let password = match args.get(2) {
                Some(given) => {
                    warn_argv_password();
                    given.clone()
                }
                None => prompt_new_password()?,
            };
            let pool = connect_db(&config).await?;
            db::set_user_password(&pool, &login, &password).await?;
            println!("password set for {login}");
            Ok(())
        }

        Some("set-source") => {
            let address = arg(&args, 1, "address")?;
            let host = arg(&args, 2, "host")?;
            let username = arg(&args, 3, "username")?;
            let password = match args.get(4).filter(|a| !a.starts_with("--")) {
                Some(given) => {
                    warn_argv_password();
                    given.clone()
                }
                None => prompt_password("Source mailbox password: ")?,
            };
            let insecure = args.iter().any(|a| a == "--insecure-tls");
            let pool = connect_db(&config).await?;
            db::set_source(
                &pool,
                &config.key()?,
                &address,
                &host,
                993,
                &username,
                &password,
                insecure,
            )
            .await?;
            println!("source credentials stored for {address} (encrypted)");
            if insecure {
                eprintln!(
                    "  WARNING: certificate verification disabled for {host} — encrypted, \
                     but the server is NOT authenticated"
                );
            }
            Ok(())
        }

        Some("check") => {
            let login = arg(&args, 1, "login")?;
            let pool = connect_db(&config).await?;
            let deep = args.iter().any(|a| a == "--deep");
            check::run(&config, &pool, &login, deep).await
        }

        Some("rebuild-manifests") => {
            let login = arg(&args, 1, "login")?;
            let pool = connect_db(&config).await?;
            check::rebuild_manifests(&config, &pool, &login).await
        }

        Some("serve") => {
            let bind = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:1143".to_string());
            let allow_plaintext = args.iter().any(|a| a == "--allow-plaintext");
            let pool = connect_db(&config).await?;
            server::run(&std::sync::Arc::new(config), &pool, &bind, allow_plaintext).await
        }

        Some("serve-web") => {
            let bind = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:8000".to_string());
            let allow_plaintext = args.iter().any(|a| a == "--allow-plaintext");
            let assets = args
                .iter()
                .position(|a| a == "--assets")
                .and_then(|i| args.get(i + 1))
                .map(std::path::PathBuf::from);
            let pool = connect_db(&config).await?;
            web::run(
                &std::sync::Arc::new(config),
                &pool,
                &bind,
                allow_plaintext,
                assets,
            )
            .await
        }

        Some("backfill-headers") => {
            let login = arg(&args, 1, "login")?;
            let pool = connect_db(&config).await?;
            check::backfill_headers(&config, &pool, &login).await
        }

        Some("diagnose") => {
            let address = arg(&args, 1, "address")?;
            let folder = arg(&args, 2, "folder")?;
            let pool = connect_db(&config).await?;
            diagnose::run(&config, &pool, &address, &folder).await
        }

        Some("ingest") => {
            let address = arg(&args, 1, "address")?;
            let pool = connect_db(&config).await?;
            ingest::run(&config, &pool, &address).await
        }

        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

/// Read a password without echoing it.
///
/// **Never take a password from argv.** The shell rewrites `!`, `$`, backticks,
/// quotes and backslashes before this program is even started, so a password
/// containing any of them is stored as a hash of something the user cannot
/// retype -- which presents as "the password stopped working" with no way to
/// tell it from a forgotten one. It also lands in shell history and in the
/// process list, where other users can read it.
///
/// stdin passes through untouched, so this is both safer and more correct.
fn prompt_password(prompt: &str) -> Result<String> {
    use std::io::IsTerminal;

    // rpassword talks to the CONSOLE, not to stdin, so a redirect or a pipe
    // never reaches it and the process waits for a keystroke that will never
    // come. A deploy script calling this would hang indefinitely rather than
    // fail, so piped input is handled explicitly instead.
    let password = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(prompt).context("reading password")?
    } else {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading password from stdin")?;
        // Only the line ending is stripped. Trailing spaces can be part of a
        // password, and silently trimming them would store a hash of something
        // other than what was supplied.
        line.trim_end_matches(['\r', '\n']).to_string()
    };

    anyhow::ensure!(!password.is_empty(), "password was empty");
    Ok(password)
}

/// Prompt twice, so a typo becomes a retry rather than a lockout.
///
/// Worth the second prompt precisely because nothing echoes: a mistyped
/// password is otherwise stored happily and only discovered at the next login,
/// long after the typo is recoverable from memory.
fn prompt_new_password() -> Result<String> {
    use std::io::IsTerminal;

    let first = prompt_password("New password: ")?;
    // Piped input gets one line, not two: asking a script to repeat itself
    // would just make it fail.
    if !std::io::stdin().is_terminal() {
        return Ok(first);
    }
    let again = prompt_password("Repeat password: ")?;
    anyhow::ensure!(
        first == again,
        "passwords did not match; nothing was changed"
    );
    Ok(first)
}

fn warn_argv_password() {
    eprintln!(
        "WARNING: password given on the command line. Your shell may have
         altered it (!, $, backticks, quotes), it is now in your shell history,
         and it was visible in the process list. Omit it to be prompted instead."
    );
}

fn arg(args: &[String], index: usize, name: &str) -> Result<String> {
    args.get(index)
        .cloned()
        .with_context(|| format!("missing argument <{name}>\n\n{USAGE}"))
}

/// Report the schema. Migrations have already run in `connect_db`; this exists
/// to show what is there, not to be the thing that applies them.
async fn migrate(config: &Config) -> Result<()> {
    println!("loaded {config:?}");
    let pool = connect_db(config).await?;
    let database = EXPECTED_DATABASE;

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    .fetch_all(&pool)
    .await?;

    println!("migrations applied to database {database:?}. tables:");
    for t in &tables {
        println!("  {t}");
    }

    pool.close().await;
    Ok(())
}
