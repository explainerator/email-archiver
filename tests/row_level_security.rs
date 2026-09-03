//! Proves the policy is enforcing, not merely that the queries work.
//!
//! Every other test would pass identically with RLS switched off, because they
//! all go through a `Scope` and every query also carries its own `WHERE`. These
//! assert the thing that is only true when a policy exists: that reaching the
//! mail tables **without** an identity, or with somebody else's, returns
//! nothing.
//!
//! Note what a failure here means. If the archive can be read with no identity
//! set, the policy is not on — it is not that an attacker got in, it is that
//! the layer meant to catch our own future mistakes is absent.

use email_archiver::{config::Config, db};

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

const PROTECTED: [&str; 3] = ["messages", "folders", "placements"];

#[tokio::test]
async fn the_policy_is_actually_on() {
    let Some(pool) = pool().await else { return };

    let rows: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT relname, relrowsecurity, relforcerowsecurity
           FROM pg_class WHERE relname = ANY($1)",
    )
    .bind(&PROTECTED[..])
    .fetch_all(&pool)
    .await
    .expect("query pg_class");

    assert_eq!(rows.len(), PROTECTED.len(), "a protected table is missing");
    for (table, enabled, forced) in rows {
        assert!(enabled, "{table}: row level security is not enabled");
        // Without FORCE the policy would not apply to the owner, which is the
        // only role that ever connects -- so it would be decorative.
        assert!(forced, "{table}: RLS is enabled but not FORCEd");
    }
}

#[tokio::test]
async fn no_identity_sees_nothing() {
    let Some(pool) = pool().await else { return };

    for table in PROTECTED {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("{table}: unscoped count failed: {e}"));
        assert_eq!(n, 0, "{table}: readable with no identity set");
    }
}

#[tokio::test]
async fn an_empty_identity_is_treated_as_none() {
    // Regression test for migration 0008. `current_setting(..., true)` yields
    // NULL only while the variable has never been set; once a transaction on
    // the connection has set it, a rollback leaves the EMPTY STRING, and
    // `''::bigint` raises instead of returning NULL. The behaviour therefore
    // depended on whether the pooled connection had been used before.
    let Some(pool) = pool().await else { return };

    let mut tx = sqlx::Acquire::begin(&pool).await.unwrap();
    sqlx::query("SELECT set_config('archive.user_id', '1', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    // Same connection, now with the setting present but empty.
    for table in PROTECTED {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| {
                panic!("{table}: an empty identity errored instead of hiding rows: {e}")
            });
        assert_eq!(n, 0, "{table}: readable with an empty identity");
    }
}

#[tokio::test]
async fn another_users_identity_sees_nothing_of_mine() {
    let Some(pool) = pool().await else { return };

    let users: Vec<(i64,)> = sqlx::query_as("SELECT id FROM users ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    if users.len() < 2 {
        eprintln!("skipping: needs two users");
        return;
    }
    let (owner, other) = (users[0].0, users[1].0);

    let mut mine = db::Scope::begin(&pool, owner).await.unwrap();
    let count_mine: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(mine.conn())
        .await
        .unwrap();
    drop(mine);
    assert!(
        count_mine > 0,
        "the first user has no mail; test proves nothing"
    );

    // The same query, the same table, a different declared identity.
    let mut theirs = db::Scope::begin(&pool, other).await.unwrap();
    let count_theirs: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(theirs.conn())
        .await
        .unwrap();
    drop(theirs);

    assert_ne!(
        count_mine, count_theirs,
        "both identities see the same rows; the policy is not filtering"
    );
}

#[tokio::test]
async fn a_query_missing_its_where_clause_still_sees_nothing() {
    // The whole point of the exercise, stated as a test.
    //
    // This is a deliberately WRONG query: it reads placements with no user
    // predicate at all, exactly as a future careless one might. Before RLS it
    // returned every row in the table. It must now return only the caller's.
    let Some(pool) = pool().await else { return };

    let users: Vec<(i64,)> = sqlx::query_as("SELECT id FROM users ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    if users.len() < 2 {
        eprintln!("skipping: needs two users");
        return;
    }

    let total_unscoped: i64 = sqlx::query_scalar("SELECT count(*) FROM placements")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total_unscoped, 0, "unscoped read saw rows");

    let mut a = db::Scope::begin(&pool, users[0].0).await.unwrap();
    let seen_a: i64 = sqlx::query_scalar("SELECT count(*) FROM placements")
        .fetch_one(a.conn())
        .await
        .unwrap();
    drop(a);

    let mut b = db::Scope::begin(&pool, users[1].0).await.unwrap();
    let seen_b: i64 = sqlx::query_scalar("SELECT count(*) FROM placements")
        .fetch_one(b.conn())
        .await
        .unwrap();
    drop(b);

    // Neither can see the other's, and together they do not see one pile.
    assert_ne!(
        seen_a, seen_b,
        "a WHERE-less query returned the same rows to both users"
    );
}
