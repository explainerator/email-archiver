//! Explain a completeness discrepancy in one folder.
//!
//! Ingest warns when a folder holds fewer messages than the source reports, but
//! it cannot say *why*. There are two very different causes and they need
//! different responses:
//!
//! * **Deduplicated** — two byte-identical messages in one folder collapse into
//!   a single placement. Nothing is lost; the count is just lower.
//! * **Genuinely missing** — a message on the server that we do not hold at all.
//!   That is data loss and needs fixing.
//!
//! A warning that cannot distinguish those is a warning that gets ignored, so
//! this checks each absent source UID against what we already store.

use anyhow::Result;
use futures::StreamExt;
use sqlx::PgPool;

use crate::config::Config;
use crate::db;
use crate::ingest;

pub async fn run(config: &Config, pool: &PgPool, address: &str, folder: &str) -> Result<()> {
    let account = db::account_by_address(pool, address).await?;
    let key = config.key()?;
    let source = db::source_for(pool, &key, address).await?;

    println!("diagnosing {address} / {folder}");

    let mut session = ingest::connect_session(&source).await?;
    let mailbox = session.examine(folder).await?;
    let source_exists = mailbox.exists as i64;

    // Every UID the source currently holds.
    let mut source_uids: Vec<u32> = session
        .uid_search("ALL")
        .await?
        .into_iter()
        .collect::<Vec<_>>();
    source_uids.sort_unstable();

    // Every source UID we recorded a placement for.
    let ours: Vec<i64> = sqlx::query_scalar(
        "SELECT p.source_uid FROM placements p
         JOIN folders f ON f.id = p.folder_id
         WHERE f.account_id = $1 AND f.name = $2 AND p.source_uid IS NOT NULL",
    )
    .bind(account.id)
    .bind(folder)
    .fetch_all(pool)
    .await?;
    let ours: std::collections::HashSet<i64> = ours.into_iter().collect();

    let absent: Vec<u32> = source_uids
        .iter()
        .copied()
        .filter(|u| !ours.contains(&(*u as i64)))
        .collect();

    println!("  source holds {source_exists}, we placed {}", ours.len());
    println!("  {} source UIDs have no placement", absent.len());
    if absent.is_empty() {
        return Ok(());
    }

    // For each absent UID, fetch it and see whether its content is already
    // archived under a different UID in this same folder. If so it was a
    // duplicate, not a loss.
    let mut duplicates = 0usize;
    let mut missing = Vec::new();

    for chunk in absent.chunks(50) {
        let set = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut stream = session.uid_fetch(&set, "(UID BODY.PEEK[])").await?;

        let mut fetched = Vec::new();
        while let Some(item) = stream.next().await {
            let item = item?;
            if let (Some(uid), Some(body)) = (item.uid, item.body()) {
                fetched.push((uid, body.to_vec()));
            }
        }
        drop(stream);

        for (uid, raw) in fetched {
            let hash = blake3::hash(&raw).to_hex().to_string();
            let placed_here: Option<i64> = sqlx::query_scalar(
                "SELECT p.uid FROM placements p
                 JOIN folders f  ON f.id = p.folder_id
                 JOIN messages m ON m.id = p.message_id
                 WHERE f.account_id = $1 AND f.name = $2 AND m.blake3 = $3",
            )
            .bind(account.id)
            .bind(folder)
            .bind(&hash)
            .fetch_optional(pool)
            .await?;

            match placed_here {
                Some(existing_uid) => {
                    duplicates += 1;
                    println!("  uid {uid}: duplicate of content already at our uid {existing_uid}");
                }
                None => {
                    missing.push(uid);
                    println!("  uid {uid}: NOT ARCHIVED ({} bytes)", raw.len());
                }
            }
        }
    }

    session.logout().await.ok();

    println!(
        "\n  {duplicates} deduplicated, {} genuinely missing",
        missing.len()
    );
    if !missing.is_empty() {
        println!("  missing source UIDs: {missing:?}");
    }
    Ok(())
}
