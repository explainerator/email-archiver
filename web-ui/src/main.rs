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
use std::collections::HashSet;

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
                label { r#for: "login", "Email address" }
                input {
                    id: "login",
                    // type=email so phones offer the right keyboard, and
                    // autocomplete=email so password managers fill it. The
                    // login IS the address; there is no separate username.
                    r#type: "email",
                    autocomplete: "email",
                    autocapitalize: "none",
                    spellcheck: "false",
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
    // Held here rather than in the lists: switching folders or running a
    // search remounts those, and a layout preference that reset itself
    // every time you clicked a folder would be worse than no preference.
    let view = use_signal(RowView::load);

    // The term that has been SUBMITTED, kept apart from what is being typed:
    // searching per keystroke would fire a ~570 ms query per character.
    let mut query = use_signal(String::new);
    let mut typed = use_signal(String::new);

    // Which nodes are open, by full path. Empty means everything collapsed,
    // which is the default: 46 folders four levels deep is a wall of text, and
    // two account rows is a place to start from.
    let mut expanded = use_signal(HashSet::<String>::new);

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
                            for node in build_tree(list.clone()) {
                                FolderNode {
                                    key: "{node.path}",
                                    node,
                                    depth: 0,
                                    expanded,
                                    selected: selected().map(|f| f.id),
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
                                view,
                            }
                        },
                        (_, None) => rsx! { div { class: "empty", "Select a folder to begin." } },
                        (_, Some(folder)) => rsx! {
                            MessageList {
                                key: "{folder.id}",
                                folder,
                                opened: open(),
                                onopen: move |hash| open.set(Some(hash)),
                                view,
                            }
                        },
                    }
                }
                section { class: "reader",
                    match open() {
                        None => rsx! { div { class: "empty", "Select a message to read it." } },
                        Some(hash) => rsx! { Reader { key: "{hash}", blake3: hash } },
                    }
                }
            }
        }
    }
}

/// One node of the folder tree.
///
/// A node is not the same thing as a folder. `personal/Archives` holds 15 years
/// of mail in children but contains nothing itself, and would not appear in a
/// flat list at all if the server had not returned it -- while `personal/INBOX`
/// is both a real folder of 53,573 messages AND a parent. So `folder` is
/// optional and `children` is independent of it.
#[derive(Clone, PartialEq)]
struct Node {
    /// Just this segment, which is what gets displayed.
    label: String,
    /// The full path, used as the identity for expansion state.
    path: String,
    folder: Option<Folder>,
    children: Vec<Node>,
}

impl Node {
    /// Unread in this node and everything under it.
    ///
    /// A collapsed parent has to answer for its children: a `Junk` folder with
    /// 3,971 unread hidden two levels down is exactly the thing a collapsed
    /// tree must not conceal.
    fn unread_total(&self) -> i64 {
        self.folder.as_ref().map_or(0, |f| f.unread)
            + self.children.iter().map(Node::unread_total).sum::<i64>()
    }

    fn has_children(&self) -> bool {
        !self.children.is_empty()
    }
}

/// Turn the server's flat list of `account/path/to/folder` into a tree.
///
/// Intermediate segments that the server never sent as folders still become
/// nodes, so a child is never orphaned by a missing parent.
fn build_tree(folders: Vec<Folder>) -> Vec<Node> {
    let mut roots: Vec<Node> = Vec::new();

    // Sorted first so insertion order does not decide sibling order, and so
    // parents are created before the children that need them.
    let mut folders = folders;
    folders.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));

    for folder in folders {
        let segments: Vec<&str> = folder
            .path
            .split(SEPARATOR)
            .filter(|s| !s.is_empty())
            .collect();
        if segments.is_empty() {
            continue;
        }

        let mut level = &mut roots;
        let mut prefix = String::new();

        for (i, segment) in segments.iter().enumerate() {
            if !prefix.is_empty() {
                prefix.push(SEPARATOR);
            }
            prefix.push_str(segment);

            let existing = level.iter().position(|n| n.label == *segment);
            let index = match existing {
                Some(index) => index,
                None => {
                    level.push(Node {
                        label: (*segment).to_string(),
                        path: prefix.clone(),
                        folder: None,
                        children: Vec::new(),
                    });
                    level.len() - 1
                }
            };

            if i == segments.len() - 1 {
                level[index].folder = Some(folder.clone());
            }
            level = &mut level[index].children;
        }
    }

    sort_tree(&mut roots);
    roots
}

const SEPARATOR: char = '/';

/// INBOX first, then everything else alphabetically.
///
/// Matching what every mail client does: INBOX is the one folder people look
/// for by position rather than by reading.
fn sort_tree(nodes: &mut Vec<Node>) {
    nodes.sort_by(|a, b| {
        let rank = |n: &Node| {
            if n.label.eq_ignore_ascii_case("INBOX") {
                0
            } else {
                1
            }
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    for node in nodes.iter_mut() {
        sort_tree(&mut node.children);
    }
}

#[component]
fn FolderNode(
    node: Node,
    depth: usize,
    expanded: Signal<HashSet<String>>,
    selected: Option<i64>,
    onselect: EventHandler<Folder>,
) -> Element {
    let is_open = expanded.read().contains(&node.path);
    let is_selected = node.folder.as_ref().map(|f| f.id) == selected && selected.is_some();

    // Collapsed nodes answer for their subtree; expanded ones show only their
    // own, since the children are on screen speaking for themselves.
    let badge = if is_open {
        node.folder.as_ref().map_or(0, |f| f.unread)
    } else {
        node.unread_total()
    };

    let toggle_path = node.path.clone();
    let toggle = move |event: Event<MouseData>| {
        // Stops the row's own click handler from also firing and selecting the
        // folder: expanding a parent and opening it are different intents.
        event.stop_propagation();
        let mut set = expanded.write();
        if !set.remove(&toggle_path) {
            set.insert(toggle_path.clone());
        }
    };

    let chosen = node.folder.clone();
    let fallback_path = node.path.clone();
    let activate = move |_| match chosen.clone() {
        Some(folder) => onselect.call(folder),
        // A node with no folder of its own has nothing to open, so clicking it
        // does the only useful thing available.
        None => {
            let mut set = expanded.write();
            if !set.remove(&fallback_path) {
                set.insert(fallback_path.clone());
            }
        }
    };

    rsx! {
        div {
            class: if is_selected { "folder chosen" } else { "folder" },
            style: "padding-left: {0.4 + depth as f32 * 0.85}rem",
            onclick: activate,

            if node.has_children() {
                button {
                    class: "twisty",
                    // The row is a div rather than a button so this can be a
                    // button inside it; nested buttons are invalid HTML and
                    // browsers recover from them unpredictably.
                    "aria-expanded": if is_open { "true" } else { "false" },
                    "aria-label": if is_open { "Collapse" } else { "Expand" },
                    onclick: toggle,
                    if is_open { "\u{25BE}" } else { "\u{25B8}" }
                }
            } else {
                span { class: "twisty spacer" }
            }

            span {
                class: if node.folder.is_some() { "name" } else { "name container" },
                title: "{node.path}",
                "{node.label}"
            }
            if badge > 0 {
                span { class: "badge", "{badge}" }
            }
        }

        if is_open {
            for child in node.children.iter() {
                FolderNode {
                    key: "{child.path}",
                    node: child.clone(),
                    depth: depth + 1,
                    expanded,
                    selected,
                    onselect,
                }
            }
        }
    }
}

#[component]
fn MessageList(
    folder: Folder,
    opened: Option<String>,
    onopen: EventHandler<String>,
    view: Signal<RowView>,
) -> Element {
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
            ViewToggle { view }
        }
        if let Some(message) = error() {
            p { class: "error pad", "{message}" }
        }
        // Two presentations of the same rows, chosen by the reader. Both go
        // through open_message, so only the layout differs -- the behaviour
        // cannot drift apart between them.
        if view() == RowView::Table {
            table {
                tbody {
                    for message in messages() {
                        tr {
                            key: "{message.uid}",
                            class: state_class("", opened.as_deref() == Some(message.blake3.as_str()), message.seen),
                            onclick: {
                                let row = message.clone();
                                move |_| open_message(messages, onopen, id, &row)
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
                            td { class: "date", "{smart_date(&message.date)}" }
                        }
                    }
                }
            }
        } else {
            div { class: "cards",
                for message in messages() {
                    div {
                        key: "{message.uid}",
                        class: state_class("crow", opened.as_deref() == Some(message.blake3.as_str()), message.seen),
                        onclick: {
                            let row = message.clone();
                            move |_| open_message(messages, onopen, id, &row)
                        },
                        Avatar { seed: avatar_seed(&message), label: sender(&message) }
                        div { class: "cmain",
                            div { class: "cline1",
                                span {
                                    class: "cwho",
                                    title: "{message.from.clone().unwrap_or_default()}",
                                    "{sender(&message)}"
                                }
                                span { class: "cwhen", "{smart_date(&message.date)}" }
                            }
                            div { class: "cline2",
                                span { class: "csubj",
                                    "{message.subject.clone().unwrap_or_else(|| String::from(\"(no subject)\"))}"
                                }
                                if message.has_attachments {
                                    span { class: "cclip", title: "Has attachments", "\u{1F4CE}" }
                                }
                            }
                        }
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

/// Which presentation the message list uses.
///
/// A preference rather than a redesign. The table is denser and lines its
/// columns up for comparison; the cards put the correspondent first and are
/// quicker to scan for a person. People genuinely disagree about which they
/// want, so both stay and the choice belongs to the reader.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowView {
    Table,
    Cards,
}

const VIEW_KEY: &str = "archive.rowview";

impl RowView {
    /// Cards by default, because it is the friendlier of the two on first
    /// sight. Anyone who prefers the table has to say so exactly once.
    fn load() -> Self {
        match storage().and_then(|s| s.get_item(VIEW_KEY).ok().flatten()) {
            Some(choice) if choice == "table" => RowView::Table,
            _ => RowView::Cards,
        }
    }

    fn save(self) {
        if let Some(store) = storage() {
            let _ = store.set_item(
                VIEW_KEY,
                match self {
                    RowView::Table => "table",
                    RowView::Cards => "cards",
                },
            );
        }
    }
}

/// Local storage, if this host has any.
///
/// Fallible twice over -- no window outside a browser, and a browser that
/// refuses storage outright in private mode -- and neither failure deserves to
/// be surfaced. Forgetting a layout preference is not something to interrupt
/// anyone about.
fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
/// Indexed with Sunday at 0, which is what the epoch offset in `smart_date`
/// produces.
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// A date a person can read at a glance.
///
/// Today gives the time, because for today's mail the hour is the useful part;
/// the past week gives the weekday; older gives the date. A column of
/// `2026-09-02` on every row is accurate and says almost nothing at a glance.
///
/// Still not a date library. `js_sys` is already in every wasm build, and the
/// civil-date arithmetic below is a dozen lines, so `chrono` would be a large
/// bundle bought for a small sum.
///
/// The message keeps the Y-M-D its own timestamp carries, which is the SENDER's
/// offset, while "today" is the reader's. That is the convention the fixed date
/// column already used, and the alternative -- restating each timestamp in the
/// reader's zone -- would relabel the day a message was sent.
fn smart_date(rfc3339: &str) -> String {
    let Some((y, m, d)) = date_parts(rfc3339) else {
        return rfc3339.to_string();
    };
    let now = js_sys::Date::new_0();
    let now_year = now.get_full_year() as i64;
    let then = days_from_civil(y, m, d);
    let today = days_from_civil(now_year, now.get_month() as i64 + 1, now.get_date() as i64);
    let month = MONTHS.get((m - 1) as usize).copied().unwrap_or("");

    match today - then {
        0 => time_part(rfc3339).unwrap_or_else(|| String::from("Today")),
        1 => String::from("Yesterday"),
        // Weekdays only look backwards. A date in the future is a clock wrong
        // somewhere, and "Thursday" would hide that rather than show it.
        2..=6 => String::from(WEEKDAYS[(then + 4).rem_euclid(7) as usize]),
        _ if y == now_year => format!("{d} {month}"),
        _ => format!("{d} {month} {y}"),
    }
}

/// `2026-09-02T18:50:03+00:00` -> `(2026, 9, 2)`.
fn date_parts(rfc3339: &str) -> Option<(i64, i64, i64)> {
    let mut parts = rfc3339.split('T').next()?.split('-');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

/// `2026-09-02T18:50:03+00:00` -> `18:50`.
fn time_part(rfc3339: &str) -> Option<String> {
    let mut parts = rfc3339.split('T').nth(1)?.split(':');
    let hour = parts.next()?;
    let minute = parts.next()?;
    (hour.len() == 2 && minute.len() == 2).then(|| format!("{hour}:{minute}"))
}

/// Days from 1970-01-01 to a civil date.
///
/// Howard Hinnant's algorithm: exact for any proleptic Gregorian date, no
/// lookup tables, and short enough to read.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// One letter for the avatar, falling back rather than showing a blank circle.
fn monogram(label: &str) -> String {
    label
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| String::from("?"))
}

/// A stable hue per correspondent.
///
/// FNV-1a because the only requirement is that one sender always lands on one
/// colour. This picks a swatch; it protects nothing, and should not be mistaken
/// for something that does.
fn hue(seed: &str) -> u32 {
    let mut hash: u32 = 2166136261;
    for byte in seed.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash % 360
}

/// Seeded on the ADDRESS wherever there is one, so a sender who changes their
/// display name keeps the colour you have learned to recognise them by.
fn avatar_seed(message: &MessageSummary) -> String {
    message
        .from
        .clone()
        .or_else(|| message.from_name.clone())
        .unwrap_or_default()
}

/// Row classes shared by both presentations, so "open" and "unread" cannot
/// drift apart between them.
fn state_class(base: &str, opened: bool, seen: bool) -> String {
    let state = match (opened, seen) {
        (true, _) => "open",
        (false, false) => "unseen",
        (false, true) => "",
    };
    match (base.is_empty(), state.is_empty()) {
        (true, _) => state.to_string(),
        (false, true) => base.to_string(),
        (false, false) => format!("{base} {state}"),
    }
}

/// Open a message, and mark it read on the way.
///
/// A free function rather than a closure inside the component, because both
/// presentations call it. Two copies of "and also mark it seen" is exactly how
/// the table and the cards would quietly stop agreeing with each other.
fn open_message(
    mut messages: Signal<Vec<MessageSummary>>,
    onopen: EventHandler<String>,
    folder_id: i64,
    message: &MessageSummary,
) {
    onopen.call(message.blake3.clone());
    if message.seen {
        return;
    }
    // Optimistic: the row updates immediately and the request follows, because
    // waiting on a round trip to grey out a row you just clicked feels broken.
    // A failure leaves read state stale, which is the mildest thing that can go
    // wrong here.
    let uid = message.uid;
    if let Some(row) = messages.write().iter_mut().find(|m| m.uid == uid) {
        row.seen = true;
    }
    spawn(async move {
        let _ = api::set_seen(folder_id, uid, true).await;
    });
}

/// The switch between the two list presentations.
///
/// A segmented control rather than two buttons: it is one decision with two
/// answers, and the pressed state has to say which answer is currently in
/// force. Icons alone, because the choice is visual and a word for each would
/// be wider than the thing it describes -- but each carries a title and
/// `aria-pressed`, so it is neither a mystery glyph nor invisible to a screen
/// reader.
#[component]
fn ViewToggle(mut view: Signal<RowView>) -> Element {
    rsx! {
        div { class: "viewtoggle", role: "group", aria_label: "Message list layout",
            button {
                r#type: "button",
                class: if view() == RowView::Table { "on" } else { "" },
                title: "Table",
                aria_pressed: "{view() == RowView::Table}",
                onclick: move |_| {
                    view.set(RowView::Table);
                    RowView::Table.save();
                },
                svg {
                    view_box: "0 0 16 16",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "1.6",
                    stroke_linecap: "round",
                    path { d: "M2.5 4h11M2.5 8h11M2.5 12h11" }
                }
            }
            button {
                r#type: "button",
                class: if view() == RowView::Cards { "on" } else { "" },
                title: "Cards",
                aria_pressed: "{view() == RowView::Cards}",
                onclick: move |_| {
                    view.set(RowView::Cards);
                    RowView::Cards.save();
                },
                svg {
                    view_box: "0 0 16 16",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "1.5",
                    stroke_linecap: "round",
                    circle { cx: "4", cy: "4.6", r: "2.1" }
                    path { d: "M8.6 3.4h5M8.6 6h3.2" }
                    circle { cx: "4", cy: "11.4", r: "2.1" }
                    path { d: "M8.6 10.2h5M8.6 12.8h3.2" }
                }
            }
        }
    }
}

/// The sender's initial, on a colour that is theirs.
#[component]
fn Avatar(seed: String, label: String) -> Element {
    rsx! {
        span {
            class: "avatar",
            style: "--h: {hue(&seed)}",
            // Decorative: the sender's name is the very next thing read out.
            aria_hidden: "true",
            "{monogram(&label)}"
        }
    }
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
fn SearchResults(
    query: String,
    opened: Option<String>,
    onopen: EventHandler<String>,
    view: Signal<RowView>,
) -> Element {
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
            ViewToggle { view }
        }
        if let Some(message) = error() {
            p { class: "error pad", "{message}" }
        }
        if view() == RowView::Table {
            table {
                tbody {
                    for hit in hits() {
                        tr {
                            key: "{hit.folder_id}-{hit.message.uid}",
                            class: state_class("", opened.as_deref() == Some(hit.message.blake3.as_str()), hit.message.seen),
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
                            td { class: "date", "{smart_date(&hit.message.date)}" }
                        }
                    }
                }
            }
        } else {
            div { class: "cards",
                for hit in hits() {
                    div {
                        key: "{hit.folder_id}-{hit.message.uid}",
                        class: state_class("crow", opened.as_deref() == Some(hit.message.blake3.as_str()), hit.message.seen),
                        onclick: {
                            let hash = hit.message.blake3.clone();
                            move |_| onopen.call(hash.clone())
                        },
                        Avatar { seed: avatar_seed(&hit.message), label: sender(&hit.message) }
                        div { class: "cmain",
                            div { class: "cline1",
                                span {
                                    class: "cwho",
                                    title: "{hit.message.from.clone().unwrap_or_default()}",
                                    "{sender(&hit.message)}"
                                }
                                span { class: "cwhen", "{smart_date(&hit.message.date)}" }
                            }
                            div { class: "cline2",
                                span { class: "csubj",
                                    "{hit.message.subject.clone().unwrap_or_else(|| String::from(\"(no subject)\"))}"
                                }
                                span { class: "infolder", "{hit.folder_path}" }
                                if hit.message.has_attachments {
                                    span { class: "cclip", title: "Has attachments", "\u{1F4CE}" }
                                }
                            }
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: i64, path: &str, unread: i64) -> Folder {
        Folder {
            id,
            account: path.split('/').next().unwrap_or("").to_string(),
            path: path.to_string(),
            total: 0,
            unread,
        }
    }

    fn find<'a>(nodes: &'a [Node], path: &str) -> Option<&'a Node> {
        for node in nodes {
            if node.path == path {
                return Some(node);
            }
            if let Some(found) = find(&node.children, path) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn nests_by_path_segment() {
        let tree = build_tree(vec![
            folder(1, "personal/Archives/2019", 0),
            folder(2, "personal/INBOX", 0),
        ]);
        assert_eq!(tree.len(), 1, "expected one account root");
        assert_eq!(tree[0].label, "personal");
        assert!(find(&tree, "personal/Archives/2019").is_some());
    }

    #[test]
    fn invents_missing_parents() {
        // The real archive has personal/Archives/qra/2014/Sent where some
        // intermediate levels hold no mail of their own. A child must never be
        // orphaned because the server did not list its parent.
        let tree = build_tree(vec![folder(1, "personal/Archives/qra/2014/Sent", 3)]);
        let qra = find(&tree, "personal/Archives/qra").expect("parent invented");
        assert!(
            qra.folder.is_none(),
            "invented parent must not claim to be a folder"
        );
        assert!(find(&tree, "personal/Archives/qra/2014/Sent").is_some());
    }

    #[test]
    fn a_node_can_be_both_folder_and_parent() {
        // personal/INBOX holds 53,573 messages AND has children.
        let tree = build_tree(vec![
            folder(1, "personal/INBOX", 7),
            folder(2, "personal/INBOX/Sent", 0),
        ]);
        let inbox = find(&tree, "personal/INBOX").unwrap();
        assert!(inbox.folder.is_some(), "lost its own folder");
        assert!(inbox.has_children(), "lost its children");
    }

    #[test]
    fn collapsed_parents_answer_for_their_children() {
        // The property that makes collapsing safe: 3,971 unread hidden two
        // levels down must still be visible on the collapsed ancestor.
        let tree = build_tree(vec![
            folder(1, "personal/Junk", 3971),
            folder(2, "personal/Archives/2019", 1765),
        ]);
        assert_eq!(tree[0].unread_total(), 3971 + 1765);
    }

    #[test]
    fn inbox_sorts_first_then_alphabetical() {
        let tree = build_tree(vec![
            folder(1, "a/Zebra", 0),
            folder(2, "a/apple", 0),
            folder(3, "a/INBOX", 0),
        ]);
        let labels: Vec<&str> = tree[0].children.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["INBOX", "apple", "Zebra"]);
    }

    #[test]
    fn every_node_has_a_unique_path_for_expansion_state() {
        // Expansion is keyed by path, so a duplicate would make two nodes open
        // and close together.
        let tree = build_tree(vec![folder(1, "a/x/Sent", 0), folder(2, "b/x/Sent", 0)]);
        let mut seen = std::collections::HashSet::new();
        fn walk(nodes: &[Node], seen: &mut std::collections::HashSet<String>) {
            for n in nodes {
                assert!(seen.insert(n.path.clone()), "duplicate path {}", n.path);
                walk(&n.children, seen);
            }
        }
        walk(&tree, &mut seen);
        assert_eq!(seen.len(), 6);
    }
}

/// The date and avatar helpers, which are pure and so can be checked here.
///
/// `smart_date` itself mostly cannot: it asks `js_sys` for today, and there is
/// no JS runtime under `cargo test`. Everything it relies on to reach an answer
/// is tested instead, which is where the arithmetic that could actually be
/// wrong lives.
#[cfg(test)]
mod rows {
    use super::*;

    #[test]
    fn civil_days_anchor_on_the_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        // Thirty years, seven of them leap.
        assert_eq!(days_from_civil(2000, 1, 1), 10957);
    }

    #[test]
    fn weekday_offset_names_the_right_day() {
        // The epoch was a Thursday and 2000-01-01 a Saturday. If the +4 in
        // smart_date is ever "simplified" away, these are what catch it.
        let epoch = days_from_civil(1970, 1, 1);
        assert_eq!(WEEKDAYS[(epoch + 4).rem_euclid(7) as usize], "Thursday");
        let millennium = days_from_civil(2000, 1, 1);
        assert_eq!(
            WEEKDAYS[(millennium + 4).rem_euclid(7) as usize],
            "Saturday"
        );
    }

    #[test]
    fn parses_the_wire_timestamp() {
        assert_eq!(date_parts("2026-09-02T18:50:03+00:00"), Some((2026, 9, 2)));
        assert_eq!(
            time_part("2026-09-02T18:50:03+00:00").as_deref(),
            Some("18:50")
        );
        assert_eq!(date_parts("not a date"), None);
        assert_eq!(time_part("2026-09-02"), None);
    }

    #[test]
    fn monograms_fall_back_rather_than_blank() {
        assert_eq!(monogram("Ken Duck"), "K");
        assert_eq!(monogram("ken@twoducks.ca"), "K");
        // Leading punctuation is skipped, not displayed.
        assert_eq!(monogram("\"Ken\""), "K");
        assert_eq!(monogram(""), "?");
        assert_eq!(monogram("   "), "?");
    }

    #[test]
    fn one_sender_keeps_one_hue() {
        assert_eq!(hue("ken@twoducks.ca"), hue("ken@twoducks.ca"));
        assert_ne!(hue("ken@twoducks.ca"), hue("art@jduck.ca"));
        for seed in ["", "a", "ken@twoducks.ca", "\u{1F600}"] {
            assert!(hue(seed) < 360, "hue out of range for {seed:?}");
        }
    }

    #[test]
    fn row_state_is_the_same_in_both_layouts() {
        assert_eq!(state_class("", true, false), "open");
        assert_eq!(state_class("", false, false), "unseen");
        assert_eq!(state_class("", false, true), "");
        assert_eq!(state_class("crow", true, true), "crow open");
        assert_eq!(state_class("crow", false, false), "crow unseen");
        assert_eq!(state_class("crow", false, true), "crow");
    }
}
