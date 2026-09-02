//! Where a listener binds, and whether it is allowed to speak plaintext.
//!
//! Two servers now accept connections — IMAP (`server.rs`) and HTTP
//! (`web.rs`) — and both carry credentials: IMAP LOGIN sends the password in
//! clear text, and the web client sends a session cookie that is equivalent to
//! one. The rule for when plaintext is acceptable is therefore identical for
//! both, and it lives here so there is one copy of it rather than two that can
//! drift apart.
//!
//! The rule: TLS whenever a certificate is configured *and* the listener's
//! loopback policy allows it (see [`Loopback`]). Without TLS, plaintext is
//! permitted on loopback and refused anywhere else unless explicitly overridden
//! for local testing.

use crate::config::Config;
use crate::tls::CertReloader;
use anyhow::Result;
use std::sync::Arc;

/// How a listener will carry traffic, once the rules below have been applied.
pub enum Transport {
    Tls(Arc<CertReloader>),
    /// Plaintext, and whether that is on loopback. A non-loopback plaintext
    /// listener only exists because it was explicitly asked for, and callers
    /// use this to say so in their startup banner.
    Plaintext {
        loopback: bool,
    },
}

impl Transport {
    pub fn is_tls(&self) -> bool {
        matches!(self, Transport::Tls(_))
    }
}

/// What a listener does on loopback when a certificate *is* configured.
///
/// The two servers differ here, for a reason specific to browsers.
///
/// The archive's certificate is issued for `archive.thebackroom420.ca`. A
/// browser connecting to `https://127.0.0.1:8000` checks the name on the
/// certificate against the name it dialled, they do not match, and it refuses
/// the connection — so TLS on loopback is not merely unnecessary for the web
/// client, it cannot work at all. There is no flag that makes it work, short of
/// a second certificate nobody wants to issue or maintain.
///
/// IMAP is different: a test client can be pointed at a loopback listener and
/// told to skip verification, which is how the TLS path was originally
/// checked. That is a real workflow and removing it would be a regression.
///
/// Hence: IMAP keeps `MayUseTls`, the web server takes `AlwaysPlaintext`. This
/// also means one config file works in both places — the production
/// `config.toml`, whose `tls.cert_path` points at `/etc/letsencrypt/...`, can
/// be used unchanged on a development machine where that path does not exist,
/// because the web listener never tries to read it on loopback.
pub enum Loopback {
    /// Serve TLS on loopback if a certificate is configured.
    MayUseTls,
    /// Never serve TLS on loopback, whatever the config says.
    AlwaysPlaintext,
}

/// Decide how to serve `bind`, or refuse.
///
/// `protocol` and `exposure` appear in the refusal message — the reason
/// plaintext is unacceptable differs between the two servers, and an error that
/// explains the actual risk is worth more than a generic one.
///
/// `loopback_policy` is what separates the two callers. See [`Loopback`].
pub fn resolve(
    config: &Config,
    bind: &str,
    allow_plaintext: bool,
    protocol: &str,
    exposure: &str,
    loopback_policy: Loopback,
) -> Result<Transport> {
    let loopback = is_loopback(bind);

    let use_configured_tls = match loopback_policy {
        Loopback::MayUseTls => true,
        Loopback::AlwaysPlaintext => !loopback,
    };

    if use_configured_tls {
        if let Some((cert, key)) = config.tls.paths()? {
            return Ok(Transport::Tls(Arc::new(CertReloader::new(cert, key)?)));
        }
    }

    if !loopback {
        anyhow::ensure!(
            allow_plaintext,
            "refusing to serve plaintext on {bind}: {exposure}.\n\
             Set tls.cert_path and tls.key_path, bind 127.0.0.1, or pass \
             --allow-plaintext to override for local testing."
        );
        // A flag rather than a config option, deliberately: a config setting
        // persists and would quietly remain enabled after deployment. This has
        // to be re-stated every time the process starts.
        eprintln!(
            "  WARNING: serving {protocol} in PLAINTEXT on {bind} because \
             --allow-plaintext was given."
        );
    }

    Ok(Transport::Plaintext { loopback })
}

/// Only loopback may be served without TLS.
///
/// A bind address that does not parse as an IP -- a hostname, or something
/// malformed -- is treated as NOT loopback. Guessing in the permissive
/// direction here would be the wrong way to be wrong.
pub fn is_loopback(bind: &str) -> bool {
    bind.rsplit_once(':')
        .and_then(|(host, _)| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_addresses_are_recognised() {
        assert!(is_loopback("127.0.0.1:8000"));
        assert!(is_loopback("127.0.0.1:1143"));
        assert!(is_loopback("[::1]:8000"));
        assert!(is_loopback("127.5.5.5:993"));
    }

    #[test]
    fn public_addresses_are_not_loopback() {
        assert!(!is_loopback("0.0.0.0:993"));
        assert!(!is_loopback("51.79.93.209:993"));
        assert!(!is_loopback("[::]:443"));
    }

    #[test]
    fn unparseable_binds_are_treated_as_public() {
        // Failing safe: anything we cannot prove is loopback is refused
        // plaintext, rather than being waved through.
        assert!(!is_loopback("localhost:8000"));
        assert!(!is_loopback("8000"));
        assert!(!is_loopback(""));
    }
}
