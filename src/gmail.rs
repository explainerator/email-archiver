//! Google Workspace access via a service account with domain-wide delegation.
//!
//! Phase 8a of `ARCHIVE-PLAN.md`, and the one with a deadline: those mailboxes
//! disappear when the mail migration completes and we do not control the date.
//!
//! # Why a service account rather than per-user OAuth
//!
//! The usual installed-app OAuth flow means a browser consent screen per
//! mailbox and a refresh token stored for each — plus the trap that refresh
//! tokens issued while the consent screen is in *Testing* expire after seven
//! days, which would break ingest silently a week after it was set up.
//!
//! Domain-wide delegation replaces all of that with one credential. The service
//! account is authorised once in the Admin console to impersonate users in the
//! domain, and mints a token for any mailbox on demand. Nothing is stored per
//! account, and there is no consent screen to leave in the wrong state.
//!
//! # Setting it up (once)
//!
//! 1. Google Cloud console → new project → enable the **Gmail API**.
//! 2. Create a **service account**; create a **JSON key** for it and save it.
//! 3. Note the service account's **Client ID** (a long number, on its details
//!    page — not the email address).
//! 4. Admin console → Security → Access and data control → **API controls** →
//!    **Domain-wide delegation** → Add new. Client ID from step 3, and the
//!    single scope `https://mail.google.com/`.
//! 5. Point `gmail.service_account_key` in `config.toml` at the JSON file.
//!
//! Step 4 is the one that is easy to miss, and its failure mode is a token
//! request rejected with `unauthorized_client` — see [`AccessTokens::for_user`].
//!
//! # Why the HTTP request is hand-rolled
//!
//! One POST, to one known endpoint, returning one field. `hyper`,
//! `tokio-rustls` and `webpki-roots` are already dependencies (the web server
//! and the S3 client between them), so this needs no new crate — against
//! pulling in a full HTTP client for a single request.

use anyhow::{Context, Result};
use base64::Engine as _;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// The scope Gmail's IMAP endpoint requires.
///
/// `https://mail.google.com/` is the only scope that grants IMAP; the narrower
/// `gmail.readonly` covers the REST API but IMAP refuses it. Worth knowing,
/// because it looks like an over-broad request and is not.
const SCOPE: &str = "https://mail.google.com/";

/// Tokens last an hour; renew early so a long ingest never trips over the edge.
const RENEW_BEFORE: Duration = Duration::from_secs(300);

/// A service account key, as downloaded from the Google Cloud console.
#[derive(serde::Deserialize)]
pub struct ServiceAccount {
    pub client_email: String,
    /// PEM, PKCS#8. Secret: this is the whole credential.
    private_key: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

/// Never print the key, even by accident.
impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("client_email", &self.client_email)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl ServiceAccount {
    /// Parse a key. `source` only names the origin for error messages.
    pub fn parse(json: &str, source: &str) -> Result<Self> {
        let account: ServiceAccount = serde_json::from_str(json)
            .with_context(|| format!("parsing {source} as a Google service account key"))?;
        anyhow::ensure!(
            !account.private_key.is_empty(),
            "{source} has no private_key; is it a service account key rather than an OAuth client?"
        );
        Ok(account)
    }

    /// Read a key from a file. Used once, by `set-google`, to put it in the
    /// database; ingest reads it from there afterwards so the file need not
    /// exist on whichever machine runs the import.
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading the service account key {path}"))?;
        Self::parse(&raw, path)
    }
}

/// Mints and caches access tokens, one per impersonated mailbox.
pub struct AccessTokens {
    account: ServiceAccount,
    cache: Mutex<HashMap<String, (String, SystemTime)>>,
}

impl AccessTokens {
    pub fn new(account: ServiceAccount) -> Arc<Self> {
        Arc::new(Self {
            account,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// An access token for `user`, minted or reused.
    ///
    /// Cached because ingest reconnects per folder and on every network blip; a
    /// token request per reconnect would be both slow and rate-limited.
    pub async fn for_user(&self, user: &str) -> Result<String> {
        let mut cache = self.cache.lock().await;
        if let Some((token, expires)) = cache.get(user) {
            if *expires > SystemTime::now() + RENEW_BEFORE {
                return Ok(token.clone());
            }
        }

        let assertion = self.assertion(user)?;
        let (token, lifetime) = exchange(&self.account.token_uri, &assertion)
            .await
            .with_context(|| {
                format!(
                    "requesting a Google access token for {user}.\n\
                     `unauthorized_client` here almost always means the service account's \
                     Client ID has not been granted domain-wide delegation for the scope \
                     {SCOPE} in the Admin console — see src/gmail.rs."
                )
            })?;

        cache.insert(
            user.to_string(),
            (token.clone(), SystemTime::now() + lifetime),
        );
        Ok(token)
    }

    /// Build and sign the JWT that asks to act as `user`.
    fn assertion(&self, user: &str) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the epoch")?
            .as_secs();

        let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
        // `sub` is what makes this delegation rather than the service account
        // acting as itself: it names the mailbox to impersonate.
        let claims = serde_json::json!({
            "iss": self.account.client_email,
            "sub": user,
            "scope": SCOPE,
            "aud": self.account.token_uri,
            "iat": now,
            "exp": now + 3600,
        });

        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let signing_input = format!(
            "{}.{}",
            b64.encode(serde_json::to_vec(&header)?),
            b64.encode(serde_json::to_vec(&claims)?)
        );

        let signature = sign_rs256(&self.account.private_key, signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", b64.encode(signature)))
    }
}

/// RS256 over `message`, using the PKCS#8 PEM private key.
fn sign_rs256(private_key_pem: &str, message: &[u8]) -> Result<Vec<u8>> {
    let mut pem = private_key_pem.as_bytes();
    let key = rustls_pemfile::private_key(&mut pem)
        .context("parsing the service account private key")?
        .context("the service account key contains no PRIVATE KEY block")?;

    let pair = ring::signature::RsaKeyPair::from_pkcs8(key.secret_der())
        .map_err(|e| anyhow::anyhow!("service account key is not a usable RSA key: {e}"))?;

    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        message,
        &mut signature,
    )
    .map_err(|e| anyhow::anyhow!("signing the assertion failed: {e}"))?;

    Ok(signature)
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// POST the assertion and read back an access token.
///
/// Hand-rolled rather than pulling in an HTTP client for one request; see the
/// module docs.
async fn exchange(token_uri: &str, assertion: &str) -> Result<(String, Duration)> {
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;

    let uri: hyper::Uri = token_uri.parse().context("token_uri is not a URL")?;
    let host = uri.host().context("token_uri has no host")?.to_string();
    let port = uri.port_u16().unwrap_or(443);

    let body = format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={assertion}"
    );

    // Google's own roots, verified normally. No allow_invalid_certs escape here
    // deliberately: this connection carries a credential that can read every
    // mailbox in the domain.
    let tls = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(tokio_rustls::rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        })
        .with_no_client_auth();

    let stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.clone())
        .context("token host is not a valid DNS name")?;
    let stream = tokio_rustls::TlsConnector::from(Arc::new(tls))
        .connect(server_name, stream)
        .await
        .context("TLS handshake with the token endpoint")?;

    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .context("HTTP handshake with the token endpoint")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let request = hyper::Request::post(uri.path())
        .header(hyper::header::HOST, &host)
        .header(
            hyper::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(Full::new(Bytes::from(body)))
        .context("building the token request")?;

    let response = sender
        .send_request(request)
        .await
        .context("token request")?;
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .context("reading the token response")?
        .to_bytes();

    anyhow::ensure!(
        status.is_success(),
        "token endpoint returned {status}: {}",
        String::from_utf8_lossy(&bytes)
    );

    let parsed: TokenResponse =
        serde_json::from_slice(&bytes).context("token response was not the expected JSON")?;

    Ok((parsed.access_token, Duration::from_secs(parsed.expires_in)))
}

/// SASL XOAUTH2, as Gmail's IMAP expects it.
///
/// The exchange is one-shot: the client sends the whole credential in the
/// initial response, and the server either accepts it or issues a challenge
/// carrying an error, which we answer with an empty string to end the exchange
/// cleanly rather than leaving the connection mid-SASL.
pub struct XOAuth2 {
    pub user: String,
    pub token: String,
    sent: bool,
}

impl XOAuth2 {
    pub fn new(user: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            token: token.into(),
            sent: false,
        }
    }
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        if self.sent {
            // A second call means the server rejected the credential and sent
            // an error challenge. Answering empty lets it fail cleanly.
            return String::new();
        }
        self.sent = true;
        // The \x01 separators and the doubled terminator are part of the
        // format, not a typo.
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_imap::Authenticator;

    #[test]
    fn xoauth2_uses_the_wire_format_gmail_expects() {
        let mut auth = XOAuth2::new("someone@example.com", "ya29.token");
        let first = auth.process(b"");
        assert_eq!(
            first,
            "user=someone@example.com\x01auth=Bearer ya29.token\x01\x01"
        );
    }

    #[test]
    fn a_second_challenge_ends_the_exchange() {
        // Gmail answers a bad credential with a base64 JSON error challenge.
        // Replying with the credential again would loop; empty ends it.
        let mut auth = XOAuth2::new("someone@example.com", "bad");
        let _ = auth.process(b"");
        assert_eq!(auth.process(br#"{"status":"400"}"#), "");
    }

    #[test]
    fn a_service_account_key_never_prints_its_private_key() {
        let account = ServiceAccount {
            client_email: "svc@project.iam.gserviceaccount.com".into(),
            private_key: "-----BEGIN PRIVATE KEY-----SECRET-----END PRIVATE KEY-----".into(),
            token_uri: default_token_uri(),
        };
        let shown = format!("{account:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("svc@project.iam.gserviceaccount.com"));
    }

    #[test]
    fn a_key_file_that_is_not_a_service_account_is_rejected_clearly() {
        let dir = std::env::temp_dir().join("email-archiver-gmail-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("oauth-client.json");
        // What you get if you download an OAuth *client* by mistake -- an easy
        // confusion, and one whose failure would otherwise be a parse error.
        std::fs::write(&path, br#"{"installed":{"client_id":"x"}}"#).unwrap();

        let err = ServiceAccount::load(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("service account"), "{err}");
        let _ = std::fs::remove_file(&path);
    }
}
