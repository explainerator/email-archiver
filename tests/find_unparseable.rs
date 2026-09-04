//! Locate messages that failed to parse at ingest.
//!
//! Diagnostic, run on demand:
//!
//!     cargo test --test find_unparseable -- --nocapture --ignored
//!
//! An unparseable message keeps its bytes but loses its index: no subject, no
//! sender, and -- the part that shows -- an internaldate of the moment it was
//! ingested rather than when it was sent, which floats it to the top of the
//! folder. Empty envelope is the marker, since every parsed message has at
//! least a date in it.

use email_archiver::{config::Config, db};

async fn pool() -> Option<sqlx::PgPool> {
    let config = Config::load().ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&config.database.url)
        .await
        .ok()
}

#[tokio::test]
#[ignore = "diagnostic; run with --ignored"]
async fn find_unparseable() {
    let Some(pool) = pool().await else {
        eprintln!("no EMAIL_ARCHIVER_CONFIG; skipping");
        return;
    };

    // UserSummary carries no id, so the login is resolved back through the
    // same path every command uses.
    let users = db::all_users(&pool).await.expect("users");

    for user in &users {
        let (user_id, _) = db::user_by_login(&pool, &user.login)
            .await
            .expect("user_by_login")
            .expect("a listed user must resolve");
        // Scoped: messages is policy-covered, so an unscoped read would report
        // a clean archive for every user no matter what is in it.
        let mut scope = db::Scope::begin(&pool, user_id).await.expect("scope");

        let rows: Vec<(
            String,
            Option<String>,
            Option<String>,
            chrono::DateTime<chrono::Utc>,
            i64,
            String,
            i64,
        )> = sqlx::query_as(
            "SELECT m.blake3,
                        m.subject,
                        m.from_addr,
                        m.internaldate,
                        m.size,
                        f.name,
                        p.source_uid
                   FROM messages m
                   JOIN placements p ON p.message_id = m.id
                   JOIN folders f ON f.id = p.folder_id
                  WHERE m.user_id = $1
                    AND m.envelope = '{}'::jsonb
                  ORDER BY m.internaldate DESC",
        )
        .bind(user_id)
        .fetch_all(scope.conn())
        .await
        .expect("query");

        scope.commit().await.ok();

        println!("\n=== {} ===", user.login);
        if rows.is_empty() {
            println!("  none");
            continue;
        }
        for (blake3, subject, from, date, size, folder, uid) in &rows {
            println!("  {folder} uid {uid}");
            println!("    blake3       {blake3}");
            println!("    size         {size} bytes");
            println!("    internaldate {date}  <- ingest time, not the real date");
            println!("    subject      {subject:?}");
            println!("    from         {from:?}");
        }
        println!("  {} unparseable message(s)", rows.len());
    }
}
