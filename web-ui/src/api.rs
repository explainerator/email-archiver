//! Calls to the archive's HTTP API.
//!
//! Same-origin in both environments — the binary serves these assets in
//! production, and the dev server proxies `/api` in development — so the
//! session cookie rides along under `fetch`'s default `same-origin` credentials
//! policy. Nothing here has to know the cookie exists, and no CORS policy needs
//! to exist anywhere.

use archive_api_types::{ApiError, Identity, LoginRequest};
use gloo_net::http::Request;

/// Everything that can go wrong with a call, from the client's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The server said no, with a message safe to show the user.
    Api(String),
    /// Authenticated request without a valid session. Distinguished from
    /// `Api` because the app reacts to it structurally: it returns to the login
    /// screen rather than showing an error.
    Unauthorized,
    /// Could not reach the server, or it did not answer with what we expected.
    Transport(String),
}

impl Error {
    pub fn message(&self) -> String {
        match self {
            Error::Api(m) => m.clone(),
            Error::Unauthorized => "Your session has expired. Please sign in again.".into(),
            Error::Transport(m) => format!("Could not reach the archive: {m}"),
        }
    }
}

/// Read a response, mapping status to [`Error`] before attempting to decode.
///
/// `auth_401` says what a 401 MEANS for this call, and the two meanings are not
/// interchangeable. On an authenticated endpoint it means the session is gone,
/// and the app should return to the login screen. On `login` itself it means
/// the credentials were rejected — reporting that as "your session has expired"
/// tells the user to do the exact thing they are already doing, and hides the
/// server's actual message.
async fn decode<T: serde::de::DeserializeOwned>(
    response: gloo_net::http::Response,
    auth_401: bool,
) -> Result<T, Error> {
    let status = response.status();

    if status == 401 && auth_401 {
        return Err(Error::Unauthorized);
    }

    if !(200..300).contains(&status) {
        // Every failure shares one body shape, so this is the only place that
        // needs to know it. A body that will not parse still has to produce
        // something showable rather than an empty message.
        let message = response
            .json::<ApiError>()
            .await
            .map(|e| e.error)
            .unwrap_or_else(|_| format!("request failed ({status})"));
        return Err(Error::Api(message));
    }

    response
        .json::<T>()
        .await
        .map_err(|e| Error::Transport(e.to_string()))
}

/// Who is logged in, if anyone. Also refreshes the session cookie, which is
/// what makes the 30-day expiry sliding — so the app calls this on every boot.
pub async fn session() -> Result<Identity, Error> {
    let response = Request::get("/api/session")
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    decode(response, true).await
}

pub async fn login(login: String, password: String) -> Result<Identity, Error> {
    let response = Request::post("/api/login")
        .json(&LoginRequest { login, password })
        .map_err(|e| Error::Transport(e.to_string()))?
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    decode(response, false).await
}

pub async fn logout() -> Result<(), Error> {
    Request::post("/api/logout")
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    // Deliberately ignores the status. Logging out is a local intent as much as
    // a server one: if the call fails, the app still returns to the login
    // screen rather than trapping the user in a session they asked to leave.
    Ok(())
}

pub async fn folders() -> Result<Vec<archive_api_types::Folder>, Error> {
    let response = Request::get("/api/folders")
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    decode(response, true).await
}

/// One page of a folder. `cursor` is passed back verbatim from a previous
/// page's `next` — its encoding is the server's business.
pub async fn messages(
    folder_id: i64,
    cursor: Option<String>,
) -> Result<archive_api_types::MessagePage, Error> {
    let url = match cursor {
        // The cursor is server-generated and currently digits and a dot, but it
        // still goes through encoding: a query parameter built by concatenation
        // is a bug waiting for the format to change.
        Some(c) => format!("/api/folders/{folder_id}/messages?cursor={}", encode(&c)),
        None => format!("/api/folders/{folder_id}/messages"),
    };
    let response = Request::get(&url)
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    decode(response, true).await
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

pub async fn message(blake3: &str) -> Result<archive_api_types::MessageDetail, Error> {
    let response = Request::get(&format!("/api/messages/{}", encode(blake3)))
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    decode(response, true).await
}
