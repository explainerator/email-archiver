//! email-archiver — read-only email archive.
//!
//! Storage in S3 (one bucket per user), index in Postgres, IMAPS to clients.
//! See ARCHIVE-PLAN.md for the design and phasing.

mod config;
mod store;

use anyhow::{Context, Result};
use config::Config;
use sqlx::postgres::PgPoolOptions;

/// The archive has its own database on a cluster shared with the game services
/// (`defaultdb`) and `backroom`. Writing into the wrong one would be difficult
/// to notice and unpleasant to unpick, so it is asserted before migrating.
const EXPECTED_DATABASE: &str = "archive";

/// Capped deliberately low. The Postgres cluster is shared with the game
/// services and we connect as the same `gern` role, so a bulk ingest must not
/// be able to exhaust the instance's connection budget. See ARCHIVE-PLAN.md R4.
const MAX_CONNECTIONS: u32 = 3;

const USAGE: &str = "\
email-archiver

USAGE:
    email-archiver migrate
        Apply database migrations to the `archive` database.

Configuration is read from $EMAIL_ARCHIVER_CONFIG, else /etc/email-archiver/config.toml.
Generate it with: cd terraform && terraform output -raw archiver_config > config.toml
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = Config::load()?;

    match args.first().map(String::as_str) {
        Some("migrate") => migrate(&config).await,

        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

async fn migrate(config: &Config) -> Result<()> {
    println!("loaded {config:?}");

    let pool = PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect(&config.database.url)
        .await
        .context("connecting to Postgres (is this host on the database IP allowlist?)")?;

    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await?;
    anyhow::ensure!(
        database == EXPECTED_DATABASE,
        "connected to database {database:?}, expected {EXPECTED_DATABASE:?}. \
         Refusing to migrate — this cluster also hosts the game services' `defaultdb`."
    );

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
