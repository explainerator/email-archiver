//! Consistency check between Postgres and S3.
//!
//! The archive rests on a claim: the index is derivable from the bucket alone.
//! That claim is only true if every placement has a manifest and every message
//! has its blob — so this checks it directly rather than assuming.
//!
//! Cheap enough to run after every ingest, and the natural thing to run before
//! trusting a restore.

use anyhow::Result;
use futures::stream::{self, StreamExt};
use sqlx::PgPool;

use crate::config::Config;
use crate::db;
use crate::store::{Manifest, Store};

pub async fn run(config: &Config, pool: &PgPool, login: &str, deep: bool) -> Result<()> {
    let (user_id, bucket): (i64, String) =
        sqlx::query_as("SELECT id, bucket FROM users WHERE login = $1")
            .bind(login)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no such user {login:?}"))?;

    println!("checking {login} (bucket {bucket})");

    let messages: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await?;

    let placements: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM placements p
         JOIN folders f  ON f.id = p.folder_id
         JOIN accounts a ON a.id = f.account_id
         WHERE a.user_id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    let store = Store::open(config, &bucket).await?;
    let blobs = store.list("messages/").await?;
    let manifests = store.list("manifest/").await?;

    println!("  postgres: {messages} messages, {placements} placements");
    println!("  s3:       {} blobs, {} manifests", blobs.len(), manifests.len());

    let mut problems = Vec::new();

    // One manifest per placement: a placement without one would be invisible
    // to a rebuild, which is the failure that matters most here.
    if manifests.len() as i64 != placements {
        problems.push(format!(
            "manifest count {} != placement count {placements} — a rebuild would not \
             reproduce the index",
            manifests.len()
        ));
    }

    // One blob per distinct message. Fewer blobs means mail we cannot serve.
    if (blobs.len() as i64) < messages {
        problems.push(format!(
            "{} blobs for {messages} messages — some message bodies are missing",
            blobs.len()
        ));
    }

    // Read blobs back and confirm they still hash to their key. get_message
    // re-hashes, so silent corruption surfaces here rather than when someone
    // opens a twenty-year-old message.
    //
    // Sampling answers "is the archive plausibly intact". Only --deep answers
    // "is every body actually readable", which is the question that matters
    // before trusting this as the remote copy.
    let hashes: Vec<String> = if deep {
        sqlx::query_scalar("SELECT blake3 FROM messages WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query_scalar(
            "SELECT blake3 FROM messages WHERE user_id = $1 ORDER BY random() LIMIT 5",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await?
    };

    let concurrency = config.ingest.concurrency;
    let store_ref = &store;
    let results: Vec<(String, Result<()>)> = stream::iter(hashes.iter().map(|h| async move {
        (h.clone(), store_ref.get_message(h).await.map(|_| ()))
    }))
    .buffer_unordered(concurrency)
    .collect()
    .await;

    let mut verified = 0usize;
    for (hash, outcome) in results {
        match outcome {
            Ok(()) => verified += 1,
            Err(e) => problems.push(format!("blob {hash} unreadable or corrupt: {e}")),
        }
    }

    if deep {
        println!("  deep verify: {verified}/{} blobs read and hash-checked", hashes.len());
    } else {
        println!(
            "  spot-check: {verified}/{} sampled blobs verified (use --deep for all)",
            hashes.len()
        );
    }

    if problems.is_empty() {
        println!("  consistent");
        Ok(())
    } else {
        for p in &problems {
            eprintln!("  PROBLEM: {p}");
        }
        anyhow::bail!("{} consistency problem(s)", problems.len())
    }
}

/// Regenerate every manifest for a user from the index.
///
/// Purges the existing `manifest/` prefix first, version-aware: these buckets
/// have versioning enabled, so a plain delete would leave stale objects behind
/// as billed noncurrent versions — and, worse, the count would still look wrong
/// to a rebuild that lists versions.
pub async fn rebuild_manifests(config: &Config, pool: &PgPool, login: &str) -> Result<()> {
    let (user_id, bucket): (i64, String) =
        sqlx::query_as("SELECT id, bucket FROM users WHERE login = $1")
            .bind(login)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no such user {login:?}"))?;

    let store = Store::open(config, &bucket).await?;
    println!("rebuilding manifests for {login} (bucket {bucket})");

    let stale = store.list_versions("manifest/").await?;
    for (key, version) in &stale {
        store.delete_version(key, version).await?;
    }
    println!("  purged {} stale manifest versions", stale.len());

    let rows = db::placements_for_user(pool, user_id).await?;
    for row in &rows {
        store
            .put_manifest(&Manifest {
                account: row.address.clone(),
                folder: row.folder.clone(),
                uid: row.uid,
                // Rows ingested before placements.source_uid existed have no
                // value to restore; 0 marks it as unknown rather than implying
                // a real UID.
                source_uid: row.source_uid.unwrap_or(0),
                internaldate: row.internaldate.to_rfc3339(),
                seen: row.seen,
                blake3: row.blake3.clone(),
                size: row.size,
            })
            .await?;
    }
    println!("  wrote {} manifests", rows.len());
    Ok(())
}
