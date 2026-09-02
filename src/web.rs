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
//! **Phases 1-3**: the listener, health, static assets, authentication, the
//! folder list and paged message lists. Message bodies and attachments are
//! phase 4-5.
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
use crate::session::{self, UserScope};

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
            date: r.internaldate.to_rfc3339(),
            size: r.size,
            seen: r.seen,
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
    fn page_size_is_capped() {
        // limit is user input; uncapped it would pull a whole 53k folder into
        // memory in one request.
        assert_eq!(500i64.clamp(1, MAX_PAGE), MAX_PAGE);
        assert_eq!(0i64.clamp(1, MAX_PAGE), 1);
        assert_eq!((-5i64).clamp(1, MAX_PAGE), 1);
        assert_eq!(50i64.clamp(1, MAX_PAGE), 50);
    }
}
