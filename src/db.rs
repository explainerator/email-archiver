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
    headers: &[u8],
) -> Result<i64> {
    // Deduplication is per user, matching the per-user buckets: the same
    // message arriving in two of one person's accounts is stored once.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO messages
             (user_id, blake3, size, internaldate, subject, from_addr, envelope,
              bodystructure, headers)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (user_id, blake3)
             DO UPDATE SET headers = COALESCE(messages.headers, EXCLUDED.headers)
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
    .bind(headers)
    .fetch_one(pool)
    .await
    .context("inserting message")?;
    Ok(id)
}

/// Claim the next UID in a folder and place a message at it.
///
/// Returns `(uid, is_new)`. On a re-run the existing uid comes back with
/// `is_new = false` rather than nothing: the caller still needs the uid so it
/// can rewrite the manifest, which is what makes a re-ingest repair missing or
/// wrongly-keyed manifests instead of silently leaving them broken.
pub async fn place_message(
    pool: &PgPool,
    folder_id: i64,
    message_id: i64,
    source_uid: i64,
    seen: bool,
) -> Result<(i64, bool)> {
    let mut tx = pool.begin().await?;

    let already: Option<i64> =
        sqlx::query_scalar("SELECT uid FROM placements WHERE folder_id = $1 AND message_id = $2")
            .bind(folder_id)
            .bind(message_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(uid) = already {
        // Backfill source_uid if this row predates the column. Without this a
        // re-ingest cannot repair it, and the database would stay unable to
        // reproduce manifests field-for-field. COALESCE so an existing value is
        // never overwritten.
        sqlx::query(
            "UPDATE placements SET source_uid = COALESCE(source_uid, $3)
             WHERE folder_id = $1 AND uid = $2",
        )
        .bind(folder_id)
        .bind(uid)
        .bind(source_uid)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok((uid, false));
    }

    // Lock the folder row so two ingest passes cannot hand out the same UID.
    let uid: i64 = sqlx::query_scalar(
        "UPDATE folders SET uidnext = uidnext + 1 WHERE id = $1 RETURNING uidnext - 1",
    )
    .bind(folder_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO placements (folder_id, uid, message_id, source_uid, seen)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(folder_id)
    .bind(uid)
    .bind(message_id)
    .bind(source_uid)
    .bind(seen)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((uid, true))
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

/// Everything needed to regenerate one user's manifests from the index.
///
/// The mirror of a rebuild: manifests reconstruct the database after a
/// disaster, and this reconstructs manifests when they drift or were written
/// under an older key scheme.
pub struct PlacementRow {
    pub address: String,
    pub folder: String,
    pub uid: i64,
    pub source_uid: Option<i64>,
    pub internaldate: DateTime<Utc>,
    pub seen: bool,
    pub blake3: String,
    pub size: i64,
}

pub async fn placements_for_user(pool: &PgPool, user_id: i64) -> Result<Vec<PlacementRow>> {
    let rows: Vec<(
        String,
        String,
        i64,
        Option<i64>,
        DateTime<Utc>,
        bool,
        String,
        i64,
    )> = sqlx::query_as(
        "SELECT a.address, f.name, p.uid, p.source_uid, m.internaldate, p.seen,
                    m.blake3, m.size
             FROM placements p
             JOIN folders  f ON f.id = p.folder_id
             JOIN accounts a ON a.id = f.account_id
             JOIN messages m ON m.id = p.message_id
             WHERE a.user_id = $1
             ORDER BY a.address, f.name, p.uid",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| PlacementRow {
            address: r.0,
            folder: r.1,
            uid: r.2,
            source_uid: r.3,
            internaldate: r.4,
            seen: r.5,
            blake3: r.6,
            size: r.7,
        })
        .collect())
}

/// How many messages we hold in one folder.
pub async fn count_placements(pool: &PgPool, folder_id: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM placements WHERE folder_id = $1")
            .bind(folder_id)
            .fetch_one(pool)
            .await?,
    )
}

/// Messages for a user with no cached header block yet.
pub async fn messages_missing_headers(pool: &PgPool, user_id: i64) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT blake3 FROM messages WHERE user_id = $1 AND headers IS NULL")
            .bind(user_id)
            .fetch_all(pool)
            .await?,
    )
}

/// Record the hierarchy delimiter the source server reported.
///
/// Written on every ingest rather than once, so a server that changes its
/// layout is picked up rather than remembered wrongly.
/// Decrypted source credentials for one account.
///
/// The password is decrypted here and lives only in memory. Nothing writes it
/// back, and Source's Debug redacts it.
pub async fn source_for(
    pool: &PgPool,
    key: &crate::secrets::SecretKey,
    address: &str,
) -> Result<crate::config::Source> {
    let row: Option<(Option<String>, i32, Option<String>, Option<String>, bool)> = sqlx::query_as(
        "SELECT imap_host, imap_port, imap_username, imap_password_enc, allow_invalid_certs
         FROM accounts WHERE address = $1",
    )
    .bind(address)
    .fetch_optional(pool)
    .await?;

    let (host, port, username, password_enc, allow_invalid_certs) =
        row.with_context(|| format!("no account {address:?} in the database"))?;

    let host = host.with_context(|| {
        format!(
            "account {address:?} has no source credentials. Set them with: \
                 email-archiver set-source {address} <host> <username>"
        )
    })?;
    let username = username.context("account has a host but no username")?;
    let password_enc = password_enc.context("account has a host but no password")?;

    Ok(crate::config::Source {
        host,
        port: port as u16,
        username,
        password: key.decrypt(&password_enc)?,
        allow_invalid_certs,
    })
}

pub async fn set_source(
    pool: &PgPool,
    key: &crate::secrets::SecretKey,
    address: &str,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    allow_invalid_certs: bool,
) -> Result<()> {
    let encrypted = key.encrypt(password)?;
    let updated = sqlx::query(
        "UPDATE accounts
         SET imap_host = $2, imap_port = $3, imap_username = $4,
             imap_password_enc = $5, allow_invalid_certs = $6
         WHERE address = $1",
    )
    .bind(address)
    .bind(host)
    .bind(port as i32)
    .bind(username)
    .bind(&encrypted)
    .bind(allow_invalid_certs)
    .execute(pool)
    .await?;
    anyhow::ensure!(
        updated.rows_affected() == 1,
        "no account {address:?} — add it first with: email-archiver add-account"
    );
    Ok(())
}

pub async fn set_user_password(pool: &PgPool, login: &str, password: &str) -> Result<()> {
    let hash = crate::secrets::hash_password(password)?;
    let updated = sqlx::query("UPDATE users SET password_hash = $2 WHERE login = $1")
        .bind(login)
        .bind(hash)
        .execute(pool)
        .await?;
    anyhow::ensure!(updated.rows_affected() == 1, "no such user {login:?}");
    Ok(())
}

/// Verify an IMAP login. Returns the user on success.
///
/// A user whose password_hash is the '!' placeholder can never authenticate,
/// which is what keeps a freshly created account from being reachable before a
/// password is deliberately set.
pub async fn authenticate(
    pool: &PgPool,
    login: &str,
    password: &str,
) -> Result<Option<(i64, String)>> {
    let row: Option<(i64, String, String)> =
        sqlx::query_as("SELECT id, bucket, password_hash FROM users WHERE login = $1")
            .bind(login)
            .fetch_optional(pool)
            .await?;

    Ok(match row {
        Some((id, bucket, hash)) if crate::secrets::verify_password(password, &hash) => {
            Some((id, bucket))
        }
        _ => None,
    })
}

/// Every folder the user can see, with message and unread counts.
///
/// Scoped by `accounts.user_id`, so a user cannot see another's folders even if
/// they guess ids. That scoping is in the SQL rather than in the handler
/// deliberately -- see WEBAPP-PLAN.md 4.4.
///
/// The counts are computed rather than cached. `placements` is keyed
/// `(folder_id, uid)`, so grouping by folder walks that index; if this ever
/// becomes slow at real volume, a cached count is a schema change to make with
/// evidence, not in advance.
pub async fn folders_for_user(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<(i64, String, String, Option<String>, i64, i64)>> {
    let rows: Vec<(i64, String, String, Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT f.id,
                a.label,
                f.name,
                a.hierarchy_delimiter,
                COUNT(p.uid),
                COUNT(p.uid) FILTER (WHERE NOT p.seen)
           FROM folders f
           JOIN accounts a ON a.id = f.account_id
           LEFT JOIN placements p ON p.folder_id = f.id
          WHERE a.user_id = $1
          GROUP BY f.id, a.label, f.name, a.hierarchy_delimiter
          ORDER BY a.label, f.name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// One page of a folder, newest first.
///
/// **Keyset pagination.** `cursor` is the `(internaldate, uid)` of the last row
/// already seen; rows strictly before it come next. `OFFSET` would re-walk
/// every skipped row, which at 53,000 messages makes late pages progressively
/// slower for no reason.
///
/// The `user_id` bind is what stops a guessed `folder_id` from reading someone
/// else's mail: the join to `accounts` makes ownership part of the query rather
/// than a check a handler might forget.
pub async fn messages_page(
    pool: &PgPool,
    user_id: i64,
    folder_id: i64,
    cursor: Option<(chrono::DateTime<chrono::Utc>, i64)>,
    limit: i64,
) -> Result<Vec<MessageRow>> {
    let (before_date, before_uid) = match cursor {
        Some((d, u)) => (Some(d), Some(u)),
        None => (None, None),
    };

    let rows: Vec<MessageRow> = sqlx::query_as(
        "SELECT p.uid,
                p.seen,
                m.blake3,
                m.subject,
                m.from_addr,
                -- The sender's display name, from the envelope already stored at
                -- ingest. No migration and no re-parse of 53,000 messages: the
                -- data was captured the first time round.
                m.envelope->'from'->0->>'name' AS from_name,
                m.internaldate,
                m.size,
                -- Likewise for the paperclip: bodystructure already records
                -- is_attachment per part.
                EXISTS (
                    SELECT 1 FROM jsonb_array_elements(m.bodystructure->'parts') part
                     WHERE (part->>'is_attachment')::boolean
                ) AS has_attachments
           FROM placements p
           JOIN messages m ON m.id = p.message_id
           JOIN folders  f ON f.id = p.folder_id
           JOIN accounts a ON a.id = f.account_id
          WHERE a.user_id = $1
            AND p.folder_id = $2
            AND ($3::timestamptz IS NULL
                 OR (m.internaldate, p.uid) < ($3, $4))
          ORDER BY m.internaldate DESC, p.uid DESC
          LIMIT $5",
    )
    .bind(user_id)
    .bind(folder_id)
    .bind(before_date)
    .bind(before_uid)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// A row of the message list. Ordering matches the SELECT above.
#[derive(sqlx::FromRow)]
pub struct MessageRow {
    pub uid: i64,
    pub seen: bool,
    pub blake3: String,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub internaldate: chrono::DateTime<chrono::Utc>,
    pub size: i64,
    pub has_attachments: bool,
}

/// Locate one message for a user, returning the bucket it lives in.
///
/// Does authorisation and lookup in a single query. The `blake3` is
/// user-supplied, so the `user_id` bind is what stops a guessed content address
/// from reading another person's mail -- messages are unique per user precisely
/// so this check is meaningful (see ARCHIVE-PLAN.md 2.3).
///
/// Returns the bucket rather than taking one, so no caller has to decide which
/// bucket a message belongs in and none can get it wrong.
pub async fn message_for_user(
    pool: &PgPool,
    user_id: i64,
    blake3: &str,
) -> Result<Option<(String, i64, chrono::DateTime<chrono::Utc>)>> {
    let row: Option<(String, i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT u.bucket, m.size, m.internaldate
           FROM messages m
           JOIN users u ON u.id = m.user_id
          WHERE m.user_id = $1 AND m.blake3 = $2",
    )
    .bind(user_id)
    .bind(blake3)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Set read state on one placement. Returns false if it is not this user's.
///
/// The scoping join is what makes a guessed folder/uid pair harmless: without
/// it, any authenticated user could flip read state on anyone's mail. Postgres
/// does not allow a JOIN in UPDATE directly, so ownership is a subquery.
pub async fn set_seen(
    pool: &PgPool,
    user_id: i64,
    folder_id: i64,
    uid: i64,
    seen: bool,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE placements SET seen = $4
          WHERE folder_id = $2
            AND uid = $3
            AND folder_id IN (
                SELECT f.id FROM folders f
                  JOIN accounts a ON a.id = f.account_id
                 WHERE a.user_id = $1
            )",
    )
    .bind(user_id)
    .bind(folder_id)
    .bind(uid)
    .bind(seen)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// One user by id, for the web session endpoint.
///
/// Separate from `authenticate` because the caller already holds a verified
/// session and is asking "who is this", not "is this password right".
pub async fn user_by_id(pool: &PgPool, user_id: i64) -> Result<Option<(String, String)>> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT login, display_name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;

    // display_name is nullable; falling back to the login keeps the API's shape
    // stable so the client never has to handle a missing name.
    Ok(row.map(|(login, display)| {
        let display = display.unwrap_or_else(|| login.clone());
        (login, display)
    }))
}

pub async fn set_hierarchy_delimiter(
    pool: &PgPool,
    account_id: i64,
    delimiter: Option<char>,
) -> Result<()> {
    sqlx::query("UPDATE accounts SET hierarchy_delimiter = $2 WHERE id = $1")
        .bind(account_id)
        .bind(delimiter.map(|c| c.to_string()))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_headers(pool: &PgPool, user_id: i64, blake3: &str, headers: &[u8]) -> Result<()> {
    sqlx::query("UPDATE messages SET headers = $3 WHERE user_id = $1 AND blake3 = $2")
        .bind(user_id)
        .bind(blake3)
        .bind(headers)
        .execute(pool)
        .await?;
    Ok(())
}
