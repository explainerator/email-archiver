//! Tests that exercise the real queries against the real database.
//!
//! These exist because of a specific failure. `sqlx::FromRow` binds by column
//! name and is checked at RUNTIME, so a query that selects `f.id` into a struct
//! field called `folder_id` compiles cleanly and then fails on first use. Search
//! shipped in that state. The scratch test meant to catch it had re-written the
//! query by hand — a `tests/` directory cannot import a binary crate — so it
//! exercised a copy with no `SearchRow` in it and passed happily.
//!
//! Every test here therefore calls the **actual function** the server calls.
//! A test that reimplements the thing it is testing is worse than no test: it
//! keeps passing while production is broken.
//!
//! They need `config.toml` and a reachable database. Where that is missing they
//! skip with a message rather than fail, so a checkout without credentials is
//! not a red build — but on a machine that can reach the archive, they run.

use email_archiver::{config::Config, db};

/// Open a scope the same way the server does.
async fn scope(pool: &sqlx::PgPool, user_id: i64) -> db::Scope<'_> {
    db::Scope::begin(pool, user_id).await.expect("scope")
}

/// Load the config the same way the binary does, or `None` if unavailable.
async fn pool() -> Option<(sqlx::PgPool, i64)> {
    let path = std::env::var("EMAIL_ARCHIVER_CONFIG").unwrap_or_else(|_| "config.toml".into());
    if !std::path::Path::new(&path).exists() {
        eprintln!("skipping: no {path}");
        return None;
    }
    std::env::set_var("EMAIL_ARCHIVER_CONFIG", &path);

    let config = Config::load().ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&config.database.url)
        .await
        .ok()
        .or_else(|| {
            eprintln!("skipping: database unreachable");
            None
        })?;

    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users ORDER BY id LIMIT 1")
        .fetch_one(&pool)
        .await
        .ok()?;
    Some((pool, user_id))
}

#[tokio::test]
async fn search_maps_every_column_it_selects() {
    let Some((pool, user_id)) = pool().await else {
        return;
    };

    // "a" appears in essentially every mailbox, so this returns rows on any
    // archive with content -- the point is to exercise the mapping, not to
    // assert on particular mail.
    let rows = db::search(&mut scope(&pool, user_id).await, "a", None, None, 5)
        .await
        .expect("search must not fail to map its own columns");

    assert!(!rows.is_empty(), "no rows; cannot verify the mapping");

    for row in &rows {
        // The field that shipped broken: selected as `f.id`, needed as
        // `folder_id`.
        assert!(row.folder_id > 0, "folder_id not populated");
        assert!(!row.account_label.is_empty(), "account_label not populated");
        assert!(!row.folder_name.is_empty(), "folder_name not populated");
        assert_eq!(row.blake3.len(), 64, "blake3 not populated");
        assert!(row.uid > 0, "uid not populated");
    }
}

#[tokio::test]
async fn search_scopes_to_the_user() {
    let Some((pool, user_id)) = pool().await else {
        return;
    };

    // A user id that cannot exist returns nothing rather than everything --
    // the guard against a missing WHERE clause silently exposing the archive.
    let rows = db::search(&mut scope(&pool, -1).await, "a", None, None, 5)
        .await
        .unwrap();
    assert!(
        rows.is_empty(),
        "search returned rows for a non-existent user"
    );

    let mine = db::search(&mut scope(&pool, user_id).await, "a", None, None, 5)
        .await
        .unwrap();
    assert!(!mine.is_empty(), "expected the real user to have results");
}

#[tokio::test]
async fn search_treats_wildcards_as_literal_text() {
    let Some((pool, user_id)) = pool().await else {
        return;
    };

    // '%' is a LIKE wildcard. Unescaped it matches every row; escaped it matches
    // only text actually containing a percent sign.
    //
    // Asserting on the CONTENT, not on counts: both queries hit the same limit,
    // so comparing lengths proves nothing -- which is how an earlier version of
    // this test managed to fail while the code was correct.
    for row in db::search(&mut scope(&pool, user_id).await, "%", None, None, 20)
        .await
        .unwrap()
    {
        let subject = row.subject.unwrap_or_default();
        let from = row.from_addr.unwrap_or_default();
        assert!(
            subject.contains('%') || from.contains('%'),
            "'%' matched a row containing no percent sign, so it acted as a              wildcard: subject={subject:?} from={from:?}"
        );
    }

    // Same for '_', which matches any single character when unescaped.
    for row in db::search(&mut scope(&pool, user_id).await, "_", None, None, 20)
        .await
        .unwrap()
    {
        let subject = row.subject.unwrap_or_default();
        let from = row.from_addr.unwrap_or_default();
        assert!(
            subject.contains('_') || from.contains('_'),
            "'_' acted as a wildcard: subject={subject:?} from={from:?}"
        );
    }
}

#[tokio::test]
async fn folders_and_message_pages_map_their_columns_too() {
    let Some((pool, user_id)) = pool().await else {
        return;
    };

    let folders = db::folders_for_user(&mut scope(&pool, user_id).await)
        .await
        .expect("folders");
    assert!(!folders.is_empty(), "expected folders");

    // The largest folder, so paging has something to page through.
    let (folder_id, _, _, _, total, _) = folders.iter().max_by_key(|f| f.4).unwrap().clone();
    assert!(total > 0);

    let page = db::messages_page(&mut scope(&pool, user_id).await, folder_id, None, 5)
        .await
        .expect("messages_page must map its own columns");
    assert!(!page.is_empty());
    for row in &page {
        assert_eq!(row.blake3.len(), 64);
        assert!(row.uid > 0);
    }

    // Keyset paging: the second page must not repeat the first.
    let last = page.last().unwrap();
    let next = db::messages_page(
        &mut scope(&pool, user_id).await,
        folder_id,
        Some((last.internaldate, last.uid)),
        5,
    )
    .await
    .expect("second page");

    let first_uids: Vec<i64> = page.iter().map(|r| r.uid).collect();
    for row in &next {
        assert!(
            !first_uids.contains(&row.uid),
            "cursor returned a row already on the previous page"
        );
    }
}
