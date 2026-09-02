//! Read-only HTTP API and static host for the browser client.
//!
//! See WEBAPP-PLAN.md. The short version of the design decision that shapes
//! this file: **the browser cannot speak IMAP**, so the web client does not go
//! through `server.rs`. It reads Postgres and S3 directly through the same
//! `db`, `store` and `fetch` modules the IMAP server uses. Routing a browser
//! request through our own IMAP server would serialise the archive into IMAP
//! wire format purely to parse it back.
//!
//! Local development runs plaintext on `127.0.0.1:8000`; production runs TLS on
//! `0.0.0.0:443`. Which of those happens is decided by `listen::resolve`, so
//! the rule is shared with the IMAP listener rather than written twice.
//!
//! **Phases 1-4a**: the listener, health, static assets, authentication, the
//! folder list, paged message lists, and plain-text message bodies. Sanitised
//! HTML (4b) and attachment downloads (5) are still to come.
//!
//! Every endpoint that touches mail takes a [`UserScope`], which cannot be
//! constructed without a valid session, and passes its id to a query that binds
//! it. Scoping is therefore structural rather than remembered — see
//! WEBAPP-PLAN.md 4.4.

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::db;
use crate::listen;
use crate::naming;
use crate::ratelimit::Limiter;
use crate::sanitise;
use crate::session::{self, UserScope};
use crate::store::Store;

/// Everything a handler needs, cloned per request.
#[derive(Clone)]
pub struct AppState {
    #[allow(dead_code)] // Phase 3 onward: S3 credentials for message bodies.
    pub config: Arc<Config>,
    pub pool: PgPool,
    /// Derived from `config.encryption_key`; see `session::derive_signing_key`.
    pub session_key: [u8; 32],
    /// Whether cookies carry `Secure`. Follows the listener, not a constant.
    pub secure_cookies: bool,
    pub limiter: Arc<Limiter>,
    /// S3 clients, one per bucket, built on first use.
    ///
    /// `Store::open` constructs an AWS SDK client, which is far too much work to
    /// repeat per request. The IMAP server caches one per connection; HTTP has
    /// no connection to hang it on, so it is cached here and shared.
    pub stores: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Store>>>,
}

impl AppState {
    /// The store for a bucket, opening and caching it if needed.
    async fn store(&self, bucket: &str) -> Result<Store> {
        let mut cache = self.stores.lock().await;
        if let Some(store) = cache.get(bucket) {
            return Ok(store.clone());
        }
        let store = Store::open(&self.config, bucket).await?;
        cache.insert(bucket.to_string(), store.clone());
        Ok(store)
    }
}

pub async fn run(
    config: &Arc<Config>,
    pool: &PgPool,
    bind: &str,
    allow_plaintext: bool,
    assets: Option<PathBuf>,
) -> Result<()> {
    // A session cookie is a bearer credential: anyone who reads it off the wire
    // is logged in as that user until it expires. That makes plaintext exactly
    // as unacceptable here as it is for IMAP LOGIN, so the same rule applies.
    //
    // AlwaysPlaintext on loopback because a certificate for
    // archive.thebackroom420.ca cannot validate for https://127.0.0.1 — see
    // listen::Loopback.
    let transport = listen::resolve(
        config,
        bind,
        allow_plaintext,
        "HTTP",
        "session cookies are bearer credentials and would cross the network in clear text",
        listen::Loopback::AlwaysPlaintext,
    )?;

    if transport.is_tls() {
        // Phase 7. Deliberately an error rather than a silent downgrade to
        // plaintext: a web server that quietly stopped using the certificate it
        // was configured with is precisely the failure this program should
        // never have.
        anyhow::bail!(
            "TLS for the web listener is not implemented yet (WEBAPP-PLAN.md phase 7).\n\
             Run it on loopback for now: email-archiver serve-web"
        );
    }

    let state = AppState {
        config: Arc::clone(config),
        pool: pool.clone(),
        session_key: session::derive_signing_key(&config.encryption_key),
        secure_cookies: transport.is_tls(),
        limiter: Arc::new(Limiter::new()),
        stores: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    };

    let app = router(state, assets.clone())?;

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;

    match &transport {
        listen::Transport::Plaintext { loopback: true } => {
            println!("web listening on http://{bind} (plaintext, loopback)")
        }
        listen::Transport::Plaintext { loopback: false } => {
            println!("web listening on http://{bind} (PLAINTEXT, NOT loopback)")
        }
        listen::Transport::Tls(_) => unreachable!("bailed above"),
    }
    match &assets {
        Some(dir) => println!("  serving assets from {}", dir.display()),
        None => println!("  no frontend built yet; API only. Try /api/health"),
    }

    // ConnectInfo carries the peer address, which the login throttle keys on.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serving HTTP")?;

    Ok(())
}

fn router(state: AppState, assets: Option<PathBuf>) -> Result<Router> {
    let api = Router::new()
        .route("/health", get(health))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/session", get(whoami))
        .route("/folders", get(folders))
        .route("/folders/{id}/messages", get(messages))
        .route("/messages/{blake3}", get(message))
        .route("/messages/{blake3}/inline/{cid}", get(inline_part))
        .route("/messages/{blake3}/parts/{index}", get(download_part))
        .route(
            "/placements/{folder_id}/{uid}",
            axum::routing::patch(set_seen),
        )
        // The API needs its OWN fallback. `nest` does not isolate 404s: an
        // unmatched path under /api falls through to the outer fallback, which
        // is the SPA shell -- so without this, a misspelled endpoint answers
        // 200 with HTML and a JSON client fails somewhere far from the cause.
        .fallback(api_not_found);

    let app = Router::new().nest("/api", api);

    // The frontend is a single-page app: the browser may deep-link to a route
    // the server knows nothing about, so anything not matched by a file falls
    // back to index.html and the client router takes it from there.
    //
    // The API is nested FIRST, so a missing endpoint returns 404 rather than
    // being answered with index.html -- a JSON client receiving HTML is a
    // confusing way to learn a route is misspelled.
    //
    // The fallback is a handler holding index.html in memory rather than
    // ServeDir's not_found_service, which returns the document with the 404
    // status that triggered it. A deep link is a real page, not a missing one:
    // a 404 there tells crawlers and monitoring the app is broken, and some
    // clients refuse to render the body at all.
    let app = match assets {
        Some(dir) => {
            let index = std::fs::read_to_string(dir.join("index.html")).with_context(|| {
                format!(
                    "reading {}. Build the frontend first: cd web-ui && dx build --platform web --release",
                    dir.join("index.html").display()
                )
            })?;
            app.fallback_service(
                tower_http::services::ServeDir::new(dir)
                    .fallback(get(move || spa_index(index.clone()))),
            )
        }
        None => app,
    };

    // The wasm bundle is ~2 MB and compresses to a fraction of that. Worth more
    // here than it would normally be: `dx`'s bundled wasm-opt crashes on
    // Windows (WEBAPP-PLAN.md 9.5), so the wasm we ship is unoptimised, and
    // gzip recovers most of what that costs over the wire.
    let app = app.layer(tower_http::compression::CompressionLayer::new());

    // Cheap headers that apply to everything. The message-body CSP is a
    // separate and much stricter policy applied to the reading frame in phase
    // 4b; this is only the shell.
    let app = app
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .with_state(state);

    Ok(app)
}

/// 404 for an unmatched /api path, in the same JSON shape as every other error
/// so a client has one thing to parse.
async fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "no such endpoint" })),
    )
        .into_response()
}

/// Serve the SPA shell for a client-side route.
///
/// 200, not 404: the URL is a real page that the client router will resolve.
async fn spa_index(index: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        index,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Liveness, and proof the database is actually reachable.
///
/// A health check that only proves the process is running would report healthy
/// while every real request failed on a dead pool, which is worse than no check
/// at all -- it is a check that lies. The query is trivial so this stays cheap
/// enough to poll.
///
/// Unauthenticated on purpose: it reveals nothing beyond "this process is up",
/// and a health check that needs credentials is one the deploy cannot use.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();

    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        // Health is a liveness signal, not a document; caching it would defeat
        // the point.
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "status": if db_ok { "ok" } else { "degraded" },
            "database": if db_ok { "up" } else { "down" },
        })),
    )
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginRequest {
    login: String,
    password: String,
}

#[derive(Serialize)]
struct Identity {
    login: String,
    display_name: String,
}

/// Exchange a password for a session cookie.
///
/// The same credentials as IMAP — `users.password_hash`, Argon2id — so there is
/// one password per person and one thing to rotate.
async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginRequest>,
) -> Response {
    // Keyed separately by login and by address: throttling only the login lets
    // one attacker work through many accounts freely, and throttling only the
    // address lets a distributed attempt through.
    let by_login = format!("login:{}", body.login);
    let by_peer = format!("peer:{}", peer.ip());

    if state.limiter.is_blocked(&by_login) || state.limiter.is_blocked(&by_peer) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "too many attempts, try again later" })),
        )
            .into_response();
    }

    let authenticated = match db::authenticate(&state.pool, &body.login, &body.password).await {
        Ok(result) => result,
        Err(e) => {
            // Log the cause, tell the client nothing: a database error and a
            // wrong password must look identical from outside.
            eprintln!("login: database error: {e:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
                .into_response();
        }
    };

    let Some((user_id, _bucket)) = authenticated else {
        state.limiter.record_failure(&by_login);
        state.limiter.record_failure(&by_peer);
        // Identical for an unknown login and a wrong password, matching
        // db::authenticate's existing behaviour: distinguishing them would
        // confirm which accounts exist.
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "invalid login or password" })),
        )
            .into_response();
    };

    // A legitimate user who mistyped a few times should not stay throttled.
    state.limiter.clear(&by_login);
    state.limiter.clear(&by_peer);

    let identity = match db::user_by_id(&state.pool, user_id).await {
        Ok(Some((login, display_name))) => Identity {
            login,
            display_name,
        },
        // Authenticated a moment ago, so a miss here means the row vanished
        // mid-request. Refuse rather than issue a session for a user we cannot
        // describe.
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
                .into_response()
        }
    };

    let cookie = session::set_cookie(
        &session::issue(&state.session_key, user_id),
        state.secure_cookies,
    );

    (
        StatusCode::OK,
        [
            (header::SET_COOKIE, cookie),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(identity),
    )
        .into_response()
}

/// Drop the session.
///
/// Unauthenticated on purpose: logging out should work even from a session that
/// has already expired or been tampered with. Refusing would leave a broken
/// cookie in place with no way to clear it from the UI.
async fn logout(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            (
                header::SET_COOKIE,
                session::clear_cookie(state.secure_cookies),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(serde_json::json!({ "status": "logged out" })),
    )
}

/// Who the caller is, and a refreshed cookie.
///
/// The refresh is what makes the 30-day expiry sliding: the client calls this
/// on boot, so anyone using the archive keeps their session, and only genuine
/// disuse expires it. Doing it here rather than in middleware means one
/// `Set-Cookie` per app load instead of one per request.
///
/// Requires a valid session — `UserScope` cannot be constructed without one, so
/// this is also the endpoint that demonstrates the phase 2 gate.
async fn whoami(State(state): State<AppState>, scope: UserScope) -> Response {
    let identity = match db::user_by_id(&state.pool, scope.user_id()).await {
        Ok(Some((login, display_name))) => Identity {
            login,
            display_name,
        },
        Ok(None) => {
            // A validly signed cookie for a user who no longer exists. Clear it
            // rather than leaving the client in a loop it cannot escape.
            return (
                StatusCode::UNAUTHORIZED,
                [(
                    header::SET_COOKIE,
                    session::clear_cookie(state.secure_cookies),
                )],
                Json(serde_json::json!({ "error": "not authenticated" })),
            )
                .into_response();
        }
        Err(e) => {
            eprintln!("session: database error: {e:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
                .into_response();
        }
    };

    let cookie = session::set_cookie(
        &session::issue(&state.session_key, scope.user_id()),
        state.secure_cookies,
    );

    (
        StatusCode::OK,
        [
            (header::SET_COOKIE, cookie),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(identity),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Mail
// ---------------------------------------------------------------------------
//
// Every handler below takes a UserScope, so none of them can be reached without
// a valid session, and the queries they call all bind its user_id. Ownership is
// enforced in SQL rather than by a check here -- a handler that forgot one
// would not compile, because the query functions require the id.

/// Folders visible to this user, with counts.
async fn folders(State(state): State<AppState>, scope: UserScope) -> Response {
    let rows = match db::folders_for_user(&state.pool, scope.user_id()).await {
        Ok(rows) => rows,
        Err(e) => return internal("folders", e),
    };

    let folders: Vec<archive_api_types::Folder> = rows
        .into_iter()
        .filter_map(|(id, label, name, delimiter, total, unread)| {
            // The archive-root INBOX exists only because IMAP requires a
            // mailbox by that name; it never holds anything. Showing a
            // permanently empty folder would just be a thing to explain.
            if total == 0 && name.eq_ignore_ascii_case("INBOX") && label.is_empty() {
                return None;
            }

            // Same namespaced path the IMAP server serves, so both clients
            // agree on what a folder is called.
            let delimiter = delimiter.as_deref().and_then(|d| d.chars().next());
            let path = format!("{label}/{}", naming::to_display(&name, delimiter));

            Some(archive_api_types::Folder {
                id,
                account: label,
                path,
                total,
                unread,
            })
        })
        .collect();

    (StatusCode::OK, Json(folders)).into_response()
}

#[derive(Deserialize)]
struct PageQuery {
    cursor: Option<String>,
    limit: Option<i64>,
}

/// Largest page a client may ask for.
///
/// A cap rather than a suggestion: `limit` is user input, and an uncapped one
/// lets a single request pull an entire 53,000-message folder into memory.
const MAX_PAGE: i64 = 200;
const DEFAULT_PAGE: i64 = 50;

/// One page of a folder, newest first.
async fn messages(
    State(state): State<AppState>,
    scope: UserScope,
    Path(folder_id): Path<i64>,
    Query(q): Query<PageQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);

    let cursor = match q.cursor.as_deref().map(decode_cursor) {
        None => None,
        Some(Some(c)) => Some(c),
        // A cursor we did not mint. Refusing beats silently restarting from the
        // top, which would look like the list had reset itself.
        Some(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "malformed cursor" })),
            )
                .into_response()
        }
    };

    // Fetch one more than asked for: if it comes back, there is another page.
    // Cheaper and more accurate than a separate COUNT, which would race.
    let rows =
        match db::messages_page(&state.pool, scope.user_id(), folder_id, cursor, limit + 1).await {
            Ok(rows) => rows,
            Err(e) => return internal("messages", e),
        };

    let has_more = rows.len() as i64 > limit;
    let rows = &rows[..rows.len().min(limit as usize)];

    let next = has_more
        .then(|| rows.last())
        .flatten()
        .map(|last| encode_cursor(last.internaldate, last.uid));

    let messages: Vec<archive_api_types::MessageSummary> = rows
        .iter()
        .map(|r| archive_api_types::MessageSummary {
            uid: r.uid,
            blake3: r.blake3.clone(),
            subject: r.subject.clone(),
            from: r.from_addr.clone(),
            from_name: r.from_name.clone(),
            date: r.internaldate.to_rfc3339(),
            size: r.size,
            seen: r.seen,
            has_attachments: r.has_attachments,
        })
        .collect();

    (
        StatusCode::OK,
        Json(archive_api_types::MessagePage { messages, next }),
    )
        .into_response()
}

/// Cursors are `<unix_millis>.<uid>`.
///
/// Opaque to the client by contract, so the encoding can change without being a
/// breaking API change. It is not signed: it identifies a position in a list
/// the caller is already authorised to read, and forging one can only move you
/// within your own folder.
fn encode_cursor(date: chrono::DateTime<chrono::Utc>, uid: i64) -> String {
    format!("{}.{}", date.timestamp_millis(), uid)
}

fn decode_cursor(raw: &str) -> Option<(chrono::DateTime<chrono::Utc>, i64)> {
    let (millis, uid) = raw.split_once('.')?;
    let date = chrono::DateTime::from_timestamp_millis(millis.parse().ok()?)?;
    Some((date, uid.parse().ok()?))
}

/// Log the cause, tell the client nothing. Database errors can carry query text
/// and connection details, neither of which belongs in a browser.
fn internal(what: &str, e: anyhow::Error) -> Response {
    eprintln!("{what}: {e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "internal error" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip() {
        let date = chrono::DateTime::from_timestamp_millis(1_725_000_000_123).unwrap();
        let encoded = encode_cursor(date, 4242);
        let (back_date, back_uid) = decode_cursor(&encoded).unwrap();
        assert_eq!(back_date, date);
        assert_eq!(back_uid, 4242);
    }

    #[test]
    fn malformed_cursors_are_rejected() {
        // Refused rather than treated as "start from the top", which would look
        // to the user like the list had silently reset itself.
        for bad in ["", "nonsense", "123", "abc.5", "123.xyz", ".", "123."] {
            assert!(decode_cursor(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn cursors_survive_dates_before_the_epoch() {
        // Mail predating 1970 is rare but real, and a negative timestamp must
        // not break paging through the folder that contains it.
        let date = chrono::DateTime::from_timestamp_millis(-86_400_000).unwrap();
        let (back, uid) = decode_cursor(&encode_cursor(date, 1)).unwrap();
        assert_eq!(back, date);
        assert_eq!(uid, 1);
    }

    #[test]
    fn attachment_names_cannot_escape_or_break_the_header() {
        // The name comes from the message, so it is attacker-controlled.
        // Asserting the PROPERTY, not an exact string: what matters is that
        // no path separator survives, so the name cannot traverse anywhere,
        // whatever the remaining punctuation looks like.
        let traversal = safe_filename("../../etc/passwd");
        assert!(!traversal.contains('/'), "{traversal}");
        assert!(!traversal.contains(char::from(92)), "{traversal}");
        assert!(!traversal.starts_with('.'), "{traversal}");

        // Characters built from codes so the test itself carries no escapes:
        // 92 backslash, 34 double quote, 39 single quote.
        let backslash = char::from(92);
        let quote = char::from(34);
        assert_eq!(
            safe_filename(&format!("C:{backslash}evil.exe")),
            "C__evil.exe"
        );
        // A quote would end the Content-Disposition field early and let the
        // rest of the name be read as further directives.
        assert!(!safe_filename(&format!("a{quote}b")).contains(quote));

        // Directory tricks and empty names are not names.
        for bad in ["..", ".", "   ", ""] {
            assert_eq!(safe_filename(bad), "attachment", "{bad:?}");
        }
        // Ordinary names survive intact.
        assert_eq!(safe_filename("invoice 2026.pdf"), "invoice 2026.pdf");
    }

    #[test]
    fn images_are_identified_by_their_bytes() {
        assert_eq!(
            sniff_image(&[0x89, 80, 78, 71, 13, 10, 26, 10, 0]),
            Some("image/png")
        );
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_image(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_image(b"RIFF____WEBPVP8 "), Some("image/webp"));
    }

    #[test]
    fn a_declared_type_cannot_smuggle_a_document() {
        // The whole point of sniffing. A part claiming image/png but containing
        // HTML must not be served -- as a same-origin document it would be
        // script execution, and the sandbox does not cover a direct URL fetch.
        assert_eq!(sniff_image(b"<html><script>alert(1)</script>"), None);
        assert_eq!(sniff_image(b"<!doctype html>"), None);
        assert_eq!(sniff_image(b"%PDF-1.7"), None);
        assert_eq!(sniff_image(b""), None);
        // Truncated magic must not be accepted on a prefix match.
        assert_eq!(sniff_image(&[0x89, 80]), None);
        assert_eq!(sniff_image(b"RIFF____"), None);
    }

    #[test]
    fn page_size_is_capped() {
        // limit is user input; uncapped it would pull a whole 53k folder into
        // memory in one request.
        assert_eq!(500i64.clamp(1, MAX_PAGE), MAX_PAGE);
        assert_eq!(0i64.clamp(1, MAX_PAGE), 1);
        assert_eq!((-5i64).clamp(1, MAX_PAGE), 1);
        assert_eq!(50i64.clamp(1, MAX_PAGE), 50);
    }
}

/// One message, opened.
///
/// Phase 4a: the plain-text body. `has_html` reports that an HTML alternative
/// exists without shipping it -- rendering that safely needs the sanitiser,
/// sandbox and CSP of WEBAPP-PLAN.md 6, which is phase 4b. Saying so is better
/// than silently showing an empty pane for an HTML-only message.
#[derive(Deserialize)]
struct MessageQuery {
    /// `images=remote` opts in to loading remote images for THIS request.
    ///
    /// A per-request choice rather than a stored preference: consenting to be
    /// tracked by one sender is not consent for every sender, and a persisted
    /// setting would quietly apply to messages the reader has not seen yet.
    images: Option<String>,
}

async fn message(
    State(state): State<AppState>,
    scope: UserScope,
    Path(blake3): Path<String>,
    Query(q): Query<MessageQuery>,
) -> Response {
    // Reject a malformed address before it reaches the database or S3. The
    // value is user-supplied and ends up in an object key.
    if blake3.len() != 64 || !blake3.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "malformed message id" })),
        )
            .into_response();
    }

    // Authorises and locates in one query: the bucket comes back from the row,
    // so this handler never chooses which bucket to read.
    let found = match db::message_for_user(&state.pool, scope.user_id(), &blake3).await {
        Ok(found) => found,
        Err(e) => return internal("message lookup", e),
    };

    let Some((bucket, size, internaldate)) = found else {
        // Identical whether the message does not exist or belongs to someone
        // else: distinguishing them would confirm the existence of another
        // user's mail.
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such message" })),
        )
            .into_response();
    };

    let store = match state.store(&bucket).await {
        Ok(store) => store,
        Err(e) => return internal("opening bucket", e),
    };

    // get_message re-hashes on read and rejects a mismatch, so a corrupted or
    // substituted object fails here rather than being rendered.
    let raw = match store.get_message(&blake3).await {
        Ok(raw) => raw,
        Err(e) => return internal("fetching message body", e),
    };

    let Some(parsed) = mail_parser::MessageParser::default().parse(&raw) else {
        // Archived bytes that will not parse. Real, and not something to hide:
        // the message is in the archive and this is the honest report of it.
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "message could not be parsed" })),
        )
            .into_response();
    };

    let allow_remote = q.images.as_deref() == Some("remote");
    let sanitised = parsed.body_html(0).map(|raw| {
        // cid: references are rewritten onto this message's own parts, which is
        // what lets the frame policy be img-src 'self' rather than allowing
        // arbitrary remote hosts.
        sanitise::clean(
            &raw,
            &format!("/api/messages/{blake3}/inline"),
            allow_remote,
        )
    });

    let detail = archive_api_types::MessageDetail {
        blake3: blake3.clone(),
        subject: parsed.subject().map(str::to_string),
        from: mailboxes(parsed.from()),
        to: mailboxes(parsed.to()),
        cc: mailboxes(parsed.cc()),
        // The archive's internaldate, not the Date: header. The header is
        // sender-controlled and frequently wrong or absent; internaldate is what
        // the list is sorted by, so using it keeps the two consistent.
        date: internaldate.to_rfc3339(),
        size,
        text: parsed.body_text(0).map(|t| t.into_owned()),
        has_html: parsed.body_html(0).is_some(),
        html: sanitised
            .as_ref()
            .map(|s| sanitise::frame_document(&s.html)),
        blocked_images: sanitised.as_ref().map_or(0, |s| s.blocked_images),
        parts: parts(&parsed),
    };

    (
        StatusCode::OK,
        // Message bodies are private. Without this a shared or proxy cache could
        // retain one, and the browser could serve it from disk after logout.
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(detail),
    )
        .into_response()
}

fn mailboxes(address: Option<&mail_parser::Address>) -> Vec<archive_api_types::Mailbox> {
    let Some(address) = address else {
        return Vec::new();
    };
    address
        .iter()
        .map(|a| archive_api_types::Mailbox {
            name: a.name().map(str::to_string),
            email: a.address().map(str::to_string),
        })
        .collect()
}

/// Attachments and inline parts, for display. Downloading them is phase 5.
fn parts(parsed: &mail_parser::Message) -> Vec<archive_api_types::Part> {
    use mail_parser::MimeHeaders;

    parsed
        .attachments()
        .enumerate()
        .map(|(index, part)| archive_api_types::Part {
            index,
            filename: part.attachment_name().map(str::to_string),
            content_type: part
                .content_type()
                .map(|c| match c.subtype() {
                    Some(sub) => format!("{}/{}", c.ctype(), sub),
                    None => c.ctype().to_string(),
                })
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            size: part.len() as i64,
        })
        .collect()
}

/// One inline part, addressed by Content-ID, for display inside the frame.
///
/// Deliberately separate from the attachment download (phase 5), because the
/// two have opposite jobs: this one is displayed, that one must never be. A
/// single handler switching on a flag is the shape of bug a later refactor
/// introduces.
///
/// Three rules make displaying it safe:
///
/// * only parts whose MAGIC BYTES are a known image type are served at all;
/// * `Content-Type` comes from that sniff, never from the message's own claim,
///   which the sender controls -- a part declaring `text/html` and served as
///   such would be script execution on our origin;
/// * `nosniff`, so a browser cannot second-guess us either.
async fn inline_part(
    State(state): State<AppState>,
    scope: UserScope,
    Path((blake3, cid)): Path<(String, String)>,
) -> Response {
    use mail_parser::MimeHeaders;

    if blake3.len() != 64 || !blake3.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (StatusCode::BAD_REQUEST, "malformed message id").into_response();
    }

    let found = match db::message_for_user(&state.pool, scope.user_id(), &blake3).await {
        Ok(found) => found,
        Err(e) => return internal("inline lookup", e),
    };
    let Some((bucket, _, _)) = found else {
        return (StatusCode::NOT_FOUND, "no such message").into_response();
    };

    let store = match state.store(&bucket).await {
        Ok(store) => store,
        Err(e) => return internal("opening bucket", e),
    };
    let raw = match store.get_message(&blake3).await {
        Ok(raw) => raw,
        Err(e) => return internal("fetching message", e),
    };
    let Some(parsed) = mail_parser::MessageParser::default().parse(&raw) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unparseable message").into_response();
    };

    // Content-IDs are conventionally wrapped in angle brackets in the header and
    // referenced without them in the body.
    let wanted = cid.trim_matches(['<', '>']);
    let part = parsed.parts.iter().find(|p| {
        p.content_id()
            .map(|id| id.trim_matches(['<', '>']) == wanted)
            .unwrap_or(false)
    });

    let Some(part) = part else {
        return (StatusCode::NOT_FOUND, "no such part").into_response();
    };

    let bytes = part.contents();
    let Some(mime) = sniff_image(bytes) else {
        // Not an image by its own bytes. Refused rather than served as
        // something guessed: this endpoint exists only to display images.
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "not a recognised image").into_response();
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_DISPOSITION, "inline".to_string()),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
            // Belt and braces: even if this were somehow rendered as a
            // document, it could load nothing and run nothing.
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'".to_string(),
            ),
        ],
        bytes.to_vec(),
    )
        .into_response()
}

/// Identify an image by its leading bytes, ignoring anything the message claims.
fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Download one attachment.
///
/// The opposite job to `inline_part`, and deliberately a separate handler:
/// that one displays a narrow set of sniffed image types inside the sandbox,
/// this one hands over ANYTHING the message contains and must therefore never
/// be displayed. One handler switching on a flag is the shape of bug a later
/// refactor introduces.
async fn download_part(
    State(state): State<AppState>,
    scope: UserScope,
    Path((blake3, index)): Path<(String, usize)>,
) -> Response {
    use mail_parser::MimeHeaders;

    if blake3.len() != 64 || !blake3.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (StatusCode::BAD_REQUEST, "malformed message id").into_response();
    }

    let found = match db::message_for_user(&state.pool, scope.user_id(), &blake3).await {
        Ok(found) => found,
        Err(e) => return internal("attachment lookup", e),
    };
    let Some((bucket, _, _)) = found else {
        return (StatusCode::NOT_FOUND, "no such message").into_response();
    };

    let store = match state.store(&bucket).await {
        Ok(store) => store,
        Err(e) => return internal("opening bucket", e),
    };
    let raw = match store.get_message(&blake3).await {
        Ok(raw) => raw,
        Err(e) => return internal("fetching message", e),
    };
    let Some(parsed) = mail_parser::MessageParser::default().parse(&raw) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unparseable message").into_response();
    };

    let Some(part) = parsed.attachments().nth(index) else {
        return (StatusCode::NOT_FOUND, "no such part").into_response();
    };

    let filename = part
        .attachment_name()
        .map(safe_filename)
        .unwrap_or_else(|| format!("part-{index}"));

    (
        StatusCode::OK,
        [
            // ALWAYS attachment, and always octet-stream. The message's own
            // Content-Type is attacker-controlled, and a part claiming text/html
            // served inline on our origin is stored XSS with the sandbox
            // bypassed entirely. Saving the file loses nothing: the operating
            // system decides what opens it, from the name.
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        part.contents().to_vec(),
    )
        .into_response()
}

/// Strip anything from a sender-supplied filename that could escape a directory
/// or break out of the Content-Disposition header.
///
/// The name comes from the message, so it may contain path separators, control
/// characters, or a quote that would end the header field early and let the
/// rest be read as further directives.
fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\"' | '\'' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    // "..", "." and an empty name are all directory tricks rather than names.
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        return "attachment".to_string();
    }
    trimmed.chars().take(200).collect()
}

#[derive(Deserialize)]
struct SeenRequest {
    seen: bool,
}

/// Mark a message read or unread.
///
/// The ONLY write the entire API offers. `placements.seen` is the only mutable
/// column in the schema, and it is a boolean rather than a flag array precisely
/// so that "read state is the only thing a client can change" is enforced by the
/// database rather than by this handler being careful.
async fn set_seen(
    State(state): State<AppState>,
    scope: UserScope,
    Path((folder_id, uid)): Path<(i64, i64)>,
    Json(body): Json<SeenRequest>,
) -> Response {
    match db::set_seen(&state.pool, scope.user_id(), folder_id, uid, body.seen).await {
        Ok(true) => (StatusCode::NO_CONTENT, ()).into_response(),
        // The placement does not exist, or belongs to someone else. Not
        // distinguished, for the same reason as everywhere else.
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such message" })),
        )
            .into_response(),
        Err(e) => internal("set seen", e),
    }
}
