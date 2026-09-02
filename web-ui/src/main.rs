//! Browser client for the mail archive.
//!
//! Three panes: folders, message list, reading pane. Read-only — the only write
//! the whole API offers is marking a message seen, and the schema enforces that
//! (WEBAPP-PLAN.md 5.3).
//!
//! Wire types come from `archive-api-types`, shared with the server, so a
//! renamed field breaks this build rather than failing at runtime.

mod api;

use archive_api_types::{Folder, Identity, MessageSummary};
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
                                    onselect: move |f| selected.set(Some(f)),
                                }
                            }
                        },
                    }
                }
                section { class: "list",
                    match selected() {
                        None => rsx! { p { class: "muted pad", "Select a folder." } },
                        Some(folder) => rsx! { MessageList { key: "{folder.id}", folder } },
                    }
                }
                section { class: "reader",
                    p { class: "muted pad", "Select a message." }
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
fn MessageList(folder: Folder) -> Element {
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
                    tr { key: "{message.uid}", class: if message.seen { "" } else { "unseen" },
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
