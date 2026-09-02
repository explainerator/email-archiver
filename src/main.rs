//! email-archiver — read-only email archive.
//!
//! Phase 1 scope: apply database migrations and report the resulting schema.
//! The IMAP server and ingest worker follow in later phases; see
//! ARCHIVE-PLAN.md for the design and phasing.

use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;

/// The archive has its own database on a cluster shared with the game services
/// (`defaultdb`) and `backroom`. Writing into the wrong one would be difficult
/// to notice and unpleasant to unpick, so it is asserted before migrating.
const EXPECTED_DATABASE: &str = "archive";

/// Capped deliberately low. The Postgres cluster is shared with the game
/// services and we connect as the same `gern` role, so a bulk ingest must not
/// be able to exhaust the instance's connection budget. See ARCHIVE-PLAN.md R4.
const MAX_CONNECTIONS: u32 = 3;

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = std::env::var("DATABASE_URL").context(
        "DATABASE_URL is not set. Expected form:\n  \
         postgres://gern:PASSWORD@qw300972-001.ca.clouddb.ovh.net:35628/archive?sslmode=require\n\n\
         Note the database is `archive`, not `defaultdb`. The host must also be on the \
         cluster's IP allowlist.",
    )?;

    let pool = PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect(&database_url)
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
