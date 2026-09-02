//! email-archiver — read-only email archive.
//!
//! Storage in S3 (one bucket per user), index in Postgres, IMAPS to clients.
//! See ARCHIVE-PLAN.md for the design and phasing.

mod check;
mod config;
mod db;
mod envelope;
mod ingest;
mod server;
mod store;

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
        Apply database migrations to the `archive` database.

    email-archiver add-user <login> <bucket> [display name]
        Register an archive user and the S3 bucket they own.

    email-archiver add-account <login> <address> <label> <imap|gmail>
        Register a source mailbox feeding that user's archive. <label> becomes
        the IMAP namespace prefix, e.g. work -> work/INBOX.

    email-archiver check <login> [--deep]
        Verify Postgres and S3 agree for one user. Samples 5 blobs by default;
        --deep reads and hash-checks every message body.

    email-archiver rebuild-manifests <login>
        Regenerate all S3 manifests for a user from the database. Use when
        manifests are missing, or were written under an older key scheme.

    email-archiver serve [bind]
        Read-only IMAP server (Phase 4 spike). Default 127.0.0.1:1143.
        Plaintext, loopback only, ANY password accepted — not a service yet.

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
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:1143".to_string());
            let pool = connect_db(&config).await?;
            server::run(&std::sync::Arc::new(config), &pool, &bind).await
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

/// Connect, and refuse to proceed unless this really is the archive database.
///
/// The cluster also hosts the game services' `defaultdb`. Every command goes
/// through here so that guard cannot be forgotten by a new subcommand.
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
    Ok(pool)
}

async fn migrate(config: &Config) -> Result<()> {
    println!("loaded {config:?}");
    let pool = connect_db(config).await?;
    let database = EXPECTED_DATABASE;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("applying migrations")?;

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
