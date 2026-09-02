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
//! **Phase 1**: the listener, the health endpoint, and static asset serving.
//! Authentication (Phase 2) and everything that reads mail (Phase 3 onward) are
//! not here yet — so there is currently nothing behind this server to protect,
//! and no endpoint that touches a message.

use anyhow::{Context, Result};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::listen;

/// Everything a handler needs, cloned per request.
#[derive(Clone)]
pub struct AppState {
    #[allow(dead_code)] // Phase 3 onward.
    pub config: Arc<Config>,
    pub pool: PgPool,
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
    };

    let app = router(state, assets.clone());

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
        None => println!("  no asset directory; API only (health at /api/health)"),
    }

    axum::serve(listener, app).await.context("serving HTTP")?;

    Ok(())
}

fn router(state: AppState, assets: Option<PathBuf>) -> Router {
    let api = Router::new().route("/health", get(health));

    let app = Router::new().nest("/api", api);

    // The frontend is a single-page app: the browser may deep-link to a route
    // the server knows nothing about, so anything not matched by a file falls
    // back to index.html and the client router takes it from there.
    //
    // The API is nested FIRST, so a missing endpoint returns 404 rather than
    // being answered with index.html -- a JSON client receiving HTML is a
    // confusing way to learn a route is misspelled.
    match assets {
        Some(dir) => {
            let index = dir.join("index.html");
            app.fallback_service(
                tower_http::services::ServeDir::new(dir)
                    .not_found_service(tower_http::services::ServeFile::new(index)),
            )
        }
        None => app,
    }
    .with_state(state)
}

/// Liveness, and proof the database is actually reachable.
///
/// A health check that only proves the process is running would report healthy
/// while every real request failed on a dead pool, which is worse than no check
/// at all -- it is a check that lies. The query is trivial so this stays cheap
/// enough to poll.
async fn health(axum::extract::State(state): axum::extract::State<AppState>) -> impl IntoResponse {
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
