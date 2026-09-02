//! Wire types shared by the HTTP API and the web client.
//!
//! This crate exists so the JSON contract is written once. Both sides depend on
//! it by path, so a field renamed here fails to compile on whichever side has
//! not caught up — which is the whole reason WEBAPP-PLAN.md 3.3 chose a Rust
//! frontend over a TypeScript one. Hand-maintained interfaces on the client
//! drift silently; these cannot.
//!
//! Keep this crate free of anything that will not build for `wasm32`: no
//! sqlx, no tokio, no filesystem. It is types and `serde`, nothing else.

use serde::{Deserialize, Serialize};

/// `POST /api/login`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub login: String,
    pub password: String,
}

/// Who the caller is. Returned by `POST /api/login` and `GET /api/session`.
///
/// Deliberately thin. The client needs a name to show and nothing else — no
/// user id, no bucket. The id lives in the signed cookie and the bucket is a
/// storage detail the browser has no use for, so neither is exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub login: String,
    pub display_name: String,
}

/// Error body for any non-2xx JSON response.
///
/// One shape for every failure, so the client has one thing to parse. The
/// message is safe to display: handlers deliberately do not put internal detail
/// in it — a wrong password and an unknown login produce identical text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

// ---------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------

/// One folder in the user's archive.
///
/// `path` is the namespaced name the IMAP server also serves (`work/INBOX`), so
/// the two clients agree on what a folder is called. The account label is sent
/// separately as well, because a web client may reasonably want to group by
/// account rather than show the raw path (WEBAPP-PLAN.md Q3) and should not
/// have to re-split the string to do it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: i64,
    pub account: String,
    pub path: String,
    pub total: i64,
    pub unread: i64,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A row in the message list. Everything here comes from indexed columns on
/// `messages`, so rendering a page never touches S3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSummary {
    /// Per-folder IMAP UID. Together with the folder it identifies the
    /// placement, which is what read state hangs off.
    pub uid: i64,
    /// Content address of the message body, for fetching and for part URLs.
    pub blake3: String,
    pub subject: Option<String>,
    /// The sender's address.
    pub from: Option<String>,
    /// The sender's display name, when the message carried one.
    ///
    /// Sent alongside the address rather than instead of it: the list shows the
    /// name because it is what people recognise, but a name is sender-supplied
    /// and freely forgeable, so the address has to remain available.
    pub from_name: Option<String>,
    /// RFC 3339. A string rather than a timestamp so the crate stays free of a
    /// date library that both sides would have to agree on.
    pub date: String,
    pub size: i64,
    pub seen: bool,
    /// Whether any part is an attachment, for the list's paperclip column.
    pub has_attachments: bool,
}

/// One page of a folder, plus how to ask for the next.
///
/// Keyset pagination, not offset: the main INBOX has ~53,000 messages, and
/// `OFFSET 50000` re-walks every skipped row. See WEBAPP-PLAN.md 5.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePage {
    pub messages: Vec<MessageSummary>,
    /// Opaque cursor for the next page, or `None` at the end. Clients must pass
    /// it back verbatim rather than constructing one -- the encoding is the
    /// server's business and is free to change.
    pub next: Option<String>,
}

/// A named mailbox, as displayed in the reading pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    pub name: Option<String>,
    pub email: Option<String>,
}

/// A non-body part: attachment, or an inline image the body refers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    /// Index within the message, used to address the part for download.
    pub index: usize,
    pub filename: Option<String>,
    /// The part's DECLARED content type, shown for information only.
    ///
    /// Never trusted when serving bytes: the sender controls it, so the
    /// download endpoint sets `Content-Type` from its own extension mapping and
    /// the inline-image endpoint sniffs magic bytes. See WEBAPP-PLAN.md 6.4/7.
    pub content_type: String,
    pub size: i64,
}

/// One message, opened.
///
/// Phase 4a carries the plain-text body only. `has_html` says an HTML
/// alternative exists without shipping it, so the client can be honest about
/// what it is not yet showing; rendering that safely is phase 4b (§6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDetail {
    pub blake3: String,
    pub subject: Option<String>,
    pub from: Vec<Mailbox>,
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    /// RFC 3339, from the archive's `internaldate`.
    pub date: String,
    pub size: i64,
    /// The `text/plain` part, if the message has one.
    pub text: Option<String>,
    /// Whether a `text/html` alternative exists.
    pub has_html: bool,
    /// The HTML body, sanitised and wrapped in a complete document for the
    /// reading frame -- including its own Content-Security-Policy.
    ///
    /// The client must render this in a sandboxed iframe via `srcdoc` and must
    /// never inject it into the page. It is safe because of what was removed,
    /// not because of what it is, and the sandbox is a deliberate second layer
    /// over the sanitiser (WEBAPP-PLAN.md 6.5).
    pub html: Option<String>,
    /// Remote images blocked while sanitising. Non-zero means the client should
    /// offer to load them, rather than silently hiding that anything was cut.
    pub blocked_images: usize,
    pub parts: Vec<Part>,
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// One search result: the message, plus which folder it was found in.
///
/// Search spans every folder, so a hit without its location would leave the
/// reader unable to tell a work receipt from a personal one, or to go and see
/// the surrounding thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub message: MessageSummary,
    pub folder_id: i64,
    pub folder_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub next: Option<String>,
}

/// Shortest query accepted.
///
/// A single character matches essentially the whole archive, which is a slow
/// query returning nothing useful. Enforced on both sides: the client to avoid
/// the round trip, the server because the client is not the only caller.
pub const MIN_SEARCH_LEN: usize = 2;
