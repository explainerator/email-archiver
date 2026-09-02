//! Browser client for the mail archive.
//!
//! Three panes: folders, message list, reading pane. Read-only — the only write
//! the whole API offers is marking a message seen, and the schema enforces that
//! (WEBAPP-PLAN.md 5.3).
//!
//! Wire types come from `archive-api-types`, shared with the server, so a
//! renamed field breaks this build rather than failing at runtime.

mod api;

use archive_api_types::{
    Folder, Identity, Mailbox, MessageDetail, MessageSummary, SearchHit, MIN_SEARCH_LEN,
};
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

    // The term that has been SUBMITTED, kept apart from what is being typed:
    // searching per keystroke would fire a ~570 ms query per character.
    let mut query = use_signal(String::new);
    let mut typed = use_signal(String::new);

    let sign_out = move |_| async move {
        let _ = api::logout().await;
        auth.set(Auth::SignedOut);
    };

    let submit_search = move |event: Event<FormData>| {
        event.prevent_default();
        let term = typed().trim().to_string();
        // Checked here as well as on the server: one character matches most of
        // the archive, and there is no point paying for the round trip to be
        // told so.
        if term.chars().count() >= MIN_SEARCH_LEN {
            query.set(term);
            open.set(None);
        }
    };

    rsx! {
        div { class: "shell",
            header {
                strong { "Mail archive" }
                form { class: "search", onsubmit: submit_search,
                    input {
                        r#type: "search",
                        placeholder: "Search subject and sender",
                        value: "{typed}",
                        oninput: move |e| typed.set(e.value()),
                    }
                    if !query().is_empty() {
                        button {
                            r#type: "button",
                            class: "link",
                            onclick: move |_| {
                                query.set(String::new());
                                typed.set(String::new());
                            },
                            "Clear"
                        }
                    }
                }
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
                    match (query(), selected()) {
                        // Search spans every folder, so results take over the
                        // list rather than filtering within the selected one.
                        (q, _) if !q.is_empty() => rsx! {
                            SearchResults {
                                key: "{q}",
                                query: q,
                                opened: open(),
                                onopen: move |hash| open.set(Some(hash)),
                            }
                        },
                        (_, None) => rsx! { p { class: "muted pad", "Select a folder." } },
                        (_, Some(folder)) => rsx! {
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
                            let uid = message.uid;
                            let was_seen = message.seen;
                            move |_| {
                                onopen.call(hash.clone());
                                // Opening a message marks it read. Optimistic:
                                // the row updates immediately and the request
                                // follows, because waiting on a round trip to
                                // grey out a row you just clicked feels broken.
                                // A failure leaves read state stale, which is
                                // the mildest thing that can go wrong here.
                                if !was_seen {
                                    if let Some(row) =
                                        messages.write().iter_mut().find(|m| m.uid == uid)
                                    {
                                        row.seen = true;
                                    }
                                    spawn(async move {
                                        let _ = api::set_seen(id, uid, true).await;
                                    });
                                }
                            }
                        },
                        td { class: "clip",
                            // Titled so the column is not a mystery glyph to
                            // anyone using a screen reader.
                            if message.has_attachments {
                                span { title: "Has attachments", "\u{1F4CE}" }
                            }
                        }
                        td { class: "from", title: "{message.from.clone().unwrap_or_default()}",
                            // The display name is what people recognise; the
                            // address stays in the tooltip, because a name is
                            // sender-supplied and freely forgeable.
                            "{sender(&message)}"
                        }
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
    // Per message, not global: choosing to load one sender's images says
    // nothing about the next message, and resets when a different one opens.
    let mut remote = use_signal(|| false);

    let detail = use_resource(move || {
        let blake3 = blake3.clone();
        async move { api::message(&blake3, remote()).await }
    });

    rsx! {
        match &*detail.read_unchecked() {
            None => rsx! { p { class: "muted pad", "Loading…" } },
            Some(Err(e)) => rsx! { p { class: "error pad", "{e.message()}" } },
            Some(Ok(message)) => rsx! {
                MessageView {
                    message: message.clone(),
                    remote_loaded: remote(),
                    onload_remote: move |_| remote.set(true),
                }
            },
        }
    }
}

#[component]
fn MessageView(
    message: MessageDetail,
    remote_loaded: bool,
    onload_remote: EventHandler<()>,
) -> Element {
    // Plain text is the default view even when HTML exists: it is the sender's
    // own fallback, it renders identically everywhere, and it carries none of
    // the risk. Switching to HTML is a deliberate act.
    let mut show_html = use_signal(|| false);
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
                            a {
                                href: "{api::part_url(&message.blake3, part.index)}",
                                // The server always sends Content-Disposition:
                                // attachment, so this saves the file rather than
                                // opening whatever it claims to be.
                                download: "{part.filename.clone().unwrap_or_default()}",
                                "{part.filename.clone().unwrap_or_else(|| String::from(\"(unnamed)\"))}"
                            }
                            span { class: "muted", " {part.content_type} · {kilobytes(part.size)}" }
                        }
                    }
                }
            }

            // An HTML-only message has nothing else to show, so it starts on
            // the HTML view rather than an empty pane.
            {
                let html_only = message.text.is_none() && message.html.is_some();
                let showing_html = show_html() || html_only;

                rsx! {
                    if message.html.is_some() && message.text.is_some() {
                        div { class: "viewswitch",
                            button {
                                class: if showing_html { "" } else { "on" },
                                onclick: move |_| show_html.set(false),
                                "Plain text"
                            }
                            button {
                                class: if showing_html { "on" } else { "" },
                                onclick: move |_| show_html.set(true),
                                "HTML"
                            }
                        }
                    }

                    if showing_html {
                        if message.blocked_images > 0 && !remote_loaded {
                            div { class: "blocked",
                                span {
                                    "{message.blocked_images} remote image(s) blocked. "
                                    "Loading them tells the sender you opened this."
                                }
                                button { onclick: move |_| onload_remote.call(()), "Load images" }
                            }
                        }
                        // srcdoc + sandbox WITHOUT allow-scripts or
                        // allow-same-origin: the frame gets an opaque origin, so
                        // even markup that survived the sanitiser cannot run,
                        // reach the session cookie, or touch the DOM around it.
                        // The document carries its own CSP as well.
                        iframe {
                            class: "htmlframe",
                            // Written as string-literal attributes because
                            // Dioxus's iframe element does not define them.
                            // sandbox="" is the MOST restrictive value: every
                            // capability is withheld, including scripts and
                            // same-origin. Do not add allow-scripts or
                            // allow-same-origin here -- either one undoes the
                            // second layer of WEBAPP-PLAN.md 6.5 and would let
                            // surviving markup reach the session cookie.
                            "sandbox": "",
                            "srcdoc": "{message.html.clone().unwrap_or_default()}",
                            "referrerpolicy": "no-referrer",
                            title: "Message body",
                        }
                    } else {
                        match &message.text {
                            Some(text) => rsx! { pre { class: "body", "{text}" } },
                            None => rsx! {
                                p { class: "muted pad", "This message has no readable body." }
                            },
                        }
                    }
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

/// What to show in the list's sender column.
///
/// The display name when the message carried one, falling back to the address.
/// A name is sender-supplied and trivially forged, so the address is kept in
/// the row's tooltip rather than discarded.
fn sender(message: &MessageSummary) -> String {
    message
        .from_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| message.from.clone())
        .unwrap_or_else(|| String::from("(unknown sender)"))
}

#[component]
fn SearchResults(query: String, opened: Option<String>, onopen: EventHandler<String>) -> Element {
    let mut hits = use_signal(Vec::<SearchHit>::new);
    let mut next = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut loading = use_signal(|| true);

    // Keyed on the query by the caller, so a new search remounts this component
    // and the previous results go with it rather than being appended to.
    use_future({
        let query = query.clone();
        move || {
            let query = query.clone();
            async move {
                loading.set(true);
                match api::search(&query, None).await {
                    Ok(page) => {
                        hits.set(page.hits);
                        next.set(page.next);
                    }
                    Err(e) => error.set(Some(e.message())),
                }
                loading.set(false);
            }
        }
    });

    let load_more = {
        let query = query.clone();
        move |_| {
            let query = query.clone();
            async move {
                if loading() {
                    return;
                }
                let Some(cursor) = next() else { return };
                loading.set(true);
                match api::search(&query, Some(cursor)).await {
                    Ok(page) => {
                        hits.write().extend(page.hits);
                        next.set(page.next);
                    }
                    Err(e) => error.set(Some(e.message())),
                }
                loading.set(false);
            }
        }
    };

    // Computed outside rsx!: inside, a bare string literal is a text node, so a
    // method call cannot follow one. Reading the signals here is still
    // reactive -- the component re-runs when they change.
    let status = if loading() {
        String::from("searching...")
    } else {
        format!("{} shown", hits().len())
    };

    rsx! {
        div { class: "list-head",
            strong { "Search: {query}" }
            span { class: "muted", "{status}" }
        }
        if let Some(message) = error() {
            p { class: "error pad", "{message}" }
        }
        table {
            tbody {
                for hit in hits() {
                    tr {
                        key: "{hit.folder_id}-{hit.message.uid}",
                        class: if opened.as_deref() == Some(hit.message.blake3.as_str()) {
                            "open"
                        } else if hit.message.seen {
                            ""
                        } else {
                            "unseen"
                        },
                        onclick: {
                            let hash = hit.message.blake3.clone();
                            move |_| onopen.call(hash.clone())
                        },
                        td { class: "clip",
                            if hit.message.has_attachments {
                                span { title: "Has attachments", "\u{1F4CE}" }
                            }
                        }
                        td {
                            class: "from",
                            title: "{hit.message.from.clone().unwrap_or_default()}",
                            "{sender(&hit.message)}"
                        }
                        td { class: "subject",
                            "{hit.message.subject.clone().unwrap_or_else(|| String::from(\"(no subject)\"))}"
                            // Which folder the hit came from. Search spans all of
                            // them, and a receipt in `work` means something
                            // different from the same receipt in `personal`.
                            span { class: "infolder", " {hit.folder_path}" }
                        }
                        td { class: "date", "{short_date(&hit.message.date)}" }
                    }
                }
            }
        }
        if !loading() && hits().is_empty() {
            p { class: "muted pad", "No messages match." }
        }
        if next().is_some() {
            button { class: "more", onclick: load_more, disabled: loading(),
                if loading() { "Loading..." } else { "Load more" }
            }
        }
    }
}
