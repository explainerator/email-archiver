//! Per-user S3 blob storage.
//!
//! Layout inside each user's bucket:
//!
//! ```text
//! messages/<blake3-hex>          the raw RFC 5322 bytes, content-addressed
//! manifest/<account>/<folder>/<uid>.json   sidecar: where that message sits
//! ```
//!
//! **The manifest is what makes the archive rebuildable.** Listing `manifest/`
//! and reading each object yields everything needed to reconstruct a user's
//! Postgres rows without parsing gigabytes of message bodies. Rebuild is not a
//! recovery path bolted on later — it is the *only* way the index is ever
//! populated, so it cannot rot.
//!
//! One `Store` holds one bucket's credentials. Credentials are scoped by policy
//! to a single bucket, so a `Store` structurally cannot read another user's
//! mail.

use anyhow::{Context, Result};
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Where a message sits, in one user's archive. Serialised to S3 alongside the
/// message so the database can be rebuilt from the bucket alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    /// Source account this arrived through, e.g. "ken@twoducks.ca".
    pub account: String,
    /// Folder as presented over IMAP, e.g. "INBOX".
    pub folder: String,
    /// The UID we serve to clients. Assigned once and never reissued — if these
    /// shifted, every client would silently re-download everything, so it is
    /// recorded here rather than regenerated during a rebuild.
    pub uid: i64,
    /// UID on the *source* server, for resumable ingest. Distinct from `uid`.
    pub source_uid: i64,
    /// RFC 3339. IMAP INTERNALDATE.
    pub internaldate: String,
    /// Read state. The only mutable field in the whole archive.
    pub seen: bool,
    /// blake3 of the raw message; the key under `messages/`.
    pub blake3: String,
    pub size: i64,
}

pub struct Store {
    client: Client,
    bucket: String,
}

impl Store {
    /// Build a client for one bucket using that bucket's own credentials.
    pub async fn open(config: &Config, bucket: &str) -> Result<Self> {
        let creds = config.credentials_for(bucket)?;

        let s3_config = aws_sdk_s3::Config::builder()
            .region(Region::new(config.s3.region.clone()))
            .endpoint_url(&config.s3.endpoint)
            // OVH is not AWS: the endpoint and region are supplied explicitly
            // rather than derived. Bucket names are hyphen-only (never dots),
            // so virtual-hosted-style addressing works and no path-style
            // override is needed.
            .credentials_provider(Credentials::new(
                creds.access_key.clone(),
                creds.secret_key.clone(),
                None,
                None,
                "email-archiver-config",
            ))
            .behavior_version(aws_config::BehaviorVersion::latest())
            .build();

        Ok(Self {
            client: Client::from_conf(s3_config),
            bucket: bucket.to_string(),
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn message_key(hash: &str) -> String {
        format!("messages/{hash}")
    }

    /// Manifest key. **Must include the folder**: IMAP UIDs are per-folder, so
    /// every folder starts at 1 and keying on uid alone silently overwrites one
    /// folder's manifests with another's.
    ///
    /// Segments are encoded because folder names may contain `/` (which would
    /// otherwise forge a path) or `%`. Everything needed to rebuild the index
    /// must be recoverable from the key itself.
    pub fn manifest_key(account: &str, folder: &str, uid: i64) -> String {
        format!(
            "manifest/{}/{}/{uid}.json",
            encode_segment(account),
            encode_segment(folder)
        )
    }

    /// Put an object at an arbitrary key. The primitive the rest builds on.
    pub async fn put_raw(&self, key: &str, body: &[u8]) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body.to_vec()))
            .send()
            .await
            .with_context(|| format!("putting {key} in {}", self.bucket))?;
        Ok(())
    }

    /// Get an object at an arbitrary key.
    pub async fn get_raw(&self, key: &str) -> Result<Vec<u8>> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("getting {key} from {}", self.bucket))?;

        let bytes = out
            .body
            .collect()
            .await
            .with_context(|| format!("reading body of {key}"))?;
        Ok(bytes.into_bytes().to_vec())
    }

    /// Store a raw message, keyed by its blake3 hash. Returns the hash.
    ///
    /// Idempotent by construction: the same bytes produce the same key, so a
    /// re-ingest overwrites with identical content rather than duplicating.
    pub async fn put_message(&self, raw: &[u8]) -> Result<String> {
        let hash = blake3::hash(raw).to_hex().to_string();
        self.put_raw(&Self::message_key(&hash), raw).await?;
        Ok(hash)
    }

    /// Fetch a message and verify it still hashes to the key it was stored
    /// under. Content-addressing makes this check free, so there is no reason
    /// to serve bytes we have not confirmed are the ones we stored.
    pub async fn get_message(&self, hash: &str) -> Result<Vec<u8>> {
        let raw = self.get_raw(&Self::message_key(hash)).await?;
        let actual = blake3::hash(&raw).to_hex().to_string();
        anyhow::ensure!(
            actual == hash,
            "content mismatch for messages/{hash}: bytes hash to {actual}. \
             The object was modified or corrupted in storage."
        );
        Ok(raw)
    }

    /// Write the message first, then its manifest.
    ///
    /// Order matters: a crash between the two leaves an orphan message object,
    /// which is harmless and reclaimed by a later pass. The reverse order would
    /// leave a manifest pointing at a message that does not exist — which is
    /// indistinguishable from corruption during a rebuild.
    pub async fn put_manifest(&self, manifest: &Manifest) -> Result<()> {
        let key = Self::manifest_key(&manifest.account, &manifest.folder, manifest.uid);
        let body = serde_json::to_vec_pretty(manifest).context("serialising manifest")?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(ByteStream::from(body))
            .content_type("application/json")
            .send()
            .await
            .with_context(|| format!("putting {key} in {}", self.bucket))?;

        Ok(())
    }

    pub async fn get_manifest(&self, key: &str) -> Result<Manifest> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("getting {key} from {}", self.bucket))?;

        let bytes = out.body.collect().await?.into_bytes();
        serde_json::from_slice(&bytes).with_context(|| format!("parsing manifest {key}"))
    }

    /// Every object key under a prefix, following pagination.
    ///
    /// Pagination is not optional: S3 caps a listing at 1000 keys, and an
    /// archive will have far more. Silently processing only the first page
    /// would produce a rebuild that looks successful and is missing most of
    /// the mail.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut continuation: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(token) = &continuation {
                req = req.continuation_token(token);
            }

            let page = req
                .send()
                .await
                .with_context(|| format!("listing {prefix} in {}", self.bucket))?;

            keys.extend(page.contents().iter().filter_map(|o| o.key().map(String::from)));

            match page.next_continuation_token() {
                Some(token) => continuation = Some(token.to_string()),
                None => break,
            }
        }

        Ok(keys)
    }

    /// Every object VERSION under a prefix, including delete markers.
    ///
    /// The buckets have versioning enabled, so a plain DELETE only writes a
    /// delete marker — the bytes remain as noncurrent versions and are still
    /// billed. Anything that genuinely needs to remove data has to work at the
    /// version level.
    pub async fn list_versions(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut key_marker: Option<String> = None;
        let mut version_marker: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_object_versions()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(k) = &key_marker {
                req = req.key_marker(k);
            }
            if let Some(v) = &version_marker {
                req = req.version_id_marker(v);
            }

            let page = req
                .send()
                .await
                .with_context(|| format!("listing versions of {prefix} in {}", self.bucket))?;

            for v in page.versions() {
                if let (Some(k), Some(id)) = (v.key(), v.version_id()) {
                    out.push((k.to_string(), id.to_string()));
                }
            }
            for m in page.delete_markers() {
                if let (Some(k), Some(id)) = (m.key(), m.version_id()) {
                    out.push((k.to_string(), id.to_string()));
                }
            }

            if page.is_truncated().unwrap_or(false) {
                key_marker = page.next_key_marker().map(String::from);
                version_marker = page.next_version_id_marker().map(String::from);
            } else {
                break;
            }
        }

        Ok(out)
    }

    /// Permanently remove one version. Unlike `delete`, this does not leave a
    /// delete marker behind.
    pub async fn delete_version(&self, key: &str, version_id: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .version_id(version_id)
            .send()
            .await
            .with_context(|| format!("deleting {key}@{version_id} from {}", self.bucket))?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("deleting {key} from {}", self.bucket))?;
        Ok(())
    }
}

/// Percent-encode the characters that would make a key ambiguous.
fn encode_segment(s: &str) -> String {
    s.replace('%', "%25").replace('/', "%2F")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable() {
        assert_eq!(Store::message_key("abc"), "messages/abc");
        assert_eq!(
            Store::manifest_key("ken@twoducks.ca", "INBOX", 42),
            "manifest/ken@twoducks.ca/INBOX/42.json"
        );
    }

    #[test]
    fn folders_do_not_collide_on_uid() {
        // The bug this guards against: IMAP UIDs restart at 1 in every folder,
        // so a key without the folder loses one folder's manifests entirely.
        let a = Store::manifest_key("k@x.ca", "INBOX", 1);
        let b = Store::manifest_key("k@x.ca", "INBOX.Trash", 1);
        assert_ne!(a, b);
    }

    #[test]
    fn folder_separators_cannot_forge_a_path() {
        let odd = Store::manifest_key("k@x.ca", "weird/name", 1);
        assert_eq!(odd, "manifest/k@x.ca/weird%2Fname/1.json");
        assert_ne!(odd, Store::manifest_key("k@x.ca", "weird", 1).replace("/1", "/name/1"));
    }

    #[test]
    fn manifest_roundtrips() {
        let m = Manifest {
            account: "ken@twoducks.ca".into(),
            folder: "INBOX".into(),
            uid: 7,
            source_uid: 91,
            internaldate: "2026-09-01T12:00:00Z".into(),
            seen: false,
            blake3: "a".repeat(64),
            size: 1234,
        };
        let json = serde_json::to_vec(&m).unwrap();
        assert_eq!(serde_json::from_slice::<Manifest>(&json).unwrap(), m);
    }
}
