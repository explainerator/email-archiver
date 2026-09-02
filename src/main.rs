//! email-archiver — read-only email archive.
//!
//! Storage in S3 (one bucket per user), index in Postgres, IMAPS to clients.
//! See ARCHIVE-PLAN.md for the design and phasing.

mod check;
mod config;
mod db;
mod diagnose;
mod envelope;
mod fetch;
mod ingest;
mod naming;
mod secrets;
mod server;
mod store;
mod tls;

use anyhow::{Context, Result};
use config::Config;
use sqlx::postgres::PgPoolOptions;

/// The archive has its own database on a cluster shared with the game services
/// (`defaultdb`) and `backroom`. Writing into the wrong one would be difficult
/// to notice and unpleasant to unpick, so it is asserted before migrating.
const EXPECTED_DATABASE: &str = "archive";

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

    email-archiver set-password <login> <password>
        Set an archive user's IMAP password (Argon2id). Until this is set the
        account cannot be logged into at all.

    email-archiver set-source <address> <host> <username> <password> [--insecure-tls]
        Store source mailbox credentials, encrypted. --insecure-tls accepts any
        certificate for THIS source only.

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
            let password = arg(&args, 2, "password")?;
            let pool = connect_db(&config).await?;
            db::set_user_password(&pool, &login, &password).await?;
            println!("password set for {login}");
            Ok(())
        }

        Some("set-source") => {
            let address = arg(&args, 1, "address")?;
            let host = arg(&args, 2, "host")?;
            let username = arg(&args, 3, "username")?;
            let password = arg(&args, 4, "password")?;
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

fn arg(args: &[String], index: usize, name: &str) -> Result<String> {
    args.get(index)
        .cloned()
        .with_context(|| format!("missing argument <{name}>\n\n{USAGE}"))
}

/// Connect, verify this is the archive database, and bring the schema up to date.
///
/// Migrations run here rather than in a separate command, so the schema can
/// never lag the binary. A manual step is a step that gets skipped — or worse,
/// run with a stale binary, which silently applies only the migrations that
/// existed when *it* was built and reports success.
///
/// sqlx takes an advisory lock while migrating, so concurrent starts are safe.
///
/// The database guard lives here too: the cluster also hosts the game services'
/// `defaultdb`, and every command goes through this function so neither check
/// can be forgotten by a new subcommand.
async fn connect_db(config: &Config) -> Result<sqlx::PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(config.database.max_connections)
        .connect(&config.database.url)
        .await
        .context("connecting to Postgres (is this host on the database IP allowlist?)")?;

    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await?;
    anyhow::ensure!(
        database == EXPECTED_DATABASE,
        "connected to database {database:?}, expected {EXPECTED_DATABASE:?}. \
         Refusing to continue — this cluster also hosts the game services' `defaultdb`."
    );

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("applying migrations")?;

    Ok(pool)
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
