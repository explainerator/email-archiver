//! Web sessions: a signed cookie, and the type that proves who is asking.
//!
//! Stateless by design. With four users, a `sessions` table would buy
//! revocation we have no mechanism to trigger, and would add a write path to a
//! schema whose defining property is that almost nothing writes to it. See
//! WEBAPP-PLAN.md 4.2.
//!
//! The cookie is `v1.<user_id>.<expiry>.<mac>`, where the MAC covers everything
//! before it. Nothing in it is secret — a user's own id is not information they
//! lack — so it is signed rather than encrypted. What matters is that it cannot
//! be *changed*: without the MAC, editing the user id in a cookie would be a
//! complete authentication bypass.

use crate::web::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

/// Cookie name. Prefixed `__Host-` in production would be stronger, but that
/// prefix *requires* the Secure attribute, and local development runs plaintext
/// — so the name would have to differ between environments, and a cookie name
/// that changes with the deployment is its own source of confusion.
pub const COOKIE: &str = "archive_session";

/// Domain separation for the derived signing key.
///
/// **Bumping this version invalidates every existing session**, which is the
/// intended way to force everyone to log in again. It is a code change rather
/// than a config one, which is the right shape: it should happen deliberately
/// and be recorded in history.
const KEY_CONTEXT: &str = "email-archiver 2026-09 web session cookie v1";

const TOKEN_VERSION: &str = "v1";

/// How long a session lasts. Refreshed on use — see `web::session`.
pub const TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

/// Derive the cookie signing key from the configured encryption key.
///
/// Deliberately *derived* rather than configured separately. WEBAPP-PLAN.md Q4
/// leaned toward a second secret, on the grounds that invalidating sessions
/// should not touch stored passwords. Deriving turns out to be better on both
/// counts: `KEY_CONTEXT` above already gives independent session invalidation,
/// and a second secret would mean another value to generate, deliver through
/// Terraform, and lose.
///
/// There is no added exposure. If `encryption_key` leaks, stored source-mailbox
/// passwords are readable — an attacker forging session cookies is not the
/// marginal harm at that point.
///
/// `blake3::derive_key` is key-derivation mode, not a plain hash, so the result
/// is independent of any other use of the same input.
pub fn derive_signing_key(encryption_key: &str) -> [u8; 32] {
    blake3::derive_key(KEY_CONTEXT, encryption_key.as_bytes())
}

/// Mint a cookie value for `user_id`, valid for `TTL_SECONDS` from now.
pub fn issue(key: &[u8; 32], user_id: i64) -> String {
    let expiry = chrono::Utc::now().timestamp() + TTL_SECONDS;
    let payload = format!("{TOKEN_VERSION}.{user_id}.{expiry}");
    let mac = blake3::keyed_hash(key, payload.as_bytes());
    format!("{payload}.{}", mac.to_hex())
}

/// Recover the user id from a cookie value, or `None` if it is not currently
/// valid.
///
/// Every failure returns `None` without distinguishing why. A caller that could
/// tell "expired" from "forged" would be a caller that could leak whether a
/// guessed signature was close.
pub fn verify(key: &[u8; 32], token: &str) -> Option<i64> {
    // Exactly four parts: a token with extra dots is malformed, not something
    // to be lenient about.
    let parts: Vec<&str> = token.split('.').collect();
    let [version, user_id, expiry, mac_hex] = parts.as_slice() else {
        return None;
    };

    if *version != TOKEN_VERSION {
        return None;
    }

    let payload = format!("{version}.{user_id}.{expiry}");
    let expected = blake3::keyed_hash(key, payload.as_bytes());
    let presented = blake3::Hash::from_hex(mac_hex).ok()?;

    // blake3::Hash compares in constant time, so this does not leak how much of
    // a forged signature was correct.
    if expected != presented {
        return None;
    }

    // Only trusted AFTER the MAC check: parsing attacker-controlled integers
    // before verifying them is how a signature check gets accidentally skipped.
    let expiry: i64 = expiry.parse().ok()?;
    if chrono::Utc::now().timestamp() >= expiry {
        return None;
    }

    user_id.parse().ok()
}

/// `Set-Cookie` for a freshly issued session.
///
/// `Secure` follows the listener, not a constant. A `Secure` cookie is never
/// sent over plaintext HTTP, so hardcoding it would make local development fail
/// to stay logged in with no error explaining why — the browser would simply
/// discard the cookie and every request would look unauthenticated.
pub fn set_cookie(value: &str, secure: bool) -> String {
    let mut c = format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={TTL_SECONDS}");
    if secure {
        c.push_str("; Secure");
    }
    c
}

/// `Set-Cookie` that removes the session.
pub fn clear_cookie(secure: bool) -> String {
    let mut c = format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        c.push_str("; Secure");
    }
    c
}

/// Read one cookie out of a `Cookie:` header.
///
/// Hand-rolled rather than pulling in a cookie crate: we set exactly one cookie
/// and need no attributes, expiry handling or jar semantics on the read path.
fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// Proof that a request carries a valid session, and for whom.
///
/// **The only way to obtain one is the extractor below**, which requires a
/// valid signed cookie. The field is private and there is no constructor, so a
/// handler cannot invent a user id, and a query function that takes a
/// `UserScope` cannot be called without one.
///
/// This is the mechanism behind WEBAPP-PLAN.md 4.4: scoping by user is enforced
/// by the type system rather than by remembering to write a `WHERE` clause.
/// IMAP has one connection per authenticated user; HTTP re-establishes who is
/// asking on every single request, so the discipline has to be structural.
#[derive(Clone, Copy, Debug)]
pub struct UserScope(i64);

impl UserScope {
    pub fn user_id(&self) -> i64 {
        self.0
    }
}

/// Rejection for a request with no valid session.
pub struct Unauthorized;

impl IntoResponse for Unauthorized {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "not authenticated" })),
        )
            .into_response()
    }
}

impl FromRequestParts<AppState> for UserScope {
    type Rejection = Unauthorized;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::COOKIE)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| cookie_value(h, COOKIE))
            .ok_or(Unauthorized)?;

        verify(&state.session_key, token)
            .map(UserScope)
            .ok_or(Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        derive_signing_key("test-encryption-key")
    }

    #[test]
    fn a_session_round_trips() {
        let k = key();
        let token = issue(&k, 42);
        assert_eq!(verify(&k, &token), Some(42));
    }

    #[test]
    fn a_different_key_rejects_the_token() {
        let token = issue(&key(), 42);
        let other = derive_signing_key("a-completely-different-key");
        assert_eq!(verify(&other, &token), None);
    }

    #[test]
    fn editing_the_user_id_is_rejected() {
        // The whole point of signing: without the MAC this would be a complete
        // authentication bypass, since the user id is right there in the value.
        let k = key();
        let token = issue(&k, 42);
        let forged = token.replacen(".42.", ".1.", 1);
        assert_ne!(forged, token, "test did not actually modify the token");
        assert_eq!(verify(&k, &forged), None);
    }

    #[test]
    fn an_expired_session_is_rejected() {
        let k = key();
        let past = chrono::Utc::now().timestamp() - 1;
        let payload = format!("{TOKEN_VERSION}.7.{past}");
        let mac = blake3::keyed_hash(&k, payload.as_bytes());
        // Correctly signed, genuinely ours, and still refused.
        let token = format!("{payload}.{}", mac.to_hex());
        assert_eq!(verify(&k, &token), None);
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let k = key();
        for bad in [
            "",
            "garbage",
            "v1.42",
            "v1.42.999999999999",
            "v1.42.999999999999.zz",
            // Extra segments: not something to be lenient about.
            "v1.42.999999999999.aa.bb",
            // A wrong version must not be silently accepted.
            "v2.42.999999999999.00",
        ] {
            assert_eq!(verify(&k, bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn the_signing_key_is_domain_separated() {
        // Derived, not equal to a plain hash of the same input, so the session
        // key cannot be confused with any other use of encryption_key.
        let derived = derive_signing_key("some-key");
        assert_ne!(derived, *blake3::hash(b"some-key").as_bytes());
    }

    #[test]
    fn secure_attribute_follows_the_listener() {
        assert!(set_cookie("x", true).contains("; Secure"));
        assert!(!set_cookie("x", false).contains("; Secure"));
        // Always present regardless of transport.
        assert!(set_cookie("x", false).contains("HttpOnly"));
        assert!(set_cookie("x", false).contains("SameSite=Lax"));
    }

    #[test]
    fn cookies_are_parsed_out_of_a_header() {
        assert_eq!(
            cookie_value("a=1; archive_session=tok; b=2", COOKIE),
            Some("tok")
        );
        assert_eq!(cookie_value("archive_session=tok", COOKIE), Some("tok"));
        assert_eq!(cookie_value("other=1", COOKIE), None);
        // A cookie whose name merely ends with ours must not match.
        assert_eq!(cookie_value("not_archive_session=tok", COOKIE), None);
    }
}
