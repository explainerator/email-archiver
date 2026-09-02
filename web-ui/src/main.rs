//! Browser client for the mail archive.
//!
//! Three panes: folders, message list, reading pane. Read-only — the only write
//! the whole API offers is marking a message seen, and the schema enforces that
//! (WEBAPP-PLAN.md 5.3).
//!
//! Wire types come from `archive-api-types`, shared with the server, so a
//! renamed field breaks this build rather than failing at runtime.

mod api;

use archive_api_types::{Folder, Identity, Mailbox, MessageDetail, MessageSummary};
use dioxus::prelude::*;

const CSS: Asset = asset!("/assets/main.css");

fn main() {
    dioxus::launch(App);
}

/// Whether we know who the user is yet.
///
/// `Unknown` is a distinct state from `SignedOut`, not a detail: without it the
/// app flashes the login form on every load while the session check is in
/// flight, which reads as "you were logged out" to someone who was not.
#[derive(Clone, PartialEq)]
enum Auth {
    Unknown,
    SignedIn(Identity),
    SignedOut,
}

#[component]
fn App() -> Element {
    let mut auth = use_signal(|| Auth::Unknown);

    // Also refreshes the session cookie, which is what makes the 30-day expiry
    // sliding.
    use_future(move || async move {
        match api::session().await {
            Ok(identity) => auth.set(Auth::SignedIn(identity)),
            Err(_) => auth.set(Auth::SignedOut),
        }
    });

    rsx! {
        document::Stylesheet { href: CSS }
        match auth() {
            Auth::Unknown => rsx! { div { class: "centre muted", "Loading…" } },
            Auth::SignedOut => rsx! { Login { auth } },
            Auth::SignedIn(identity) => rsx! { Shell { identity, auth } },
        }
    }
}

#[component]
fn Login(auth: Signal<Auth>) -> Element {
    let mut login = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);

    let submit = move |event: Event<FormData>| {
        event.prevent_default();
        async move {
            if busy() {
                return;
            }
            busy.set(true);
            error.set(None);
            match api::login(login(), password()).await {
                Ok(identity) => auth.set(Auth::SignedIn(identity)),
                Err(e) => error.set(Some(e.message())),
            }
            // Cleared whatever happened: on success the component goes away, and
            // on failure the form must be usable again.
            password.set(String::new());
            busy.set(false);
        }
    };

    rsx! {
        div { class: "centre",
            form { class: "card", onsubmit: submit,
                h1 { "Mail archive" }
                label { r#for: "login", "Account" }
                input {
                    id: "login",
                    r#type: "text",
                    autocomplete: "username",
                    autofocus: true,
                    value: "{login}",
                    oninput: move |e| login.set(e.value()),
                }
                label { r#for: "password", "Password" }
                input {
                    id: "password",
                    r#type: "password",
                    autocomplete: "current-password",
                    value: "{password}",
                    oninput: move |e| password.set(e.value()),
                }
                if let Some(message) = error() {
                    p { class: "error", role: "alert", "{message}" }
                }
                button { r#type: "submit", disabled: busy(),
                    if busy() { "Signing in…" } else { "Sign in" }
                }
            }
        }
    }
}

#[component]
fn Shell(identity: Identity, auth: Signal<Auth>) -> Element {
    let folders = use_resource(api::folders);
    let mut selected = use_signal(|| None::<Folder>);
    let mut open = use_signal(|| None::<String>);

    let sign_out = move |_| async move {
        let _ = api::logout().await;
        auth.set(Auth::SignedOut);
    };

    rsx! {
        div { class: "shell",
            header {
                strong { "Mail archive" }
                span { class: "muted", "{identity.display_name}" }
                button { class: "link", onclick: sign_out, "Sign out" }
            }
            div { class: "panes",
                nav { class: "folders",
                    match &*folders.read_unchecked() {
                        None => rsx! { p { class: "muted", "Loading…" } },
                        Some(Err(e)) => rsx! { p { class: "error", "{e.message()}" } },
                        Some(Ok(list)) => rsx! {
                            for folder in list.clone() {
                                FolderRow {
                                    key: "{folder.id}",
                                    folder: folder.clone(),
                                    selected: selected().map(|f| f.id) == Some(folder.id),
                                    onselect: move |f| { selected.set(Some(f)); open.set(None); },
                                }
                            }
                        },
                    }
                }
                section { class: "list",
                    match selected() {
                        None => rsx! { p { class: "muted pad", "Select a folder." } },
                        Some(folder) => rsx! {
                            MessageList {
                                key: "{folder.id}",
                                folder,
                                opened: open(),
                                onopen: move |hash| open.set(Some(hash)),
                            }
                        },
                    }
                }
                section { class: "reader",
                    match open() {
                        None => rsx! { p { class: "muted pad", "Select a message." } },
                        Some(hash) => rsx! { Reader { key: "{hash}", blake3: hash } },
                    }
                }
            }
        }
    }
}

#[component]
fn FolderRow(folder: Folder, selected: bool, onselect: EventHandler<Folder>) -> Element {
    let chosen = folder.clone();
    rsx! {
        button {
            class: if selected { "folder chosen" } else { "folder" },
            onclick: move |_| onselect.call(chosen.clone()),
            span { class: "name", "{folder.path}" }
            if folder.unread > 0 {
                span { class: "badge", "{folder.unread}" }
            }
        }
    }
}

#[component]
fn MessageList(folder: Folder, opened: Option<String>, onopen: EventHandler<String>) -> Element {
    let mut messages = use_signal(Vec::<MessageSummary>::new);
    let mut next = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut loading = use_signal(|| false);

    let id = folder.id;

    // Keyed on the folder id by the caller, so switching folders remounts this
    // component and the accumulated pages go with it rather than being appended
    // to the next folder's list.
    use_future(move || async move {
        loading.set(true);
        match api::messages(id, None).await {
            Ok(page) => {
                messages.set(page.messages);
                next.set(page.next);
            }
            Err(e) => error.set(Some(e.message())),
        }
        loading.set(false);
    });

    let load_more = move |_| async move {
        if loading() {
            return;
        }
        let Some(cursor) = next() else { return };
        loading.set(true);
        match api::messages(id, Some(cursor)).await {
            Ok(page) => {
                messages.write().extend(page.messages);
                next.set(page.next);
            }
            Err(e) => error.set(Some(e.message())),
        }
        loading.set(false);
    };

    rsx! {
        div { class: "list-head",
            strong { "{folder.path}" }
            span { class: "muted", "{folder.total} messages" }
        }
        if let Some(message) = error() {
            p { class: "error pad", "{message}" }
        }
        table {
            tbody {
                for message in messages() {
                    tr {
                        key: "{message.uid}",
                        class: match (opened.as_deref() == Some(message.blake3.as_str()), message.seen) {
                            (true, _) => "open",
                            (false, false) => "unseen",
                            (false, true) => "",
                        },
                        onclick: {
                            let hash = message.blake3.clone();
                            move |_| onopen.call(hash.clone())
                        },
                        td { class: "from", "{message.from.clone().unwrap_or_default()}" }
                        td { class: "subject",
                            "{message.subject.clone().unwrap_or_else(|| String::from(\"(no subject)\"))}"
                        }
                        td { class: "date", "{short_date(&message.date)}" }
                    }
                }
            }
        }
        if next().is_some() {
            button { class: "more", onclick: load_more, disabled: loading(),
                if loading() { "Loading…" } else { "Load more" }
            }
        } else if !messages().is_empty() {
            p { class: "muted pad", "End of folder." }
        }
    }
}

/// `2026-09-02T18:50:03+00:00` -> `2026-09-02`.
///
/// Deliberately not a date library. The server sends RFC 3339, the date part is
/// fixed-width and leading, and pulling `chrono` into the wasm bundle to take a
/// prefix would be a poor trade.
fn short_date(rfc3339: &str) -> &str {
    rfc3339.split('T').next().unwrap_or(rfc3339)
}

#[component]
fn Reader(blake3: String) -> Element {
    let detail = use_resource(move || {
        let blake3 = blake3.clone();
        async move { api::message(&blake3).await }
    });

    rsx! {
        match &*detail.read_unchecked() {
            None => rsx! { p { class: "muted pad", "Loading…" } },
            Some(Err(e)) => rsx! { p { class: "error pad", "{e.message()}" } },
            Some(Ok(message)) => rsx! { MessageView { message: message.clone() } },
        }
    }
}

#[component]
fn MessageView(message: MessageDetail) -> Element {
    rsx! {
        article { class: "message",
            header {
                h2 { "{message.subject.clone().unwrap_or_else(|| String::from(\"(no subject)\"))}" }
                dl {
                    dt { "From" }
                    dd { "{addresses(&message.from)}" }
                    if !message.to.is_empty() {
                        dt { "To" }
                        dd { "{addresses(&message.to)}" }
                    }
                    if !message.cc.is_empty() {
                        dt { "Cc" }
                        dd { "{addresses(&message.cc)}" }
                    }
                    dt { "Date" }
                    dd { "{message.date}" }
                }
            }

            if !message.parts.is_empty() {
                ul { class: "parts",
                    for part in message.parts.iter() {
                        li { key: "{part.index}",
                            // Not links yet: the download endpoint is phase 5.
                            // Listing them is still worth doing -- knowing an
                            // attachment exists is most of the value.
                            span { "{part.filename.clone().unwrap_or_else(|| String::from(\"(unnamed)\"))}" }
                            span { class: "muted", " {part.content_type} · {kilobytes(part.size)}" }
                        }
                    }
                }
            }

            match &message.text {
                Some(text) => rsx! { pre { class: "body", "{text}" } },
                None if message.has_html => rsx! {
                    p { class: "muted pad",
                        "This message has no plain-text version. Rendering HTML safely is still to come."
                    }
                },
                None => rsx! { p { class: "muted pad", "This message has no readable body." } },
            }

            if message.text.is_some() && message.has_html {
                p { class: "muted pad note",
                    "Showing the plain-text version. An HTML version exists."
                }
            }
        }
    }
}

/// `Name <addr>` per mailbox, comma separated. Falls back to whichever half is
/// present, since either can be missing.
fn addresses(list: &[Mailbox]) -> String {
    list.iter()
        .map(|m| match (&m.name, &m.email) {
            (Some(name), Some(email)) => format!("{name} <{email}>"),
            (Some(name), None) => name.clone(),
            (None, Some(email)) => email.clone(),
            (None, None) => String::from("(unknown)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn kilobytes(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
