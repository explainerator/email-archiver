//! Postgres index operations.
//!
//! Queries are runtime-checked (`sqlx::query`) rather than the compile-time
//! `query!` macros. The macros would make every build depend on a reachable
//! database or a checked-in offline cache, which is a poor trade for a project
//! one person builds occasionally from more than one machine.
//!
//! **Every query that touches user data takes a `user_id` and filters on it.**
//! Per-user S3 buckets are a structural boundary; Postgres is not — separation
//! here is only as good as the predicates, so they live in this one module
//! rather than being written ad hoc by callers. See ARCHIVE-PLAN.md 2.4 and R5.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct Account {
    pub id: i64,
    pub user_id: i64,
    pub address: String,
    pub label: String,
    /// The user's S3 bucket. Carried here so ingest never has to guess which
    /// bucket an account's mail belongs in.
    pub bucket: String,
}

#[derive(Debug, Clone)]
pub struct Folder {
    pub id: i64,
    pub uidnext: i64,
    pub source_uidvalidity: Option<i64>,
    pub last_source_uid: i64,
}

pub async fn create_user(pool: &PgPool, login: &str, bucket: &str, display: &str) -> Result<i64> {
    // password_hash is a placeholder until IMAP auth lands in Phase 4. It is
    // deliberately not a valid hash, so nothing can authenticate as this user
    // by accident before the real credential is set.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO users (login, password_hash, bucket, display_name)
         VALUES ($1, '!', $2, $3)
         ON CONFLICT (login) DO UPDATE SET bucket = EXCLUDED.bucket
         RETURNING id",
    )
    .bind(login)
    .bind(bucket)
    .bind(display)
    .fetch_one(pool)
    .await
    .with_context(|| format!("creating user {login}"))?;
    Ok(id)
}

pub async fn create_account(
    pool: &PgPool,
    login: &str,
    address: &str,
    label: &str,
    provider: &str,
) -> Result<i64> {
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE login = $1")
        .bind(login)
        .fetch_optional(pool)
        .await?
        .with_context(|| format!("no such user {login:?} — create it first"))?;

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (user_id, address, label, provider)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (address) DO UPDATE SET label = EXCLUDED.label
         RETURNING id",
    )
    .bind(user_id)
    .bind(address)
    .bind(label)
    .bind(provider)
    .fetch_one(pool)
    .await
    .with_context(|| format!("creating account {address}"))?;
    Ok(id)
}

pub async fn account_by_address(pool: &PgPool, address: &str) -> Result<Account> {
    let row: (i64, i64, String, String, String) = sqlx::query_as(
        "SELECT a.id, a.user_id, a.address, a.label, u.bucket
         FROM accounts a JOIN users u ON u.id = a.user_id
         WHERE a.address = $1",
    )
    .bind(address)
    .fetch_optional(pool)
    .await?
    .with_context(|| format!("no account {address:?} in the database — add it first"))?;

    Ok(Account {
        id: row.0,
        user_id: row.1,
        address: row.2,
        label: row.3,
        bucket: row.4,
    })
}

/// Fetch or create a folder, and reset resume state if the source server's
/// UIDVALIDITY changed.
///
/// A changed UIDVALIDITY means the source's UIDs no longer refer to the same
/// messages. Continuing from `last_source_uid` would silently skip mail, so the
/// folder is rescanned from zero. Our own `uidnext` is untouched — the UIDs we
/// serve to clients must never be reissued.
pub async fn folder_for_ingest(
    pool: &PgPool,
    account_id: i64,
    name: &str,
    source_uidvalidity: i64,
) -> Result<Folder> {
    let existing: Option<(i64, i64, Option<i64>, i64)> = sqlx::query_as(
        "SELECT id, uidnext, source_uidvalidity, last_source_uid
         FROM folders WHERE account_id = $1 AND name = $2",
    )
    .bind(account_id)
    .bind(name)
    .fetch_optional(pool)
    .await?;

    if let Some((id, uidnext, existing_validity, last_uid)) = existing {
        if existing_validity != Some(source_uidvalidity) {
            eprintln!(
                "  {name}: source UIDVALIDITY changed ({existing_validity:?} -> \
                 {source_uidvalidity}); rescanning from the start"
            );
            sqlx::query(
                "UPDATE folders SET source_uidvalidity = $2, last_source_uid = 0 WHERE id = $1",
            )
            .bind(id)
            .bind(source_uidvalidity)
            .execute(pool)
            .await?;
            return Ok(Folder {
                id,
                uidnext,
                source_uidvalidity: Some(source_uidvalidity),
                last_source_uid: 0,
            });
        }
        return Ok(Folder {
            id,
            uidnext,
            source_uidvalidity: existing_validity,
            last_source_uid: last_uid,
        });
    }

    // Our UIDVALIDITY is generated once, at creation, and never changes.
    let uidvalidity = Utc::now().timestamp();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO folders (account_id, name, uidvalidity, uidnext, source_uidvalidity, last_source_uid)
         VALUES ($1, $2, $3, 1, $4, 0)
         RETURNING id",
    )
    .bind(account_id)
    .bind(name)
    .bind(uidvalidity)
    .bind(source_uidvalidity)
    .fetch_one(pool)
    .await
    .with_context(|| format!("creating folder {name}"))?;

    Ok(Folder {
        id,
        uidnext: 1,
        source_uidvalidity: Some(source_uidvalidity),
        last_source_uid: 0,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn upsert_message(
    pool: &PgPool,
    user_id: i64,
    blake3: &str,
    size: i64,
    internaldate: DateTime<Utc>,
    subject: Option<&str>,
    from_addr: Option<&str>,
    envelope: &serde_json::Value,
    bodystructure: &serde_json::Value,
) -> Result<i64> {
    // Deduplication is per user, matching the per-user buckets: the same
    // message arriving in two of one person's accounts is stored once.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO messages
             (user_id, blake3, size, internaldate, subject, from_addr, envelope, bodystructure)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (user_id, blake3) DO UPDATE SET user_id = messages.user_id
         RETURNING id",
    )
    .bind(user_id)
    .bind(blake3)
    .bind(size)
    .bind(internaldate)
    .bind(subject)
    .bind(from_addr)
    .bind(envelope)
    .bind(bodystructure)
    .fetch_one(pool)
    .await
    .context("inserting message")?;
    Ok(id)
}

/// Claim the next UID in a folder and place a message at it.
///
/// Returns `None` if the message is already in this folder — a re-run must not
/// create a second placement, or the message would appear twice.
pub async fn place_message(
    pool: &PgPool,
    folder_id: i64,
    message_id: i64,
    seen: bool,
) -> Result<Option<i64>> {
    let mut tx = pool.begin().await?;

    let already: Option<i64> =
        sqlx::query_scalar("SELECT uid FROM placements WHERE folder_id = $1 AND message_id = $2")
            .bind(folder_id)
            .bind(message_id)
            .fetch_optional(&mut *tx)
            .await?;
    if already.is_some() {
        tx.rollback().await?;
        return Ok(None);
    }

    // Lock the folder row so two ingest passes cannot hand out the same UID.
    let uid: i64 = sqlx::query_scalar(
        "UPDATE folders SET uidnext = uidnext + 1 WHERE id = $1 RETURNING uidnext - 1",
    )
    .bind(folder_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("INSERT INTO placements (folder_id, uid, message_id, seen) VALUES ($1, $2, $3, $4)")
        .bind(folder_id)
        .bind(uid)
        .bind(message_id)
        .bind(seen)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(Some(uid))
}

/// Record ingest progress. Only ever moves forward.
pub async fn advance_source_uid(pool: &PgPool, folder_id: i64, source_uid: i64) -> Result<()> {
    sqlx::query("UPDATE folders SET last_source_uid = GREATEST(last_source_uid, $2) WHERE id = $1")
        .bind(folder_id)
        .bind(source_uid)
        .execute(pool)
        .await?;
    Ok(())
}
