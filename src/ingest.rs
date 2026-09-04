//! Ingest: pull mail from a source mailbox into the archive.
//!
//! Order of operations per message, and why it matters:
//!
//! 1. write the raw message to S3 (content-addressed)
//! 2. write its manifest to S3 (keyed by account + folder + uid)
//! 3. insert the index rows into Postgres
//! 4. advance the folder's resume marker
//!
//! A crash anywhere leaves the archive consistent-enough to resume: an orphan
//! blob or manifest is harmless and overwritten on the next pass, because the
//! key is derived from the content. The resume marker only advances after the
//! database row exists, so a message is never skipped — at worst it is
//! re-fetched and deduplicated.
//!
//! Within a batch, messages are processed concurrently, so the UIDs we assign
//! are in roughly — not exactly — source order. That is acceptable for an
//! archive: clients sort by date, and every UID is assigned before any client
//! sees the folder. It would matter for a live mailbox receiving mail during a
//! sync, which this is not.
//!
//! Resumability is not a nicety here. The largest mailbox is ~15 GB, which is a
//! multi-day pull that *will* be interrupted.

use anyhow::{Context, Result};
use chrono::Utc;
use futures::stream::{self, StreamExt};
use sqlx::PgPool;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
};
use tokio_rustls::TlsConnector;

use crate::config::{Config, Source};
use crate::db;
use crate::envelope;
use crate::store::{Manifest, Store};

/// How many messages to request per FETCH. Large enough to amortise round
/// trips, small enough that an interruption loses little work.
const BATCH: usize = 200;

/// Attempts per retryable operation before giving up.
const MAX_ATTEMPTS: u32 = 5;

/// First backoff delay; doubles each attempt.
const BACKOFF_BASE: Duration = Duration::from_secs(2);

/// Retry a fallible async operation with exponential backoff.
///
/// Every operation wrapped in this is idempotent: message keys are content-
/// derived, the message insert is ON CONFLICT, placement checks for an existing
/// row, and manifests overwrite. Retrying can therefore repeat work but cannot
/// duplicate or corrupt it.
async fn with_retry<T, F, Fut>(what: &str, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = BACKOFF_BASE;
    for attempt in 1..=MAX_ATTEMPTS {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_ATTEMPTS => {
                eprintln!(
                    "  {what}: attempt {attempt}/{MAX_ATTEMPTS} failed ({e});                      retrying in {}s",
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            Err(e) => return Err(e.context(format!("{what} failed after {MAX_ATTEMPTS} attempts"))),
        }
    }
    unreachable!()
}

/// Minimum gap between progress lines. Frequent enough to show the process is
/// alive on a slow mailbox, rare enough not to drown a fast one.
const PROGRESS_EVERY: Duration = Duration::from_secs(2);

/// Print immediately rather than when the buffer happens to fill. Rust block-
/// buffers stdout when it is not a terminal, so without this a piped or
/// redirected run shows nothing for minutes — exactly when progress matters.
fn progress(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}

pub type Session = async_imap::Session<tokio_rustls::client::TlsStream<TcpStream>>;

/// Accepts any server certificate.
///
/// Signature verification is still delegated to the crypto provider, so the
/// handshake itself remains sound — what is skipped is deciding whether the
/// certificate belongs to the host we meant to reach. Used only where a source
/// sets `allow_invalid_certs`.
#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    // Handshake signatures are accepted too, not just the certificate.
    //
    // This is not an additional concession in practice: once any certificate is
    // accepted, an attacker simply presents their own certificate and its
    // matching key, and the signature verifies perfectly. Checking it while
    // ignoring who the certificate belongs to protects against nothing.
    //
    // It is also what makes old servers reachable at all — a host whose
    // certificate expired years ago tends to negotiate signature schemes that
    // current providers no longer verify, which surfaces as BadSignature.
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    // Advertise everything the provider knows, so an old server is not refused
    // for offering a scheme we would have accepted anyway.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub async fn connect_session(source: &Source) -> Result<Session> {
    connect(source).await
}

/// TLS settings for one source, including its certificate policy.
///
/// Shared with `probe` so the diagnostic connects exactly as ingest does. A
/// probe that trusted different certificates could succeed where the real thing
/// fails, which is worse than no probe.
pub fn tls_config_for(source: &Source) -> Result<ClientConfig> {
    let tls_config = if source.allow_invalid_certs {
        // Loud, and on stderr: a run that silently stopped authenticating the
        // server should not look like a normal one in a log read months later.
        eprintln!(
            "  WARNING: certificate verification DISABLED for {} — encrypted, but the server is NOT authenticated",
            source.host
        );
        let provider = CryptoProvider::get_default()
            .context("no rustls crypto provider installed")?
            .clone();
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth()
    } else {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };

    Ok(tls_config)
}

/// Every network step below has a deadline.
///
/// None of them did, and a Gmail handshake that stalled mid-SASL hung an entire
/// import with no output: the connection was established, the server said
/// nothing, and both sides waited indefinitely. A source that stops answering
/// must fail rather than wait.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

async fn connect(source: &Source) -> Result<Session> {
    let tls_config = tls_config_for(source)?;
    let connector = TlsConnector::from(Arc::new(tls_config));

    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((source.host.as_str(), source.port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out connecting to {}:{}", source.host, source.port))?
    .with_context(|| format!("connecting to {}:{}", source.host, source.port))?;

    let domain = ServerName::try_from(source.host.clone())
        .with_context(|| format!("invalid hostname {:?}", source.host))?;
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(domain, tcp))
        .await
        .map_err(|_| anyhow::anyhow!("timed out in the TLS handshake with {}", source.host))?
        .context("TLS handshake failed")?;

    let mut client = async_imap::Client::new(tls);

    // THE GREETING MUST BE READ BEFORE AUTHENTICATING. async-imap's own docs say
    // so, and skipping it is survivable with LOGIN purely by accident:
    // check_done_ok_from skips the unread greeting and finds the tagged reply.
    //
    // AUTHENTICATE is not so lucky. The greeting is not a continuation, so it
    // takes the fall-through arm of the SASL loop, which hands off to
    // check_done_ok_from -- and that function understands only tagged replies.
    // It then swallows the server's `+` continuation while waiting for a tag,
    // and the server waits for a credential that will never be sent. Both
    // sides wait forever.
    //
    // That is what hung the Gmail import: nothing to do with the token, the
    // scope, or the delegation, all of which were correct.
    client
        .read_response()
        .await
        .transpose()
        .context("reading the server greeting")?
        .context("server closed the connection before sending a greeting")?;

    // The authentication exchange gets its own, longer deadline: this is where
    // the Gmail stall happened -- inside SASL, not at connect time.
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async move {
    match &source.auth {
        crate::config::Auth::Password(password) => client
            .login(&source.username, password)
            .await
            .map_err(|(e, _)| e)
            .with_context(|| format!("logging in as {}", source.username)),

        // Google Workspace. The token was minted for this exact mailbox by a
        // service account with domain-wide delegation; see src/gmail.rs.
        crate::config::Auth::XOAuth2 { token } => {
            let auth = crate::gmail::XOAuth2::new(source.username.clone(), token.clone());
            client
                .authenticate("XOAUTH2", auth)
                .await
                .map_err(|(e, _)| e)
                .with_context(|| {
                    format!(
                        "XOAUTH2 authentication as {} failed. If the token was minted but                          rejected, the service account most likely lacks domain-wide                          delegation for https://mail.google.com/ in the Admin console.",
                        source.username
                    )
                })
        }
    }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "authentication to {} did not finish within {}s -- the connection was open and              the server stopped answering. `email probe {}` shows the exchange.",
            source.host,
            HANDSHAKE_TIMEOUT.as_secs(),
            source.username
        )
    })?
}

/// Ingest every folder of one account.
/// Build the connection settings for one address.
///
/// Extracted so `probe` resolves a source exactly as ingest does -- the
/// provider decides how to authenticate, and a diagnostic that guessed
/// differently would be diagnosing something other than the real path.
pub async fn source_for_address(
    config: &Config,
    pool: &PgPool,
    address: &str,
) -> Result<crate::config::Source> {
    let owner = db::owner_of_address(pool, address).await?;
    let account = {
        let mut scope = db::Scope::begin(pool, owner).await?;
        db::account_by_address(&mut scope, address).await?
    };

    let source = match account.provider.as_str() {
        "gmail" => {
            // Keyed by domain: one service account is delegated for a whole
            // Workspace domain, so every mailbox in it shares the credential.
            let domain = address
                .rsplit_once('@')
                .map(|(_, d)| d)
                .with_context(|| format!("{address:?} has no domain"))?;

            let key = config.key()?;
            let key_json = db::google_domain(pool, &key, domain)
            .await?
            .with_context(|| {
                format!(
                    "no Google service account is configured for {domain}. Add one with: email set-google {domain} /path/to/service-account.json"
                )
            })?;

            let account = crate::gmail::ServiceAccount::parse(&key_json, domain)?;
            let tokens = crate::gmail::AccessTokens::new(account);
            let token = tokens.for_user(address).await?;
            crate::config::Source {
                host: "imap.gmail.com".to_string(),
                port: 993,
                username: address.to_string(),
                auth: crate::config::Auth::XOAuth2 { token },
                // Google's certificate is valid and this credential can read
                // every mailbox in the domain; there is no case for accepting
                // an unverified server here.
                allow_invalid_certs: false,
            }
        }
        _ => {
            let key = config.key()?;
            db::source_for(pool, &key, address).await?
        }
    };

    Ok(source)
}

pub async fn run(config: &Config, pool: &PgPool, address: &str) -> Result<()> {
    // accounts is policy-covered, so the owner has to be resolved first --
    // through `users`, which is not. See db::owner_of_address.
    let owner = db::owner_of_address(pool, address).await?;
    let account = {
        let mut scope = db::Scope::begin(pool, owner).await?;
        db::account_by_address(&mut scope, address).await?
    };
    // The provider decides how we authenticate, which is the whole reason that
    // column exists. Generic IMAP uses stored credentials; Google Workspace
    // mints a short-lived token from the service account instead, so those
    // accounts have no host or password recorded at all.
    let mut source = source_for_address(config, pool, address).await?;
    // Arc so every concurrent task shares one S3 client and its connection pool.
    let store = Arc::new(Store::open(config, &account.bucket).await?);

    println!("ingesting {address} -> bucket {}", account.bucket);

    let mut session = connect(&source).await?;

    // Collect names first: the session cannot be used for anything else while
    // the LIST stream is borrowed from it.
    let mut delimiter: Option<char> = None;
    let folders: Vec<String> = {
        let mut listing = session.list(Some(""), Some("*")).await?;
        let mut names = Vec::new();
        while let Some(item) = listing.next().await {
            let item = item?;
            // \Noselect entries are hierarchy placeholders, not mailboxes.
            if item
                .attributes()
                .iter()
                .any(|a| matches!(a, async_imap::types::NameAttribute::NoSelect))
            {
                continue;
            }
            // The source tells us its hierarchy separator; we previously
            // discarded it. Without it, presenting Archives.qra.2014 as a tree
            // would be guesswork — and wrong for servers that use "/".
            if delimiter.is_none() {
                delimiter = item.delimiter().and_then(|d| d.chars().next());
            }
            names.push(item.name().to_string());
        }
        names
    };

    {
        let mut scope = db::Scope::begin(pool, account.user_id).await?;
        db::set_hierarchy_delimiter(&mut scope, account.id, delimiter).await?;
        scope.commit().await?;
    }
    println!("  source hierarchy delimiter: {delimiter:?}");

    println!(
        "  {} selectable folders, concurrency {}",
        folders.len(),
        config.ingest.concurrency
    );

    let stored = AtomicUsize::new(0);
    for name in &folders {
        // Retry at folder granularity, reconnecting in between. A dropped
        // connection mid-folder is the common failure on a multi-hour run, and
        // resume state means a retry continues from the last completed batch
        // rather than starting the folder again.
        let mut attempt = 1;
        loop {
            match ingest_folder(
                pool,
                &store,
                &mut session,
                &account,
                name,
                config.ingest.concurrency,
                &stored,
            )
            .await
            {
                Ok(_) => break,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let delay = BACKOFF_BASE * 2u32.pow(attempt - 1);
                    // {e:#} for the whole chain. The bare {e} printed only
                    // the outermost context, which hid the server's own words --
                    // exactly what is needed when a source starts refusing us.
                    eprintln!(
                        "  {name}: failed ({e:#}); reconnecting and retrying in {}s (attempt {attempt}/{MAX_ATTEMPTS})",
                        delay.as_secs()
                    );
                    tokio::time::sleep(delay).await;

                    // Re-resolve the source instead of reusing it. A Google
                    // access token is minted once, up front, and lives an hour;
                    // any import long enough to need a reconnect is long enough
                    // to have outlived it, so every late reconnect failed to
                    // authenticate with a stale token -- while blaming
                    // domain-wide delegation, which was never the problem.
                    match source_for_address(config, pool, address).await {
                        Ok(fresh) => source = fresh,
                        Err(refresh_err) => {
                            eprintln!("  could not refresh credentials: {refresh_err:#}");
                        }
                    }

                    // The old session is probably dead; a fresh one is cheaper
                    // than guessing which parts of it still work.
                    session = match connect(&source).await {
                        Ok(s) => s,
                        Err(reconnect_err) => {
                            eprintln!("  reconnect failed: {reconnect_err:#}");
                            attempt += 1;
                            continue;
                        }
                    };
                    attempt += 1;
                }
                Err(e) => return Err(e.context(format!("folder {name}"))),
            }
        }
    }

    session.logout().await.ok();
    println!(
        "ingest complete: {} new messages",
        stored.load(Ordering::Relaxed)
    );
    Ok(())
}

async fn ingest_folder(
    pool: &PgPool,
    store: &Arc<Store>,
    session: &mut Session,
    account: &db::Account,
    name: &str,
    concurrency: usize,
    // Bumped as each message lands, so the run's total survives a folder that
    // fails partway. The old count came from this function's return value and
    // was therefore lost entirely when it returned Err -- a run that stored
    // 17,541 messages and then dropped its connection reported zero.
    stored: &AtomicUsize,
) -> Result<usize> {
    // EXAMINE, not SELECT: read-only on the source. Ingest must never mark the
    // user's live mail as read, or otherwise alter the mailbox it is copying.
    // This used to log and return Ok(0), which was wrong in the worst way: a
    // dead connection makes EVERY remaining folder unexaminable, so a run that
    // had actually collapsed reported "complete", counted zero new messages,
    // and exited 0. A folder that cannot be read is a failure. Returning Err
    // sends it to the retry loop, which reconnects -- and if it still cannot be
    // read, the run fails loudly instead of quietly claiming success.
    let mailbox = session
        .examine(name)
        .await
        .with_context(|| format!("examining {name}"))?;

    // UIDVALIDITY absent used to become 0, and 0 compares unequal to whatever
    // is on record, so the folder looked like it had been recreated and its
    // resume point was reset to zero -- discarding the record of a partially
    // completed folder on the strength of a value the server never sent.
    // RFC 3501 requires UIDVALIDITY in an EXAMINE response. Absent means
    // something is wrong, and the safe reading of "something is wrong" is to
    // stop, not to assume the mailbox is new.
    let uid_validity = mailbox.uid_validity.with_context(|| {
        format!("{name}: the server did not send UIDVALIDITY, so resume state cannot be trusted")
    })? as i64;
    let source_exists = mailbox.exists as i64;
    let folder = {
        let mut scope = db::Scope::begin(pool, account.user_id).await?;
        let folder = db::folder_for_ingest(&mut scope, account.id, name, uid_validity).await?;
        scope.commit().await?;
        folder
    };

    let range = format!("{}:*", folder.last_source_uid + 1);
    let mut new_messages = 0usize;

    // Searching a large mailbox is itself slow enough to look like a hang.
    progress(&format!("  {name}: searching for new messages"));

    // UID FETCH with BODY.PEEK[] — PEEK so the source's \Seen flags are not
    // modified by the act of archiving.
    let uids: Vec<u32> = {
        let mut search = session
            .uid_search(format!("UID {range}"))
            .await
            .with_context(|| format!("searching {name}"))?
            .into_iter()
            // The range cannot be trusted to exclude what we already have.
            // IMAP normalises a reversed range by swapping its endpoints, and
            // `*` means "highest UID" — so `3291:*` on a folder topping out at
            // 3290 returns 3290 rather than nothing. Without this filter every
            // re-run re-downloads the last message of every folder.
            .filter(|uid| *uid as i64 > folder.last_source_uid)
            .collect::<Vec<_>>();
        search.sort_unstable();
        search
    };

    // Drop UIDs we already hold, BEFORE fetching any bodies.
    //
    // The watermark filter above is not enough on its own. It is one mutable
    // number, and when it goes backwards -- a real UIDVALIDITY change, or the
    // unwrap_or(0) bug that used to invent one -- every message in the folder
    // looks new again. Content-hashed dedup then discards the duplicates, but
    // only after paying to download them, which is the expensive half.
    //
    // Gmail caps IMAP downloads at 2500 MB/day (Google Workspace admin docs,
    // "Gmail bandwidth limits"). Passing it suspends the account: typically an
    // hour, up to 24. So a needless re-fetch of a large folder is not merely
    // slow, it can cost access to the source outright.
    let uids = {
        let mut scope = db::Scope::begin(pool, account.user_id).await?;
        let held = db::archived_source_uids(&mut scope, folder.id).await?;
        scope.commit().await?;

        let before = uids.len();
        let remaining: Vec<u32> = uids
            .into_iter()
            .filter(|uid| !held.contains(&(*uid as i64)))
            .collect();
        let skipped = before - remaining.len();
        if skipped > 0 {
            progress(&format!(
                "  {name}: {skipped} already archived, not re-downloading"
            ));
        }
        remaining
    };

    // No early return when there is nothing to fetch: chunks() over an empty
    // slice yields no batches anyway, and returning here would skip the
    // completeness check below — which is precisely the check you want on a
    // re-run, when every folder finds nothing new.
    let total = uids.len();
    if total > 0 {
        progress(&format!("  {name}: {total} to fetch"));
    }
    let mut processed = 0usize;

    for chunk in uids.chunks(BATCH) {
        let set = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        progress(&format!(
            "    {name}: fetching {} messages ({}/{total})",
            chunk.len(),
            processed + chunk.len()
        ));

        let fetched: Vec<(u32, Vec<u8>, bool)> = {
            let mut stream = session
                .uid_fetch(&set, "(UID FLAGS BODY.PEEK[])")
                .await
                .with_context(|| format!("fetching {name} uids"))?;

            let mut out = Vec::new();
            while let Some(item) = stream.next().await {
                let item = item?;
                let Some(uid) = item.uid else { continue };
                let Some(body) = item.body() else { continue };
                let seen = item
                    .flags()
                    .any(|f| matches!(f, async_imap::types::Flag::Seen));
                out.push((uid, body.to_vec(), seen));
            }
            out
        };

        // The IMAP fetch above is necessarily serial — one session, one selected
        // mailbox — but everything after it is network round trips to S3 and
        // Postgres, which overlap happily.
        let fetched_len = fetched.len();
        let mut outcomes = stream::iter(fetched.into_iter().map(|(source_uid, raw, seen)| {
            let pool = pool.clone();
            let store = Arc::clone(store);
            let account = account.clone();
            let folder = folder.clone();
            let name = name.to_string();
            async move {
                // Retried individually: one S3 hiccup should cost a few
                // seconds, not the whole folder.
                with_retry(&format!("{name}/{source_uid}"), || {
                    store_message(
                        &pool, &store, &account, &folder, &name, source_uid, &raw, seen,
                    )
                })
                .await
            }
        }))
        .buffer_unordered(concurrency);

        // Consumed as results arrive rather than collected, so a slow batch
        // reports progress while it is still running.
        let mut done = 0usize;
        let mut last_tick = Instant::now();
        while let Some(outcome) = outcomes.next().await {
            if outcome? {
                new_messages += 1;
                stored.fetch_add(1, Ordering::Relaxed);
            }
            done += 1;
            if last_tick.elapsed() >= PROGRESS_EVERY && done < fetched_len {
                progress(&format!(
                    "    {name}: stored {}/{} of this batch, {new_messages} new so far",
                    done, fetched_len
                ));
                last_tick = Instant::now();
            }
        }

        // Advanced once per batch rather than per message: one round trip
        // instead of hundreds. Safe because it only moves after every message
        // in the batch has its database row — an interruption re-fetches the
        // batch, which is idempotent.
        let batch_max = chunk.iter().copied().max().unwrap_or(0);
        {
            let mut scope = db::Scope::begin(pool, account.user_id).await?;
            db::advance_source_uid(&mut scope, folder.id, batch_max as i64).await?;
            scope.commit().await?;
        }

        // Progress, not a summary: a large mailbox is many batches and this is
        // the only sign of life during a long run. Counts are cumulative for
        // the folder, so the last line doubles as the folder's total.
        processed += chunk.len();
        progress(&format!(
            "    {name}: {processed}/{total} done, {new_messages} new"
        ));
    }

    // Completeness against the source, which the consistency check cannot see:
    // it only compares our two stores to each other.
    //
    // Holding MORE than the source is normal and expected — the archive keeps
    // messages the source has since deleted. Holding FEWER means mail on the
    // server never made it here, which is the failure that matters.
    let held = {
        let mut scope = db::Scope::begin(pool, account.user_id).await?;
        db::count_placements(&mut scope, folder.id).await?
    };
    if held < source_exists {
        // Deliberately does not claim the mail is missing. A shortfall is
        // usually byte-identical duplicates collapsing into one placement,
        // which loses nothing — the first three cases investigated were all
        // duplicates. Claiming loss when there is none trains you to ignore
        // the warning, so it points at the tool that can tell the difference.
        eprintln!(
            "  NOTE {name}: source reports {source_exists}, archive holds {held} ({} fewer).              Usually duplicate copies collapsing into one message. Confirm with:              email-archiver diagnose <address> {name}",
            source_exists - held
        );
    } else {
        progress(&format!(
            "  {name}: {held} archived, source has {source_exists}"
        ));
    }

    Ok(new_messages)
}

/// Returns true if this produced a new placement.
#[allow(clippy::too_many_arguments)]
async fn store_message(
    pool: &PgPool,
    store: &Store,
    account: &db::Account,
    folder: &db::Folder,
    folder_name: &str,
    source_uid: u32,
    raw: &[u8],
    seen: bool,
) -> Result<bool> {
    let indexed = match envelope::index(raw, Utc::now()) {
        Ok(i) => i,
        Err(e) => {
            // Archive it anyway. A message we cannot parse is exactly the kind
            // we must not lose; it is still readable as raw bytes.
            eprintln!("  {folder_name}/{source_uid}: unparseable ({e}); archiving raw");
            envelope::Indexed {
                internaldate: Utc::now(),
                subject: None,
                from_addr: None,
                envelope: serde_json::json!({}),
                bodystructure: serde_json::json!({}),
            }
        }
    };

    // 1. message bytes, 2. manifest, 3. index rows. See module docs.
    let hash = store.put_message(raw).await?;

    // One scope for the message row and its placement, so both land or
    // neither does -- the same transaction boundary place_message used to open
    // for itself, now shared with the upsert.
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
        // Cached so the IMAP server can answer HEADER.FIELDS without a round
        // trip to S3 per message. See migration 0003.
        crate::fetch::split_header_body(raw).0,
    )
    .await?;

    let (uid, is_new) =
        db::place_message(&mut scope, folder.id, message_id, source_uid as i64, seen).await?;

    // Both rows committed together. Before this the upsert and the placement
    // were separate transactions, so an interruption between them left a
    // message row with no placement -- invisible to every client and
    // indistinguishable from mail that never arrived.
    scope.commit().await?;

    // Written unconditionally, even when the placement already existed. The
    // manifest is derived data; rewriting it is cheap and makes a re-ingest
    // repair manifests that are missing or were written under an older key
    // scheme.
    store
        .put_manifest(&Manifest {
            account: account.address.clone(),
            folder: folder_name.to_string(),
            uid,
            source_uid: source_uid as i64,
            internaldate: indexed.internaldate.to_rfc3339(),
            seen,
            blake3: hash,
            size: raw.len() as i64,
        })
        .await?;

    Ok(is_new)
}
