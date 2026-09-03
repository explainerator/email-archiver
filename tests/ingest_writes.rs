//! Exercises the INGEST WRITE PATH against the real database.
//!
//! `folder_for_ingest` and `place_message` commit their own transactions, so
//! they cannot be tested by rolling one back. They are also the only code that
//! populates the denormalised `user_id` added in migration 0006, and getting
//! that wrong would be invisible on the read side — reads would simply work,
//! and the column would quietly disagree with reality until row-level security
//! started filtering on it.
//!
//! So this creates a throwaway user, drives the real functions, checks what
//! landed, and deletes it again. Every foreign key in this schema is
//! `ON DELETE RESTRICT`, so cleanup has to run child-first; it runs even when
//! an assertion fails, or a failed run would leave rows behind and the next run
//! would collide on the unique login.

use email_archiver::{config::Config, db};

// An address, not a name: logins are email addresses since migration 0010, and
// create_user now rejects anything else. `.invalid` is reserved by RFC 2606 so
// this can never collide with a real address.
const TEST_LOGIN: &str = "write-path-probe@email-archiver.invalid";
const TEST_BUCKET: &str = "rls-write-path-probe-bucket";
const TEST_ADDRESS: &str = "probe@write-path.invalid";

async fn pool() -> Option<sqlx::PgPool> {
    let path = std::env::var("EMAIL_ARCHIVER_CONFIG").unwrap_or_else(|_| "config.toml".into());
    if !std::path::Path::new(&path).exists() {
        eprintln!("skipping: no {path}");
        return None;
    }
    std::env::set_var("EMAIL_ARCHIVER_CONFIG", &path);
    let config = Config::load().ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&config.database.url)
        .await
        .ok()
}

/// Remove the probe's rows, children first. Safe to call when nothing exists.
///
/// The three mail tables carry a policy, so their deletes must run inside a
/// scope or they match nothing and leave the rows behind — which is exactly
/// what happened the first time this ran under RLS, cascading into foreign-key
/// failures on the tables that follow. `accounts` and `users` have no policy
/// and are deleted afterwards, unscoped.
async fn cleanup(pool: &sqlx::PgPool) {
    // Keyed on the BUCKET, not the login. The bucket is what carries the unique
    // constraint that blocks a re-run, and it outlives login changes -- an
    // earlier aborted run left a row behind under a login this test no longer
    // uses, and cleanup keyed on login could not see it.
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE bucket = $1")
        .bind(TEST_BUCKET)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
    let Some(id) = id else { return };

    match db::Scope::begin(pool, id).await {
        Ok(mut scope) => {
            // accounts is in here too since migration 0009 gave it a policy --
            // it holds decryptable source credentials. Leaving it outside the
            // scope made its delete match nothing, and the users delete then
            // failed on the foreign key.
            for sql in [
                "DELETE FROM placements WHERE user_id = $1",
                "DELETE FROM messages   WHERE user_id = $1",
                "DELETE FROM folders    WHERE user_id = $1",
                "DELETE FROM accounts   WHERE user_id = $1",
            ] {
                if let Err(e) = sqlx::query(sql).bind(id).execute(scope.conn()).await {
                    eprintln!("cleanup failed on {sql}: {e}");
                }
            }
            if let Err(e) = scope.commit().await {
                eprintln!("cleanup commit failed: {e}");
            }
        }
        Err(e) => eprintln!("cleanup could not open a scope: {e}"),
    }

    // `users` is the only table left with no policy, so it is the only delete
    // that can run unscoped.
    if let Err(e) = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
    {
        eprintln!("cleanup failed to delete the user: {e}");
    }
}

#[tokio::test]
async fn ingest_writes_populate_the_denormalised_owner() {
    let Some(pool) = pool().await else { return };

    // A previous failed run must not make this one collide on the unique login.
    cleanup(&pool).await;

    let result = run(&pool).await;

    // Always, even on failure: leftovers would break the next run and leave a
    // stray user in a production table.
    cleanup(&pool).await;

    result.expect("ingest write path");
}

async fn run(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let user_id = db::create_user(pool, TEST_LOGIN, TEST_BUCKET, "write path probe").await?;
    let account_id = db::create_account(pool, TEST_LOGIN, TEST_ADDRESS, "probe", "imap").await?;

    // --- folders -------------------------------------------------------------
    // Through a Scope, exactly as ingest does since RLS phase 2b.
    let mut scope = db::Scope::begin(pool, user_id).await?;
    let folder = db::folder_for_ingest(&mut scope, account_id, "Probe", 1).await?;
    scope.commit().await?;

    // Read back through a scope. Unscoped, the policy hides the row and this
    // fails with "no rows returned" -- which is the mechanism working, and is
    // how this test first proved the policy was live.
    let mut scope = db::Scope::begin(pool, user_id).await?;
    let folder_owner: i64 = sqlx::query_scalar("SELECT user_id FROM folders WHERE id = $1")
        .bind(folder.id)
        .fetch_one(scope.conn())
        .await?;
    drop(scope);
    anyhow::ensure!(
        folder_owner == user_id,
        "folder owner {folder_owner} should be {user_id}"
    );

    // --- messages and placements --------------------------------------------
    let mut scope = db::Scope::begin(pool, user_id).await?;
    let message_id = db::upsert_message(
        &mut scope,
        &"c".repeat(64),
        42,
        chrono::Utc::now(),
        Some("probe"),
        Some("probe@write-path.invalid"),
        &serde_json::json!({}),
        &serde_json::json!({ "parts": [] }),
        b"Subject: probe\r\n",
    )
    .await?;

    let (uid, created) = db::place_message(&mut scope, folder.id, message_id, 1, false).await?;
    anyhow::ensure!(created, "placement should be new");
    scope.commit().await?;

    let mut scope = db::Scope::begin(pool, user_id).await?;
    let placement_owner: i64 =
        sqlx::query_scalar("SELECT user_id FROM placements WHERE folder_id = $1 AND uid = $2")
            .bind(folder.id)
            .bind(uid)
            .fetch_one(scope.conn())
            .await?;
    drop(scope);
    anyhow::ensure!(
        placement_owner == user_id,
        "placement owner {placement_owner} should be {user_id}"
    );

    // --- the guard against a silently empty insert ---------------------------
    // INSERT ... SELECT inserts nothing when the SELECT matches nothing, where
    // the old VALUES form raised a foreign-key violation. place_message must
    // notice, rather than returning a UID for a placement it never stored.
    let missing_folder = i64::MAX;
    let mut scope = db::Scope::begin(pool, user_id).await?;
    let outcome = db::place_message(&mut scope, missing_folder, message_id, 2, false).await;
    anyhow::ensure!(
        outcome.is_err(),
        "placing into a non-existent folder silently succeeded"
    );

    Ok(())
}
