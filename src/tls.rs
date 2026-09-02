//! TLS for the IMAP listener.
//!
//! Certificates come from files on disk rather than an in-process ACME client.
//! That keeps certificate *acquisition* out of this program entirely: `certbot`
//! (or anything else) writes the files and renews on its own schedule, and we
//! only read them. One less thing to maintain, and it works with any issuer.
//!
//! The cost is that renewal happens behind our back, so the files are reloaded
//! when they change rather than read once at startup — otherwise the server
//! would keep serving a certificate that expired ninety days ago and nobody
//! would notice until clients started refusing to connect.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::Mutex;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Holds the current certificate and reloads it when the files change.
pub struct CertReloader {
    cert_path: PathBuf,
    key_path: PathBuf,
    current: Mutex<Loaded>,
}

struct Loaded {
    acceptor: TlsAcceptor,
    /// Modification time the config was built from, so a renewal is noticed.
    stamp: Option<SystemTime>,
}

impl CertReloader {
    pub fn new(cert_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Result<Self> {
        let cert_path = cert_path.into();
        let key_path = key_path.into();
        let acceptor = build_acceptor(&cert_path, &key_path)?;
        let stamp = modified(&cert_path);
        Ok(Self {
            cert_path,
            key_path,
            current: Mutex::new(Loaded { acceptor, stamp }),
        })
    }

    /// The acceptor to use for the next connection, rebuilt if the certificate
    /// changed since last time.
    ///
    /// A failed reload keeps the previous certificate rather than dropping the
    /// listener: a half-written file during renewal is a transient condition,
    /// and refusing every connection because of it would be a worse outcome
    /// than briefly serving the old certificate.
    pub async fn acceptor(&self) -> TlsAcceptor {
        let mut guard = self.current.lock().await;
        let stamp = modified(&self.cert_path);
        if stamp != guard.stamp {
            match build_acceptor(&self.cert_path, &self.key_path) {
                Ok(acceptor) => {
                    eprintln!("TLS: certificate changed on disk, reloaded");
                    guard.acceptor = acceptor;
                    guard.stamp = stamp;
                }
                Err(e) => eprintln!("TLS: reload failed, keeping previous certificate: {e:#}"),
            }
        }
        guard.acceptor.clone()
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn build_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor> {
    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate and private key do not match, or are unusable")?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let data = std::fs::read(path)
        .with_context(|| format!("reading certificate {}", path.display()))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut data.as_slice())
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parsing certificate {}", path.display()))?;
    anyhow::ensure!(
        !certs.is_empty(),
        "no certificates found in {} — expected PEM, and the full chain rather \
         than just the leaf",
        path.display()
    );
    Ok(certs)
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let data =
        std::fs::read(path).with_context(|| format!("reading private key {}", path.display()))?;
    rustls_pemfile::private_key(&mut data.as_slice())
        .with_context(|| format!("parsing private key {}", path.display()))?
        .with_context(|| {
            format!(
                "no private key found in {} — expected PKCS#8, PKCS#1 or SEC1 PEM",
                path.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_certificate_says_which_file() {
        let err = load_certs(Path::new("definitely-not-here.pem"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("definitely-not-here.pem"), "{err}");
    }

    #[test]
    fn an_empty_pem_is_rejected_clearly() {
        // A truncated or wrong file should say what was expected, not fail
        // somewhere deep in the handshake later.
        let dir = std::env::temp_dir().join("email-archiver-tls-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.pem");
        std::fs::write(&path, b"").unwrap();

        let err = load_certs(&path).unwrap_err().to_string();
        assert!(err.contains("no certificates found"), "{err}");
        let _ = std::fs::remove_file(&path);
    }
}
