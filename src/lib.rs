//! email-archiver as a library, with `main.rs` as a thin CLI over it.
//!
//! The split exists so that **tests can call the real functions**. Everything
//! lived in the binary until a search query shipped broken: `sqlx::FromRow`
//! binds by column name, an unaliased `f.id` arrived as `id` instead of
//! `folder_id`, and the failure was at runtime rather than compile time. The
//! scratch test that was supposed to catch it had *re-written the query* rather
//! than calling `db::search`, because a `tests/` directory cannot reach into a
//! binary crate — so it exercised a copy that had no `SearchRow` in it, and
//! passed.
//!
//! Testing a reimplementation instead of the real thing is worse than no test:
//! it passes while production is broken. This module exists to make that
//! impossible for the database layer.

pub mod check;
pub mod config;
pub mod db;
pub mod diagnose;
pub mod envelope;
pub mod fetch;
pub mod gmail;
pub mod ingest;
pub mod listen;
pub mod naming;
pub mod probe;
pub mod ratelimit;
pub mod repair;
pub mod sanitise;
pub mod secrets;
pub mod server;
pub mod session;
pub mod store;
pub mod tls;
pub mod web;

use anyhow::{Context, Result};
use config::Config;
use sqlx::postgres::PgPoolOptions;

/// The archive has its own database on a cluster shared with the game services
/// (`defaultdb`) and `backroom`. Writing into the wrong one would be difficult
/// to notice and unpleasant to unpick, so it is asserted before migrating.
pub const EXPECTED_DATABASE: &str = "archive";

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
pub async fn connect_db(config: &Config) -> Result<sqlx::PgPool> {
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
