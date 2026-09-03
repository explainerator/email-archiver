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
    /// `gmail` or `imap`. Drives HOW ingest authenticates, not where mail is
    /// stored: Workspace accounts mint a token from the service account,
    /// generic IMAP uses the stored credentials.
    pub provider: String,
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

/// A database identity, and the transaction it is scoped to.
///
/// **This is the only way to read mail.** Once row-level security is enabled
/// (RLS-PLAN.md phases 3-4) every policy compares against
/// `archive.user_id`, and this type is what sets it.
///
/// Three properties, each deliberate:
///
/// * **The identity and the query's `user_id` are the same value.** Query
///   functions take a `Scope` rather than a `pool` and a separate id, so there
///   is no second argument that could disagree with the session setting.
/// * **`SET LOCAL`, never `SET`.** A plain `SET` persists on a pooled
///   connection, so the next request to borrow it would inherit this identity —
///   worse than no RLS at all, because it turns a scoping bug into a
///   timing-dependent cross-user leak. `SET LOCAL` unwinds when the transaction
///   ends, whether or not our code remembers.
/// * **Dropping is safe.** sqlx rolls back an uncommitted transaction on drop,
///   which both discards nothing (reads) and clears the setting.
pub struct Scope<'c> {
    tx: sqlx::Transaction<'c, sqlx::Postgres>,
    user_id: i64,
}

impl<'c> Scope<'c> {
    /// Open a transaction and declare who is asking.
    pub async fn begin(pool: &'c PgPool, user_id: i64) -> Result<Scope<'c>> {
        let mut tx = pool
            .begin()
            .await
            .context("beginning a scoped transaction")?;

        // `set_config(..., is_local => true)` rather than `SET LOCAL`, because
        // SET takes no bind parameters and would need the value formatted into
        // the statement. An i64 cannot carry SQL syntax so that would in fact be
        // safe, but "this particular interpolation happens to be fine" is a
        // rule that decays the moment someone adds a second one.
        sqlx::query("SELECT set_config('archive.user_id', $1, true)")
            .bind(user_id.to_string())
            .execute(&mut *tx)
            .await
            .context("setting the scoped user identity")?;

        Ok(Scope { tx, user_id })
    }

    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// The connection to run statements on.
    ///
    /// Exposed because several queries live outside this module — the IMAP
    /// server builds its own. They still go through a `Scope`, so they are
    /// covered by the same policy.
    pub fn conn(&mut self) -> &mut sqlx::PgConnection {
        &mut self.tx
    }

    /// Commit. Required for writes; reads may simply drop the scope.
    pub async fn commit(self) -> Result<()> {
        self.tx
            .commit()
            .await
            .context("committing a scoped transaction")?;
        Ok(())
    }
}

/// Is this login shaped like an email address?
///
/// Deliberately loose. Validating email properly means RFC 5322, which accepts
/// quoted strings, comments and address literals, and rejecting a real address
/// because our regex disagreed would be worse than accepting a typo. This only
/// catches the mistake it exists to catch: a bare name like `ken` where an
/// address belongs.
pub fn looks_like_email(login: &str) -> bool {
    let mut parts = login.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !login.chars().any(char::is_whitespace)
}

/// Open a transaction permitted to touch `google_domains`.
///
/// The table's policy is break-glass: readable only by a transaction that has
/// asked for it. Ingest asks; the IMAP and web servers never do, so a bug in
/// either cannot reach a credential that unlocks every mailbox in the domain.
///
/// Deliberately a separate function rather than a flag on `Scope`, so reaching
/// this data is a visible, greppable act rather than an argument someone could
/// pass without noticing.
async fn google_scope(pool: &PgPool) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('archive.google_access', 'yes', true)")
        .execute(&mut *tx)
        .await
        .context("requesting access to the Google credential table")?;
    Ok(tx)
}

/// Store a service account key for a Workspace domain, encrypted.
pub async fn set_google_domain(
    pool: &PgPool,
    key: &crate::secrets::SecretKey,
    domain: &str,
    client_email: &str,
    key_json: &str,
) -> Result<()> {
    let mut tx = google_scope(pool).await?;
    sqlx::query(
        "INSERT INTO google_domains (domain, client_email, key_enc)
         VALUES ($1, $2, $3)
         ON CONFLICT (domain)
             DO UPDATE SET client_email = EXCLUDED.client_email,
                           key_enc      = EXCLUDED.key_enc",
    )
    .bind(domain)
    .bind(client_email)
    .bind(key.encrypt(key_json)?)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("storing the service account key for {domain}"))?;
    tx.commit().await?;
    Ok(())
}

/// The decrypted service account key for a domain, if one is configured.
pub async fn google_domain(
    pool: &PgPool,
    key: &crate::secrets::SecretKey,
    domain: &str,
) -> Result<Option<String>> {
    let mut tx = google_scope(pool).await?;
    let row: Option<(String,)> =
        sqlx::query_as("SELECT key_enc FROM google_domains WHERE domain = $1")
            .bind(domain)
            .fetch_optional(&mut *tx)
            .await?;
    drop(tx);

    match row {
        Some((enc,)) => Ok(Some(key.decrypt(&enc)?)),
        None => Ok(None),
    }
}

pub async fn remove_google_domain(pool: &PgPool, domain: &str) -> Result<()> {
    let mut tx = google_scope(pool).await?;
    let affected = sqlx::query("DELETE FROM google_domains WHERE domain = $1")
        .bind(domain)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;

    anyhow::ensure!(affected == 1, "no Google domain {domain:?} is configured");
    Ok(())
}

/// Configured Workspace domains, for `sources`. Never returns the key itself.
pub async fn google_domains(pool: &PgPool) -> Result<Vec<(String, String)>> {
    let mut tx = google_scope(pool).await?;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT domain, client_email FROM google_domains ORDER BY domain")
            .fetch_all(&mut *tx)
            .await?;
    Ok(rows)
}

/// Turn certificate verification on or off for one source mailbox.
///
/// Separate from `set_source` so a boolean can be changed without re-entering a
/// password -- which would otherwise mean typing a credential to alter
/// something unrelated to it, and typing credentials needlessly is how they end
/// up somewhere they should not be.
pub async fn set_allow_invalid_certs(pool: &PgPool, address: &str, allow: bool) -> Result<()> {
    let owner = owner_of_address(pool, address).await?;
    let mut scope = Scope::begin(pool, owner).await?;

    let updated = sqlx::query("UPDATE accounts SET allow_invalid_certs = $2 WHERE address = $1")
        .bind(address)
        .bind(allow)
        .execute(scope.conn())
        .await?
        .rows_affected();

    // An UPDATE that matches nothing reports success, so this is checked rather
    // than assumed -- the same silence that hid the hierarchy-delimiter bug.
    anyhow::ensure!(updated == 1, "no account {address:?}");
    scope.commit().await?;
    Ok(())
}

/// Every archive user, for `users`.
///
/// Reads `users` and `user_logins`, neither of which carries a policy --
/// authentication has to resolve a login before there is an identity to scope
/// by. Message counts do need a scope, so they are fetched per user.
pub async fn all_users(pool: &PgPool) -> Result<Vec<UserSummary>> {
    let rows: Vec<(i64, String, Option<String>, String)> = sqlx::query_as(
        "SELECT u.id, l.login, u.display_name, u.bucket
           FROM users u
           JOIN user_logins l ON l.user_id = u.id AND l.is_primary
          ORDER BY l.login",
    )
    .fetch_all(pool)
    .await?;

    let mut users = Vec::new();
    for (id, login, display_name, bucket) in rows {
        let aliases: Vec<String> = sqlx::query_scalar(
            "SELECT login FROM user_logins WHERE user_id = $1 AND NOT is_primary ORDER BY login",
        )
        .bind(id)
        .fetch_all(pool)
        .await?;

        // Scoped, because messages is policy-covered. One extra round trip per
        // user on a handful of users.
        let mut scope = Scope::begin(pool, id).await?;
        let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
            .fetch_one(scope.conn())
            .await?;
        let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
            .fetch_one(scope.conn())
            .await?;
        drop(scope);

        users.push(UserSummary {
            login,
            display_name: display_name.unwrap_or_default(),
            bucket,
            aliases,
            accounts,
            messages,
        });
    }

    Ok(users)
}

pub struct UserSummary {
    pub login: String,
    pub display_name: String,
    pub bucket: String,
    pub aliases: Vec<String>,
    pub accounts: i64,
    pub messages: i64,
}

/// One user's source accounts, for `accounts <email>`.
pub async fn accounts_for_user(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<(String, String, String, Option<String>, bool, bool)>> {
    let mut scope = Scope::begin(pool, user_id).await?;
    let rows = sqlx::query_as(
        "SELECT address, label, provider, imap_host,
                (imap_host IS NOT NULL AND imap_password_enc IS NOT NULL),
                allow_invalid_certs
           FROM accounts ORDER BY address",
    )
    .fetch_all(scope.conn())
    .await?;

    Ok(rows)
}

/// Every account and how it authenticates, for `sources`.
///
/// Returns whether credentials are actually present rather than the
/// credentials themselves: the point is to show what is configured and what is
/// merely registered, not to hand back secrets.
pub async fn all_sources(
    pool: &PgPool,
) -> Result<Vec<(String, String, String, Option<String>, bool, bool)>> {
    let users: Vec<i64> = sqlx::query_scalar("SELECT id FROM users ORDER BY id")
        .fetch_all(pool)
        .await?;

    // accounts is policy-covered, so this walks the users rather than reading
    // the table directly -- the same bootstrap owner_of_address uses.
    let mut all = Vec::new();
    for user_id in users {
        let mut scope = Scope::begin(pool, user_id).await?;
        let rows: Vec<(String, String, String, Option<String>, bool, bool)> = sqlx::query_as(
            "SELECT address, label, provider, imap_host,
                    (imap_host IS NOT NULL AND imap_password_enc IS NOT NULL),
                    allow_invalid_certs
               FROM accounts ORDER BY address",
        )
        .fetch_all(scope.conn())
        .await?;
        drop(scope);
        all.extend(rows);
    }

    Ok(all)
}

/// The user a login belongs to, and their bucket.
///
/// The single place a login becomes a user. Every command that names an
/// address goes through here, so an alias works everywhere the primary does
/// rather than only at the login prompt -- which was the point of adding them.
pub async fn user_by_login(pool: &PgPool, login: &str) -> Result<Option<(i64, String)>> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT u.id, u.bucket
           FROM user_logins l
           JOIN users u ON u.id = l.user_id
          WHERE l.login = $1",
    )
    .bind(login)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Add another address the user may log in with.
pub async fn add_alias(pool: &PgPool, existing: &str, alias: &str) -> Result<i64> {
    anyhow::ensure!(
        looks_like_email(alias),
        "{alias:?} is not an email address. Logins are addresses."
    );

    let (user_id, _) = user_by_login(pool, existing)
        .await?
        .with_context(|| format!("no user with login {existing:?}"))?;

    // The primary key does the work: if this address is already anyone's login,
    // including this user's, the insert fails rather than silently moving it.
    sqlx::query("INSERT INTO user_logins (login, user_id, is_primary) VALUES ($1, $2, false)")
        .bind(alias)
        .bind(user_id)
        .execute(pool)
        .await
        .with_context(|| format!("{alias:?} is already a login, possibly for another user"))?;

    Ok(user_id)
}

/// Remove an alias. Refuses to remove a user's last or canonical address.
pub async fn remove_alias(pool: &PgPool, alias: &str) -> Result<()> {
    let row: Option<(i64, bool)> =
        sqlx::query_as("SELECT user_id, is_primary FROM user_logins WHERE login = $1")
            .bind(alias)
            .fetch_optional(pool)
            .await?;

    let (user_id, is_primary) = row.with_context(|| format!("{alias:?} is not a login"))?;

    // Removing the canonical address would leave the user with no name to
    // display and no obvious address to rename later. `rename-user` is the way
    // to change it.
    anyhow::ensure!(
        !is_primary,
        "{alias:?} is this user's primary address, not an alias. \
         Use `rename-user` to change it, or remove a different alias."
    );

    sqlx::query("DELETE FROM user_logins WHERE login = $1")
        .bind(alias)
        .execute(pool)
        .await?;

    let _ = user_id;
    Ok(())
}

/// Every login a user has, primary first.
pub async fn logins_for(pool: &PgPool, user_id: i64) -> Result<Vec<(String, bool)>> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT login, is_primary FROM user_logins
          WHERE user_id = $1 ORDER BY is_primary DESC, login",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Change a user's login.
///
/// Exists because the login is an email address now and the first two users
/// predate that decision. Nothing else references `users.login` -- every
/// foreign key uses `users.id` -- so this is genuinely just a rename, and read
/// state, buckets and archived mail are untouched.
pub async fn rename_user(pool: &PgPool, old: &str, new: &str) -> Result<()> {
    anyhow::ensure!(
        looks_like_email(new),
        "{new:?} is not an email address. The login IS the email address now."
    );

    // Renames the row itself rather than the primary flag, so aliases are
    // untouched: renaming the canonical address does not disturb the others.
    let affected = sqlx::query("UPDATE user_logins SET login = $2 WHERE login = $1")
        .bind(old)
        .bind(new)
        .execute(pool)
        .await
        .with_context(|| format!("renaming {old:?} to {new:?} (is {new:?} already taken?)"))?
        .rows_affected();

    anyhow::ensure!(affected == 1, "no user with login {old:?}");
    Ok(())
}

pub async fn create_user(pool: &PgPool, login: &str, bucket: &str, display: &str) -> Result<i64> {
    anyhow::ensure!(
        looks_like_email(login),
        "login {login:?} is not an email address. Logins are email addresses: one \n         thing to remember instead of two, and already unique."
    );

    // password_hash is a placeholder until IMAP auth lands in Phase 4. It is
    // deliberately not a valid hash, so nothing can authenticate as this user
    // by accident before the real credential is set.
    // The user row and its primary login go in together: a user with no login
    // could not be addressed by any command, and a login with no user is
    // rejected by the foreign key.
    let mut tx = pool.begin().await?;

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO users (password_hash, bucket, display_name)
         VALUES ('!', $1, $2)
         ON CONFLICT (bucket) DO UPDATE SET display_name = EXCLUDED.display_name
         RETURNING id",
    )
    .bind(bucket)
    .bind(display)
    .fetch_one(&mut *tx)
    .await
    .with_context(|| format!("creating user {login}"))?;

    sqlx::query(
        "INSERT INTO user_logins (login, user_id, is_primary)
         VALUES ($1, $2, true)
         ON CONFLICT (login) DO NOTHING",
    )
    .bind(login)
    .bind(id)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("{login:?} is already a login, possibly for another user"))?;

    tx.commit().await?;
    Ok(id)
}

pub async fn create_account(
    pool: &PgPool,
    login: &str,
    address: &str,
    label: &str,
    provider: &str,
) -> Result<i64> {
    let (user_id, _) = user_by_login(pool, login)
        .await?
        .with_context(|| format!("no such user {login:?} — create it first"))?;

    // The INSERT below is into a policy-covered table, so WITH CHECK requires a
    // declared identity that matches the row's owner -- the user just found.
    let mut scope = Scope::begin(pool, user_id).await?;

    let id: i64 = sqlx::query_scalar(
        // In a scope: the policy's WITH CHECK requires the new row's owner to
        // match the declared identity, which is the same user just looked up.
        "INSERT INTO accounts (user_id, address, label, provider)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (address) DO UPDATE SET label = EXCLUDED.label
         RETURNING id",
    )
    .bind(user_id)
    .bind(address)
    .bind(label)
    .bind(provider)
    .fetch_one(scope.conn())
    .await
    .with_context(|| format!("creating account {address}"))?;

    scope.commit().await?;
    Ok(id)
}

/// Which user owns an address, resolved without an identity.
///
/// The bootstrap for every CLI command that names an address:
/// `ingest`, `set-source`, `diagnose`. `accounts` is policy-covered, so it
/// cannot simply be read -- but `users` is not, because authentication has to
/// search it before anyone is logged in. So this walks the users and asks each
/// scope whether the address is theirs.
///
/// O(users) round trips, on a table with a handful of rows, once per CLI
/// invocation. That is a small price for not leaving encrypted source-mailbox
/// credentials readable by any unscoped query.
pub async fn owner_of_address(pool: &PgPool, address: &str) -> Result<i64> {
    let users: Vec<i64> = sqlx::query_scalar("SELECT id FROM users ORDER BY id")
        .fetch_all(pool)
        .await?;

    for user_id in users {
        let mut scope = Scope::begin(pool, user_id).await?;
        let found: Option<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE address = $1")
            .bind(address)
            .fetch_optional(scope.conn())
            .await?;
        if found.is_some() {
            return Ok(user_id);
        }
    }

    anyhow::bail!("no account for address {address:?}")
}

pub async fn account_by_address(scope: &mut Scope<'_>, address: &str) -> Result<Account> {
    let row: (i64, i64, String, String, String, String) = sqlx::query_as(
        "SELECT a.id, a.user_id, a.address, a.label, u.bucket, a.provider
         FROM accounts a JOIN users u ON u.id = a.user_id
         WHERE a.address = $1",
    )
    .bind(address)
    .fetch_optional(scope.conn())
    .await?
    .with_context(|| format!("no account {address:?} in the database — add it first"))?;

    Ok(Account {
        id: row.0,
        user_id: row.1,
        address: row.2,
        label: row.3,
        bucket: row.4,
        provider: row.5,
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
    scope: &mut Scope<'_>,
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
    .fetch_optional(scope.conn())
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
            .execute(scope.conn())
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
        // user_id is SELECTed from the account rather than passed in. A caller
        // cannot supply the wrong one because a caller does not supply it at
        // all -- the value comes from the row it must agree with.
        "INSERT INTO folders (account_id, name, uidvalidity, uidnext, source_uidvalidity, last_source_uid, user_id)
         SELECT $1, $2, $3, 1, $4, 0, a.user_id FROM accounts a WHERE a.id = $1
         RETURNING id",
    )
    .bind(account_id)
    .bind(name)
    .bind(uidvalidity)
    .bind(source_uidvalidity)
    .fetch_one(scope.conn())
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
    scope: &mut Scope<'_>,
    blake3: &str,
    size: i64,
    internaldate: DateTime<Utc>,
    subject: Option<&str>,
    from_addr: Option<&str>,
    envelope: &serde_json::Value,
    bodystructure: &serde_json::Value,
    headers: &[u8],
) -> Result<i64> {
    let user_id = scope.user_id();
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
    .fetch_one(scope.conn())
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
    scope: &mut Scope<'_>,
    folder_id: i64,
    message_id: i64,
    source_uid: i64,
    seen: bool,
) -> Result<(i64, bool)> {
    let already: Option<i64> =
        sqlx::query_scalar("SELECT uid FROM placements WHERE folder_id = $1 AND message_id = $2")
            .bind(folder_id)
            .bind(message_id)
            .fetch_optional(scope.conn())
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
        .execute(scope.conn())
        .await?;
        return Ok((uid, false));
    }

    // Lock the folder row so two ingest passes cannot hand out the same UID.
    let uid: i64 = sqlx::query_scalar(
        "UPDATE folders SET uidnext = uidnext + 1 WHERE id = $1 RETURNING uidnext - 1",
    )
    .bind(folder_id)
    .fetch_one(scope.conn())
    .await?;

    let inserted = sqlx::query(
        // Likewise derived, this time from the folder. The composite foreign
        // key would reject a mismatch anyway; taking the value from the same
        // row means it never has the chance to be wrong.
        "INSERT INTO placements (folder_id, uid, message_id, source_uid, seen, user_id)
         SELECT $1, $2, $3, $4, $5, f.user_id FROM folders f WHERE f.id = $1",
    )
    .bind(folder_id)
    .bind(uid)
    .bind(message_id)
    .bind(source_uid)
    .bind(seen)
    .execute(scope.conn())
    .await?;

    // INSERT ... SELECT inserts NOTHING when the SELECT matches nothing, where
    // the previous VALUES form would have raised a foreign-key violation. That
    // silence is the whole danger of the rewrite: this function would report a
    // UID it had handed out for a placement that was never stored, and ingest
    // would record the message as archived. Checked rather than assumed.
    anyhow::ensure!(
        inserted.rows_affected() == 1,
        "placement for folder {folder_id} inserted {} rows; the folder disappeared mid-transaction",
        inserted.rows_affected()
    );

    Ok((uid, true))
}

/// Record ingest progress. Only ever moves forward.
pub async fn advance_source_uid(
    scope: &mut Scope<'_>,
    folder_id: i64,
    source_uid: i64,
) -> Result<()> {
    sqlx::query("UPDATE folders SET last_source_uid = GREATEST(last_source_uid, $2) WHERE id = $1")
        .bind(folder_id)
        .bind(source_uid)
        .execute(scope.conn())
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

pub async fn placements_for_user(scope: &mut Scope<'_>) -> Result<Vec<PlacementRow>> {
    let user_id = scope.user_id();
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
    .fetch_all(scope.conn())
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
pub async fn count_placements(scope: &mut Scope<'_>, folder_id: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM placements WHERE folder_id = $1")
            .bind(folder_id)
            .fetch_one(scope.conn())
            .await?,
    )
}

/// Messages for a user with no cached header block yet.
pub async fn messages_missing_headers(scope: &mut Scope<'_>) -> Result<Vec<String>> {
    let user_id = scope.user_id();
    Ok(
        sqlx::query_scalar("SELECT blake3 FROM messages WHERE user_id = $1 AND headers IS NULL")
            .bind(user_id)
            .fetch_all(scope.conn())
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
    // accounts carries a policy since migration 0009 -- it holds these very
    // credentials, decryptable with a key the application already has. So the
    // read must declare an identity, and the owner is resolved the same way
    // every other address-named command resolves it.
    let owner = owner_of_address(pool, address).await?;
    let mut scope = Scope::begin(pool, owner).await?;

    let row: Option<(Option<String>, i32, Option<String>, Option<String>, bool)> = sqlx::query_as(
        "SELECT imap_host, imap_port, imap_username, imap_password_enc, allow_invalid_certs
         FROM accounts WHERE address = $1",
    )
    .bind(address)
    .fetch_optional(scope.conn())
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
        auth: crate::config::Auth::Password(key.decrypt(&password_enc)?),
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
    // Writing credentials into a policy-covered table: the identity must be
    // declared, and it must be the address's real owner or WITH CHECK rejects
    // the update.
    let owner = owner_of_address(pool, address).await?;
    let mut scope = Scope::begin(pool, owner).await?;

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
    .execute(scope.conn())
    .await?;
    anyhow::ensure!(
        updated.rows_affected() == 1,
        "no account {address:?} — add it first with: email-archiver add-account"
    );
    scope.commit().await?;
    Ok(())
}

pub async fn set_user_password(pool: &PgPool, login: &str, password: &str) -> Result<()> {
    let hash = crate::secrets::hash_password(password)?;

    // Through user_logins since migration 0011 -- users.login no longer exists,
    // and this is why an alias sets the password for the same person the
    // primary address does. There is one password per USER, not per address.
    let updated = sqlx::query(
        "UPDATE users SET password_hash = $2
          WHERE id = (SELECT user_id FROM user_logins WHERE login = $1)",
    )
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
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT u.id, u.bucket, u.password_hash
               FROM user_logins l
               JOIN users u ON u.id = l.user_id
              WHERE l.login = $1",
    )
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
    scope: &mut Scope<'_>,
) -> Result<Vec<(i64, String, String, Option<String>, i64, i64)>> {
    let user_id = scope.user_id();
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
    .fetch_all(scope.conn())
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
    scope: &mut Scope<'_>,
    folder_id: i64,
    cursor: Option<(chrono::DateTime<chrono::Utc>, i64)>,
    limit: i64,
) -> Result<Vec<MessageRow>> {
    let user_id = scope.user_id();
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
    .fetch_all(scope.conn())
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
    scope: &mut Scope<'_>,
    blake3: &str,
) -> Result<Option<(String, i64, chrono::DateTime<chrono::Utc>)>> {
    let user_id = scope.user_id();
    let row: Option<(String, i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT u.bucket, m.size, m.internaldate
           FROM messages m
           JOIN users u ON u.id = m.user_id
          WHERE m.user_id = $1 AND m.blake3 = $2",
    )
    .bind(user_id)
    .bind(blake3)
    .fetch_optional(scope.conn())
    .await?;

    Ok(row)
}

/// Set read state on one placement. Returns false if it is not this user's.
///
/// The scoping join is what makes a guessed folder/uid pair harmless: without
/// it, any authenticated user could flip read state on anyone's mail. Postgres
/// does not allow a JOIN in UPDATE directly, so ownership is a subquery.
pub async fn set_seen(scope: &mut Scope<'_>, folder_id: i64, uid: i64, seen: bool) -> Result<bool> {
    let user_id = scope.user_id();
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
    .execute(scope.conn())
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Substring search over subject and sender, newest first.
///
/// **Unindexed by design, for now.** WEBAPP-PLAN.md 8 planned a `pg_trgm` GIN
/// index; that extension cannot be created on this database -- the `gern` role
/// is not a superuser and `CREATE EXTENSION` is refused -- and a migration that
/// fails would stop the archiver starting at all, because migrations run on
/// every command. Measured instead: over 151,518 messages a page costs roughly
/// 570 ms of query time. Usable, and it degrades linearly, so if it becomes
/// annoying the fix is enabling the extension on the database rather than
/// changing this code.
///
/// Scoped by `user_id` like every other read here, so search can never reach
/// another person's mail.
pub async fn search(
    scope: &mut Scope<'_>,
    query: &str,
    folder_id: Option<i64>,
    cursor: Option<(chrono::DateTime<chrono::Utc>, i64)>,
    limit: i64,
) -> Result<Vec<SearchRow>> {
    let user_id = scope.user_id();
    // The caller supplies a substring, not a pattern: % and _ are wildcards in
    // LIKE, so a query containing them would silently mean something other than
    // what was typed.
    // The caller supplies a substring, not a pattern: % and _ are wildcards
    // in LIKE, so a query containing either would silently mean something
    // other than what was typed. '!' is the escape character rather than the
    // usual backslash purely because it needs no escaping in a Rust literal,
    // an SQL literal, or anything in between.
    let escaped = query
        .replace(char::from(33), "!!")
        .replace(char::from(37), "!%")
        .replace(char::from(95), "!_");
    let pattern = format!("%{escaped}%");

    let (before_date, before_uid) = match cursor {
        Some((d, u)) => (Some(d), Some(u)),
        None => (None, None),
    };

    // WHY A CTE. Written as a plain join, Postgres drives from `placements` --
    // 152,741 primary-key lookups into `messages`, testing the text predicate on
    // each. Measured at 610 ms. Filtering `messages` first instead, which
    // MATERIALIZED forces by stopping the CTE being inlined back into the join,
    // scans 151,518 rows once and hash-joins the ~3,000 survivors: 266 ms.
    //
    // The planner will not choose this on its own. Its estimate for the join
    // path is rows=19 against an actual 3,139, so it believes the nested loop is
    // nearly free. That is why the shape is pinned here rather than left open.
    //
    // No index is involved, and adding one would not have helped: a btree cannot
    // serve a leading-wildcard LIKE, and a GIN full-text index measured SLOWER
    // (1.2 s) because the planner still drove from placements and recomputed
    // to_tsvector per row. See WEBAPP-PLAN.md 8.
    let rows: Vec<SearchRow> = sqlx::query_as(
        "WITH matched AS MATERIALIZED (
             SELECT id,
                    blake3,
                    subject,
                    from_addr,
                    envelope->'from'->0->>'name' AS from_name,
                    internaldate,
                    size,
                    EXISTS (
                        SELECT 1 FROM jsonb_array_elements(bodystructure->'parts') part
                         WHERE (part->>'is_attachment')::boolean
                    ) AS has_attachments
               FROM messages
              WHERE user_id = $1
                AND (subject ILIKE $3 ESCAPE '!' OR from_addr ILIKE $3 ESCAPE '!')
         )
         SELECT p.uid,
                p.seen,
                m.blake3,
                m.subject,
                m.from_addr,
                m.from_name,
                m.internaldate,
                m.size,
                m.has_attachments,
                -- Aliased to match SearchRow's field names. sqlx::FromRow
                -- binds by COLUMN NAME, so an unaliased f.id arrives as id
                -- and fails to map onto folder_id -- at runtime, not at
                -- compile time, which is how this shipped broken once.
                f.id                   AS folder_id,
                a.label                AS account_label,
                f.name                 AS folder_name,
                a.hierarchy_delimiter  AS hierarchy_delimiter
           FROM matched m
           JOIN placements p ON p.message_id = m.id
           JOIN folders    f ON f.id = p.folder_id
           JOIN accounts   a ON a.id = f.account_id
          WHERE a.user_id = $1
            AND ($2::bigint IS NULL OR p.folder_id = $2)
            AND ($4::timestamptz IS NULL OR (m.internaldate, p.uid) < ($4, $5))
          ORDER BY m.internaldate DESC, p.uid DESC
          LIMIT $6",
    )
    .bind(user_id)
    .bind(folder_id)
    .bind(&pattern)
    .bind(before_date)
    .bind(before_uid)
    .bind(limit)
    .fetch_all(scope.conn())
    .await?;

    Ok(rows)
}

/// A search result row. Extends `MessageRow` with where the message lives.
#[derive(sqlx::FromRow)]
pub struct SearchRow {
    pub uid: i64,
    pub seen: bool,
    pub blake3: String,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub internaldate: chrono::DateTime<chrono::Utc>,
    pub size: i64,
    pub has_attachments: bool,
    pub folder_id: i64,
    pub account_label: String,
    pub folder_name: String,
    pub hierarchy_delimiter: Option<String>,
}

/// One user by id, for the web session endpoint.
///
/// Separate from `authenticate` because the caller already holds a verified
/// session and is asking "who is this", not "is this password right".
pub async fn user_by_id(scope: &mut Scope<'_>) -> Result<Option<(String, String)>> {
    let user_id = scope.user_id();
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT l.login, u.display_name
               FROM users u
               JOIN user_logins l ON l.user_id = u.id AND l.is_primary
              WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(scope.conn())
    .await?;

    // display_name is nullable; falling back to the login keeps the API's shape
    // stable so the client never has to handle a missing name.
    Ok(row.map(|(login, display)| {
        let display = display.unwrap_or_else(|| login.clone());
        (login, display)
    }))
}

pub async fn set_hierarchy_delimiter(
    scope: &mut Scope<'_>,
    account_id: i64,
    delimiter: Option<char>,
) -> Result<()> {
    sqlx::query("UPDATE accounts SET hierarchy_delimiter = $2 WHERE id = $1")
        .bind(account_id)
        .bind(delimiter.map(|c| c.to_string()))
        .execute(scope.conn())
        .await?;
    Ok(())
}

pub async fn set_headers(scope: &mut Scope<'_>, blake3: &str, headers: &[u8]) -> Result<()> {
    let user_id = scope.user_id();
    sqlx::query("UPDATE messages SET headers = $3 WHERE user_id = $1 AND blake3 = $2")
        .bind(user_id)
        .bind(blake3)
        .bind(headers)
        .execute(scope.conn())
        .await?;
    Ok(())
}

#[cfg(test)]
mod login_tests {
    use super::looks_like_email;

    #[test]
    fn accepts_real_addresses() {
        for good in [
            "ken@twoducks.ca",
            "art@jduck.ca",
            "first-last@example.co.uk",
            "a.b+tag@sub.example.com",
            "info@kenduck.ca",
        ] {
            assert!(looks_like_email(good), "rejected {good}");
        }
    }

    #[test]
    fn rejects_the_mistake_it_exists_for() {
        // A bare name where an address belongs -- what the first two users had.
        for bad in [
            "ken",
            "jaqui",
            "",
            "@",
            "ken@",
            "@twoducks.ca",
            "ken@localhost",
        ] {
            assert!(!looks_like_email(bad), "accepted {bad}");
        }
    }

    #[test]
    fn rejects_things_that_would_break_a_login_field() {
        for bad in [
            "two@at@signs.com",
            "ken @twoducks.ca",
            "ken@two ducks.ca",
            "ken@.ca",
            "ken@ca.",
        ] {
            assert!(!looks_like_email(bad), "accepted {bad}");
        }
    }
}
