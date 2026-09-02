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

type Session = async_imap::Session<tokio_rustls::client::TlsStream<TcpStream>>;

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

async fn connect(source: &Source) -> Result<Session> {
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
    let connector = TlsConnector::from(Arc::new(tls_config));

    let tcp = TcpStream::connect((source.host.as_str(), source.port))
        .await
        .with_context(|| format!("connecting to {}:{}", source.host, source.port))?;

    let domain = ServerName::try_from(source.host.clone())
        .with_context(|| format!("invalid hostname {:?}", source.host))?;
    let tls = connector
        .connect(domain, tcp)
        .await
        .context("TLS handshake failed")?;

    let client = async_imap::Client::new(tls);
    client
        .login(&source.username, &source.password)
        .await
        .map_err(|(e, _)| e)
        .with_context(|| format!("logging in as {}", source.username))
}

/// Ingest every folder of one account.
pub async fn run(config: &Config, pool: &PgPool, address: &str) -> Result<()> {
    let account = db::account_by_address(pool, address).await?;
    let source = config.source(address)?;
    // Arc so every concurrent task shares one S3 client and its connection pool.
    let store = Arc::new(Store::open(config, &account.bucket).await?);

    println!("ingesting {address} -> bucket {}", account.bucket);

    let mut session = connect(source).await?;

    // Collect names first: the session cannot be used for anything else while
    // the LIST stream is borrowed from it.
    let folders: Vec<String> = {
        let mut listing = session.list(Some(""), Some("*")).await?;
        let mut names = Vec::new();
        while let Some(item) = listing.next().await {
            let item = item?;
            // \Noselect entries are hierarchy placeholders, not mailboxes.
            if item.attributes().iter().any(|a| {
                matches!(a, async_imap::types::NameAttribute::NoSelect)
            }) {
                continue;
            }
            names.push(item.name().to_string());
        }
        names
    };

    println!(
        "  {} selectable folders, concurrency {}",
        folders.len(),
        config.ingest.concurrency
    );

    let mut total = 0usize;
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
            )
            .await
            {
                Ok(n) => {
                    total += n;
                    break;
                }
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let delay = BACKOFF_BASE * 2u32.pow(attempt - 1);
                    eprintln!(
                        "  {name}: failed ({e}); reconnecting and retrying in {}s                          (attempt {attempt}/{MAX_ATTEMPTS})",
                        delay.as_secs()
                    );
                    tokio::time::sleep(delay).await;

                    // The old session is probably dead; a fresh one is cheaper
                    // than guessing which parts of it still work.
                    session = match connect(source).await {
                        Ok(s) => s,
                        Err(reconnect_err) => {
                            eprintln!("  reconnect failed: {reconnect_err}");
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
    println!("ingest complete: {total} new messages");
    Ok(())
}

async fn ingest_folder(
    pool: &PgPool,
    store: &Arc<Store>,
    session: &mut Session,
    account: &db::Account,
    name: &str,
    concurrency: usize,
) -> Result<usize> {
    // EXAMINE, not SELECT: read-only on the source. Ingest must never mark the
    // user's live mail as read, or otherwise alter the mailbox it is copying.
    let mailbox = match session.examine(name).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("  {name}: cannot examine ({e}); skipping");
            return Ok(0);
        }
    };

    let uid_validity = mailbox.uid_validity.unwrap_or(0) as i64;
    let source_exists = mailbox.exists as i64;
    let folder = db::folder_for_ingest(pool, account.id, name, uid_validity).await?;

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
            .collect::<Vec<_>>();
        search.sort_unstable();
        search
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
        let mut outcomes = stream::iter(fetched.into_iter().map(
            |(source_uid, raw, seen)| {
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
            },
        ))
        .buffer_unordered(concurrency);

        // Consumed as results arrive rather than collected, so a slow batch
        // reports progress while it is still running.
        let mut done = 0usize;
        let mut last_tick = Instant::now();
        while let Some(outcome) = outcomes.next().await {
            if outcome? {
                new_messages += 1;
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
        db::advance_source_uid(pool, folder.id, batch_max as i64).await?;

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
    let held = db::count_placements(pool, folder.id).await?;
    if held < source_exists {
        eprintln!(
            "  WARNING {name}: source reports {source_exists} messages, archive holds {held}              — {} message(s) on the server are not archived",
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

    let message_id = db::upsert_message(
        pool,
        account.user_id,
        &hash,
        raw.len() as i64,
        indexed.internaldate,
        indexed.subject.as_deref(),
        indexed.from_addr.as_deref(),
        &indexed.envelope,
        &indexed.bodystructure,
    )
    .await?;

    let (uid, is_new) = db::place_message(pool, folder.id, message_id, source_uid as i64, seen).await?;

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
