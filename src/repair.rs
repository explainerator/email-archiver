//! Re-fetch messages that were archived as empty.
//!
//! A fetch that returns no body is stored as an empty message rather than
//! raised as an error. That is deliberate: failing would leave the resume
//! watermark unmoved, and one message the server could never deliver would then
//! block every message behind it forever. Storing something and moving on keeps
//! the import finishing.
//!
//! What makes that safe is that the result is exactly identifiable afterwards.
//! Every empty body hashes to the same constant -- blake3 of zero bytes -- and
//! no real message can, because a real message has bytes. So the archive
//! already carries a precise list of its own gaps, needing no extra column and
//! no inference from size or an empty envelope.
//!
//! This reads that list and fills them in. Usually the second fetch works --
//! the failure was transient, and the server hands over all 59,248 bytes when
//! asked again.
//!
//! Sometimes it does not, and that case matters more than it looks. One message
//! on a fifteen-year-old mail store is zero bytes AT THE SOURCE: the server
//! reports RFC822.SIZE 0 and returns nothing, every time. There is nothing to
//! repair and never will be, which is reported as such rather than as a
//! failure.
//!
//! That message is the argument against ever failing the run on an empty fetch.
//! An error there would leave the resume point stuck before it, and every
//! message after it in that folder would go unarchived for as long as the
//! folder existed.

use anyhow::{Context, Result};
use chrono::Utc;
use futures::StreamExt;
use sqlx::PgPool;

use crate::config::Config;
use crate::db;
use crate::envelope;
use crate::store::Store;

pub async fn run(config: &Config, pool: &PgPool, address: &str, fix: bool) -> Result<()> {
    let owner = db::owner_of_address(pool, address).await?;
    let account = {
        let mut scope = db::Scope::begin(pool, owner).await?;
        db::account_by_address(&mut scope, address).await?
    };

    let empties = {
        let mut scope = db::Scope::begin(pool, account.user_id).await?;
        let found = db::empty_placements(&mut scope, account.id).await?;
        scope.commit().await?;
        found
    };

    if empties.is_empty() {
        println!("{address}: nothing stored empty");
        return Ok(());
    }

    println!("{address}: {} message(s) stored empty", empties.len());
    for empty in &empties {
        match empty.source_uid {
            Some(source_uid) => println!("  {} uid {source_uid}", empty.folder_name),
            None => println!(
                "  {} uid {} — no source uid recorded, cannot re-fetch",
                empty.folder_name, empty.uid
            ),
        }
    }

    if !fix {
        println!("\nre-fetch them with: email repair {address} --fix");
        return Ok(());
    }

    let source = crate::ingest::source_for_address(config, pool, address).await?;
    let store = Store::open(config, &account.bucket).await?;
    let mut session = crate::ingest::connect_session(&source).await?;

    let mut repaired = 0usize;
    let mut failed = 0usize;
    let mut genuinely_empty = 0usize;

    for empty in &empties {
        let Some(source_uid) = empty.source_uid else {
            // Without the source UID there is nothing to ask the server for.
            // Left in place rather than guessed at: an archive that quietly
            // invents a message is worse than one with a known hole.
            failed += 1;
            continue;
        };

        print!("  {}/{source_uid}: ", empty.folder_name);
        match refetch(
            &mut session,
            &store,
            pool,
            &account,
            empty,
            source_uid as u32,
        )
        .await
        {
            // Some(0) rather than an error: the server says this message
            // really is zero bytes. Found on a fifteen-year-old mail store,
            // where one message in Sent.2009 is empty at the source -- there
            // is nothing to repair, and calling it a failure would send
            // someone looking for a fault that is not ours.
            //
            // It is also why an empty fetch must never fail the run. Had it
            // errored, the resume point would never have moved past this UID,
            // and every message after it in the folder would have gone
            // unarchived -- permanently, and quietly.
            Ok(None) => {
                println!("empty at the source too -- nothing to repair");
                genuinely_empty += 1;
            }
            Ok(Some(bytes)) => {
                println!("repaired, {bytes} bytes");
                repaired += 1;
            }
            Err(e) => {
                // The whole chain: on a repair the server's own words are the
                // point, since the question is why it would not hand the
                // message over.
                println!("FAILED — {e:#}");
                failed += 1;
            }
        }
    }

    let _ = session.logout().await;

    // Only once every placement has been moved off it.
    if repaired > 0 {
        let mut scope = db::Scope::begin(pool, account.user_id).await?;
        if db::drop_empty_message_if_unused(&mut scope).await? {
            println!("  removed the empty message row");
        }
        scope.commit().await?;
    }

    println!("\nrepaired {repaired}, empty at the source {genuinely_empty}, failed {failed}");
    anyhow::ensure!(failed == 0, "{failed} message(s) could not be repaired");
    Ok(())
}

/// Fetch one message again and point its placement at the result.
async fn refetch(
    session: &mut crate::ingest::Session,
    store: &Store,
    pool: &PgPool,
    account: &db::Account,
    empty: &db::EmptyPlacement,
    source_uid: u32,
) -> Result<Option<usize>> {
    session
        .examine(&empty.folder_name)
        .await
        .with_context(|| format!("examining {}", empty.folder_name))?;

    let (raw, declared) = {
        let mut stream = session
            .uid_fetch(source_uid.to_string(), "(UID RFC822.SIZE BODY.PEEK[])")
            .await
            .context("fetching")?;

        let mut found = None;
        while let Some(item) = stream.next().await {
            let item = item?;
            if item.uid != Some(source_uid) {
                continue;
            }
            let body = item.body().unwrap_or_default().to_vec();
            found = Some((body, item.size.unwrap_or(0) as usize));
        }
        found.context("the server returned nothing for this uid")?
    };

    // The server agreeing the message is zero bytes is an ANSWER, not a
    // failure: it is empty at the source and always will be.
    if raw.is_empty() && declared == 0 {
        return Ok(None);
    }
    anyhow::ensure!(
        !raw.is_empty(),
        "the server returned an empty body but says the message is {declared} bytes"
    );
    // The same check ingest makes, for the same reason: a short body is a
    // failed fetch. Repairing an empty into a TRUNCATED message would be worse
    // than leaving it empty, because a truncated body hashes to something
    // plausible and stops being findable at all.
    anyhow::ensure!(
        declared == 0 || raw.len() == declared,
        "got {} bytes, server says {declared}",
        raw.len()
    );

    let indexed = match envelope::index(&raw, Utc::now()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("(unparseable: {e}; archiving raw) ");
            envelope::Indexed {
                internaldate: Utc::now(),
                subject: None,
                from_addr: None,
                envelope: serde_json::json!({}),
                bodystructure: serde_json::json!({}),
            }
        }
    };

    // Bytes first, index second: a message row pointing at an object that is
    // not there yet is the one ordering that can be observed as broken.
    let hash = store.put_message(&raw).await?;

    let mut scope = db::Scope::begin(pool, account.user_id).await?;
    let message_id = db::upsert_message(
        &mut scope,
        &hash,
        raw.len() as i64,
        indexed.internaldate,
        indexed.subject.as_deref(),
        indexed.from_addr.as_deref(),
        &indexed.envelope,
        &indexed.bodystructure,
        crate::fetch::split_header_body(&raw).0,
    )
    .await?;

    // Repoint rather than place: the placement, and the uid clients have
    // already seen for it, stay exactly as they are. Only what it resolves to
    // changes.
    db::repoint_placement(&mut scope, empty.folder_id, empty.uid, message_id).await?;
    scope.commit().await?;

    Ok(Some(raw.len()))
}
