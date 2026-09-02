# archive-web — a browser client for the mail archive

Status: **All phases done.** A Dioxus app with login, folder pane and paged message
list and a reading pane showing plain text or sanitised HTML, served by
`email-archiver serve-web`, with attachment downloads, read state and search, served over TLS on 443 by its own
systemd unit.

Build and run locally:

```
cd web-ui && dx build --platform web --release
email-archiver serve-web --assets web-ui/target/dx/archive-web-ui/release/web/public
```

Then <http://127.0.0.1:8000>. **Use `--release` for anything reachable by another person:**
the debug bundle carries Dioxus's devtools shell, which `@import`s a Google font — an
external request on every page load, which both breaks the phase 4b CSP and leaks the
reader's IP and visit to a third party. That would be an odd thing to allow in an app that
blocks remote images as tracking pixels. The release bundle has no external references at
all.
Companion to `ARCHIVE-PLAN.md`, which stays the authority on storage, ingest and IMAP.

---

## 1. What this is

A small web client so the users who live in webmail can read their archive. Login page,
then three panes: folders on the left, a message list in the middle (from, subject, date),
a reading pane on the right.

It exists because of an asymmetry we already knew about but had not priced. Thunderbird
users hold a full local copy of their archive; the archive is a *backup* to them. Webmail
users hold nothing locally — for them this app **is** the archive, and it is the only way
they will ever see mail from an account that has since been closed.

That difference drives three decisions later in this document: search stops being optional
(§8), the reading pane has to render hostile HTML properly rather than showing plain text
(§6), and attachments have to be downloadable (§7).

---

## 2. Not IMAP

> "Since it will be done in dioxus, you should use the IMAP libraries you already know
> about."

**The browser cannot speak IMAP, so the webapp will not use one.** IMAP is a TCP protocol;
WASM in a browser tab has no raw sockets, only HTTP and WebSockets. There is no build of
`async-imap` or `imap-next` that changes this — the limitation is the browser sandbox, not
the crates.

It would be a detour even if it worked. The data is in Postgres and S3. A request path of
*browser → HTTP → server → IMAP → same server → Postgres* would serialise our own data
into IMAP wire format purely to parse it back. The IMAP server exists to satisfy
Thunderbird, which cannot speak anything else. The webapp has no such constraint.

`imap-next` and `async-imap` stay exactly where they are — serving Thunderbird and pulling
from source mailboxes respectively.

---

## 3. Architecture

### 3.1 A REST API in the existing binary

**`email-archiver serve-web` — a new subcommand beside `serve`.** No new crate, no
refactor.

The endpoints reuse `db`, `store` and `fetch` directly, exactly as `server.rs` does. They
also inherit `Config::load`, `connect_db` (so migrations run, and the `current_database()
= 'archive'` guard applies) and the existing `CertReloader` for free.

An earlier draft of this plan proposed splitting a shared `archive-core` library out
first, and argued that one binary serving both IMAP and HTTP would repeat the problem that
made Stalwart unworkable. **That argument was wrong** and is withdrawn: Stalwart's problem
was its configuration model, not that it was a single process. This binary already holds
the CLI, ingest and an IMAP server; an HTTP handler is not a categorical change. Splitting
later, with six months of evidence about what actually grew, is a better-informed decision
than splitting now — and it removes a refactor of working code from the critical path.

### 3.2 REST, not server functions

Dioxus fullstack's server functions earn their keep when the API surface is large enough
that hand-writing a client is real work. This one is nine endpoints; the client is about a
hundred lines of `fetch`.

Against that, fullstack costs: client and server must build together, nothing can be
tested with `curl`, and the frontend framework choice gets welded to the backend.

Plain REST gives testability, a second client someday for free (a script, a phone app),
and — the point that decides it — **the frontend becomes replaceable without touching the
backend.** If Dioxus proves irritating, that is a rewrite of one artifact, not two.

### 3.3 Frontend: Dioxus

Rust with `serde` DTOs **shared between server and client**, so the wire format is defined
once and cannot drift. The alternative — hand-maintained TypeScript interfaces, or a
codegen step — is most of the integration risk in a project like this.

Also: no Node toolchain to install, patch or fight on Windows.

The usual objection to Dioxus, a thin component ecosystem, does not bite here. A folder
tree, a virtualised table and a reading pane are components you would write by hand in
React too.

The real cost is **version churn** — Dioxus is young and its APIs have moved between
releases. Pin the version; treat an upgrade as scheduled work rather than something done
casually. See W6.

Built to WASM and **served as static assets by the same binary**, so there is one process,
one port and one thing to deploy.

### 3.4 Endpoints

All under `/api`. All except `login` require a session and are scoped to its user (§4.4).

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/login` | `{login, password}` → sets the session cookie |
| `POST` | `/api/logout` | Clears it |
| `GET` | `/api/session` | Who am I — lets the app boot without a login round trip |
| `GET` | `/api/folders` | Tree with unread counts |
| `GET` | `/api/folders/{id}/messages` | List page; keyset cursor `?before=<date>,<uid>&limit=` |
| `GET` | `/api/messages/{blake3}` | Headers, text part, sanitised HTML, part list |
| `GET` | `/api/messages/{blake3}/parts/{n}` | Attachment download (§7) |
| `PATCH` | `/api/placements/{folder_id}/{uid}` | `{seen: bool}` — the only write in the API |
| `GET` | `/api/search` | `?q=&folder=` (§8) |

Everything else is `GET` of static assets.

---

## 4. Authentication

### 4.1 Credentials

Reuse `users.password_hash` and `db::authenticate` unchanged. One password per person,
working for both Thunderbird and the web client. A second credential store would mean two
things to rotate and two ways to be locked out.

### 4.2 Sessions

A signed cookie carrying `user_id` and an expiry, HMAC'd with a key from `config.toml`
(generated like `encryption_key`). `HttpOnly` and `SameSite=Lax` always.

**`Secure` is set only when serving TLS.** This is not a preference — a `Secure` cookie is
never sent over plaintext HTTP, so hardcoding it would make local development silently
fail to stay logged in, with no error to explain why. The flag follows the listener's
mode (§9.1), which means the production cookie always carries it and the loopback one
never needs to.

Stateless deliberately: with four users, a `sessions` table buys revocation we have no
mechanism to trigger, and adds a write path to a schema whose defining property is that
almost nothing writes to it. If "log out everywhere" is ever wanted, rotating the signing
key does it for everyone at once — the honest amount of machinery at this scale.

Expiry: 30 days, sliding. These are archives, not bank accounts, and a login prompt every
session is what pushes people toward a weak password.

### 4.3 Brute force

Argon2id already makes guessing expensive, and the failure path deliberately cannot
distinguish an unknown login from a wrong password (existing `db::authenticate`
behaviour). Add an in-memory per-IP and per-login counter — ten failures in five minutes,
then a fixed delay. In-memory is enough: a restart clearing the counters is not a
meaningful bypass when every attempt still costs an Argon2 hash.

### 4.4 The isolation rule

**Every query is scoped by the authenticated `user_id`, in one place.** A `UserScope(i64)`
newtype, constructible only by the session middleware and required by every scoped call,
so an endpoint that forgets it does not compile. Not a convention — a type.

This matters more here than in IMAP. IMAP has one connection per authenticated user; HTTP
is stateless and every request re-establishes who is asking. Per-user buckets with scoped
S3 credentials are the backstop, but they only catch a mistake at the storage layer — a
wrong `WHERE` clause leaks metadata without ever touching S3.

Note `/api/messages/{blake3}`: the hash is user-supplied, so it must be resolved through
`messages.user_id`, never fetched from the bucket on the client's say-so.

---

## 5. The three panes

### 5.1 Folders

`folders` joined to `accounts` for the user, presented as the namespaced tree Thunderbird
sees (`work/INBOX`), split on the account's `hierarchy_delimiter`. Unread counts from
`placements` where `seen = false`.

The archive-root INBOX is empty by construction and should be hidden here rather than
shown as a permanently empty folder — it exists only because IMAP requires it.

### 5.2 Message list

Already indexed for this. `messages` carries `subject`, `from_addr` and `internaldate` as
denormalised columns, so **the list view needs no new schema** — it joins `placements`
(PK `(folder_id, uid)`) to `messages` by primary key and reads three columns plus `seen`.

Sort by `internaldate` descending. **Keyset pagination, not `OFFSET`**: the main INBOX has
~53,000 messages and `OFFSET 50000` re-walks every skipped row. Page on
`(internaldate, uid) < (last_date, last_uid)`.

There is no index on `(folder_id, internaldate)`; one would require denormalising
`internaldate` into `placements`, since an index cannot span a join. The plan was to
measure before adding it, and **the measurement says do not bother.**

Measured against the live archive from a development machine, on the real 53,573-message
INBOX:

| | Wall clock | Minus baseline |
|---|---|---|
| Baseline round trip (`SELECT 1`) | 128 ms | — |
| Message page (51 rows) | ~180 ms | **~50 ms** |
| Same query at page 40 (~2,000 deep) | ~180 ms | ~50 ms |
| Folder list, 46 folders with counts | 467 ms | ~340 ms |

Two things worth reading off that. **Page depth costs nothing** — page 40 is
indistinguishable from page 1, which is the keyset scheme doing its job; with `OFFSET`
this row would climb steadily. And most of the wall clock is WAN latency to OVH, not
query time: from the instance, in the same datacentre, the baseline is a fraction of a
millisecond, so these become roughly their right-hand column.

**The folder list is the slow query, not the message list** — the opposite of what was
anticipated. It counts every placement across every folder. At once per app load that is
tolerable, and it is recorded here rather than optimised because the fix (cached counts,
or dropping `total` and keeping only `unread`) should be chosen against a real complaint.

### 5.3 Reading pane

Body from S3 by `blake3` via `store::get_message`, which re-hashes on read and rejects a
mismatch. Parsed with `mail-parser`, already a dependency.

`messages.headers` (migration 0003) caches the header block, so the list and the header
display never touch S3. Only the body does.

**Both body representations are returned together**, and the pane offers a "view plain
text" toggle whenever a `text/plain` part exists. Most HTML mail is
`multipart/alternative` and ships one — it is the sender's own fallback, written by them,
and it is a far better answer to a badly-rendered message than anything we could
reconstruct from mangled HTML.

It costs essentially nothing: `messages.bodystructure` already records whether the part
exists, and the raw message is fetched once regardless, so both parts come out of the same
parse. The payoff is that "this message looks wrong" has an answer the user can act on
immediately, instead of becoming a bug report that waits on a code change.

Where a message has *only* an HTML part, the toggle is absent rather than showing an empty
pane.

Read state: `PATCH` sets `placements.seen` — the only write the web client can perform,
enforced by the schema rather than by our carefulness, since `seen` is the only mutable
column and is a boolean rather than a flag array for exactly this reason.

---

## 6. Rendering hostile HTML — the largest risk in this design

Every message is untrusted input that arrived from the internet, rendered inside an
authenticated session. This is the part most likely to go wrong.

**The allowlist is deliberately brutal: structural HTML only, and no styling at all.**
Some mail will render badly. That is an accepted, stated cost — a newsletter that looks
wrong is a complaint; a message that runs code is an incident. Loosening this later with
evidence about which specific messages broke is easy; tightening it after a leak is not.

### 6.1 Elements allowed

Nothing outside this list survives.

| Group | Elements |
|---|---|
| Block | `p` `div` `br` `hr` `blockquote` `pre` |
| Heading | `h1`–`h6` |
| Inline | `span` `strong` `b` `em` `i` `u` `s` `sub` `sup` `code` `small` |
| List | `ul` `ol` `li` `dl` `dt` `dd` |
| Table | `table` `thead` `tbody` `tfoot` `tr` `td` `th` `caption` |
| Link | `a` |
| Image | `img` (§6.3) |

Tables stay despite being a layout hack in mail, because they are structurally inert and
dropping them turns most newsletters into an unreadable run of concatenated text rather
than something merely ugly.

### 6.2 Attributes allowed

This is where strictness actually bites, and the list is nearly empty:

| Element | Attributes |
|---|---|
| `a` | `href` |
| `img` | `src` `alt` |
| `td` `th` | `colspan` `rowspan` |
| everything else | **none** |

**`style` is dropped everywhere.** No exceptions. Inline CSS is the main vector for
overlay and click-jacking tricks, for CSS-based exfiltration through attribute selectors
and background URLs, and for hiding text from the reader that is visible to a parser.
Dropping it also removes the need for `style-src 'unsafe-inline'` in the CSP below —
which is the single biggest strength gain available here, and the reason to accept the
rendering loss.

Also dropped: `class`, `id`, `width`, `height`, `align`, `valign`, `bgcolor`, `border`,
`target`, `srcset`, `background`, every `data-*`, and every `on*` handler.

`a` is rewritten server-side to carry `rel="noopener noreferrer nofollow"` and
`target="_blank"` — added by us, never taken from the message.

### 6.3 URL schemes

`href` accepts **`http`, `https`, `mailto` only**. `img src` accepts **`cid:` only**
(§6.4). Everything else — `javascript:`, `data:`, `vbscript:`, `file:`, protocol-relative
`//host`, and anything unrecognised — causes the attribute to be dropped and the element
kept as inert text.

URLs are re-parsed **after** sanitisation and re-validated before serialising. Parsing
once and trusting the result is how parser-differential bugs get through: the sanitiser's
view of a mangled URL and the browser's need not agree.

### 6.4 Images

Two separate concerns, often conflated:

**Remote images are blocked by default.** They are tracking pixels — they tell a sender
the message was read, when, and from which IP. `src` is rewritten to a placeholder, with
the "load remote images" button every mail client has. For old archived mail the default
should be off, and this is a privacy control as much as a security one.

**Inline (`cid:`) images are rewritten to a same-origin URL** pointing at a dedicated
endpoint, distinct from the attachment download in §7. That endpoint:

- serves **only** parts whose **magic bytes** match PNG, JPEG, GIF or WebP
- sets `Content-Type` from that sniff — never from the message's declared MIME type
- sets `X-Content-Type-Options: nosniff` and `Content-Disposition: inline`
- serves under `Content-Security-Policy: default-src 'none'`

Rewriting to a same-origin URL rather than inlining `data:` URIs keeps the payload small,
lets the browser cache, and — more importantly — means the CSP below can be `img-src
'self'` with no `data:` allowance at all.

### 6.5 Sandbox

The sanitised body renders in `<iframe sandbox srcdoc="...">` with **`allow-scripts`
absent**, and without `allow-same-origin` (so the frame gets an opaque origin and cannot
reach the session cookie or the DOM around it).

Not redundant with sanitising. The sanitiser is our code and may have a gap; the sandbox
is the browser's and holds when it does. Neither is trusted alone.

### 6.6 CSP

Because `style` is gone and images are same-origin, the frame policy can be genuinely
restrictive:

```
default-src 'none'; img-src 'self'; style-src 'sha256-<our block>'; form-action 'none'; base-uri 'none'
```

The `sha256` hash covers **one** stylesheet — ours, injected into the frame for basic
legibility (`img { max-width: 100% }`, word wrapping, a readable font). Any other style
block, including one that somehow survived sanitisation, fails the hash and is refused.
That is strictly stronger than `'unsafe-inline'`, and it is only reachable because we
dropped author styles entirely.

The app itself, outside the frame: `default-src 'self'; frame-src 'self'; object-src
'none'; base-uri 'none'; form-action 'self'`.

### 6.7 Also stripped

`<script>`, `<style>`, `<link>`, `<meta>`, `<base>`, `<iframe>`, `<object>`, `<embed>`,
`<applet>`, `<form>` and all form controls, `<svg>`, `<math>`, `<canvas>`, `<audio>`,
`<video>`, `<frame>`, `<frameset>`, `<marquee>` — **element and contents both**, not
unwrapped. A `<style>` block whose tags are removed but whose text is kept would render
the CSS as visible garbage.

HTML comments are stripped: Outlook conditional comments can hide markup from a naive
parser while remaining live to a browser.

Sanitisation is parse-then-reserialise (`ammonia`, on `html5ever`), never regex. Regex
over HTML loses to nesting and encoding tricks every time.

### 6.8 Plain text

Text parts get `white-space: pre-wrap`, HTML-escaped, with no HTML interpretation at all.
This is the Phase 4a path and carries none of the above risk — which is why it ships
first.

### 6.9 Accepted breakage

Stated plainly so it is not reported later as a defect: messages relying on inline CSS for
layout, colour or spacing will render as unstyled structural HTML. Background images
vanish. Multi-column newsletter layouts collapse to a single column. Web fonts do not
load.

The content is fully readable and no information is lost — only presentation. Revisit
with a list of specific messages that broke, if it ever actually matters.

**The escape hatch is the plain-text alternative** (§5.3), not a looser allowlist. When a
message renders badly, the sender's own `text/plain` part is one click away, and it is
usually the better artifact anyway. That makes bad rendering a mild annoyance rather than
a reason to weaken §6 — which is the point of having it.

Worth being precise about *why* this degrades gracefully, because it is not "only spam
uses styling". The heaviest HTML in a real mailbox is **transactional** — receipts,
invoices, itineraries, booking confirmations — and in an archive those are among the most
valuable things stored. They survive because **tables are kept** (§6.1): structure and
data remain, only colour, logos and spacing are lost. Spam degrades worst simply because
it is the most presentation-dependent content there is.

If anyone later proposes dropping tables as well, that is the change that would actually
destroy something worth keeping.

---

## 7. Attachments

**Two endpoints, deliberately separate**, because they have opposite jobs. The inline
image endpoint (§6.4) serves a narrow set of sniffed image types for display inside the
sandboxed frame. This one serves *anything* the message contains, and must therefore never
be displayed — only saved. Merging them would mean one handler whose safety depends on a
flag, which is the shape of bug that gets introduced during a later refactor.

Downloads: streamed from S3, never inline:

- `Content-Disposition: attachment` always, including images and PDFs
- `X-Content-Type-Options: nosniff`
- `Content-Type` from **our own** extension mapping, not the message's declared MIME type —
  an attacker controls that field, and a wrong `text/html` served same-origin is XSS with
  the sandbox bypassed
- Filenames sanitised: path separators and control characters stripped

Addressed by `(blake3, part_index)`. `messages.bodystructure` already holds the part tree,
so listing parts needs no re-parse of the body.

---

## 8. Search — this reverses an earlier decision

Search was previously judged unnecessary: *"my email clients end up downloading
everything, so a search index could easily be overkill."* Sound reasoning that does not
survive this change — it rested on the client holding a local copy. **Webmail users have
no local copy, so server-side search is the only search they get.** An archive of 53,000
messages you can only page through is close to useless.

1. **Substring on `subject` and `from_addr` — shipped, but WITHOUT the index.**

   The plan called for a `pg_trgm` GIN index. **That extension cannot be created on this
   database:** the `gern` role is not a superuser and `CREATE EXTENSION` is refused, even
   though pg_trgm 1.6 is available on the server. Worth knowing what that would have cost
   if it had gone in as a migration: migrations run inside `connect_db` on *every*
   command, so one that fails does not degrade search — it stops the archiver starting at
   all, including the IMAP server.

   Measured unindexed instead, against 151,518 real messages: a page of results costs
   about **570 ms** of query time (698 ms wall, less the 128 ms baseline round trip).
   Slower than an indexed search would be, and it degrades linearly with the corpus, but
   usable — and it needs no migration, which removes that failure mode entirely.

   **"Why not just index the table?"** Three answers, in the order they were tried:

   * A **btree** cannot serve `ILIKE '%term%'`. B-trees are ordered, so they help only
     when a prefix is known; a leading wildcard leaves nothing to seek on.
   * **Core full-text search** (`tsvector` + GIN) needs no extension and *does* index —
     but measured **slower**: 1.2 s against 610 ms. The index was never used. The query
     drove from `placements`, doing 152,741 primary-key lookups into `messages` and
     testing the predicate on each, so the only effect of the index was to add a
     `to_tsvector` computation per row.
   * **The query shape was the actual lever.** Filtering `messages` first — a
     `MATERIALIZED` CTE, so Postgres cannot inline it back into the join — scans 151,518
     rows once and hash-joins the ~3,000 survivors: **266 ms, down from 610 ms, with no
     index at all.** That is what shipped.

   The planner will not pick this on its own: it estimates `rows=19` for the join path
   against an actual 3,139, so it believes the nested loop is nearly free. Hence the shape
   is pinned in the SQL rather than left to the optimiser.

   pg_trgm would still help on top of this, by turning the remaining 191 ms sequential
   scan into an index lookup. If search ever becomes annoying, enabling the extension
   **on the database** (OVH's control panel is the place to look) is the fix, not a code
   change — the predicate is already written so an index would simply be used.

2. **Full text over bodies**, only if 1 proves insufficient. Needs body text extracted at
   ingest into a `tsvector`, a backfill across every archived message, and real storage.
   Not speculative work to do now.

Start at 1, ship, see whether 2 is ever asked for.

---

## 9. Running it

### 9.1 Local and production are the same binary, different listeners

Mirrors the rule `serve` already applies to IMAP, for the same reason — a plaintext
listener on a public address hands credentials to anyone on the path — so the two
subcommands behave consistently rather than each having their own logic to remember.

| | Local | Production |
|---|---|---|
| Command | `email-archiver serve-web` | `email-archiver serve-web 0.0.0.0:443` |
| Default bind | `127.0.0.1:8000` | — |
| TLS | None | `CertReloader`, the certbot cert |
| Cookie `Secure` | Off | On |
| CSP | No `upgrade-insecure-requests` | With it |

**The guard:** plaintext is permitted on loopback and refused anywhere else unless
`--allow-plaintext` is passed, which warns loudly. The rule is shared with `serve` —
`listen::resolve` — so it exists once rather than in two copies that can drift.

**Loopback is always plaintext for the web listener, whatever `tls.*` says.** This differs
from `serve`, for a reason specific to browsers: the certificate is issued for
`archive.thebackroom420.ca`, so a browser dialling `https://127.0.0.1:8000` rejects it on
hostname mismatch. TLS there cannot work, not merely "is unnecessary". The useful
consequence is that **the production `config.toml` works unchanged on a development
machine**, even though its `tls.cert_path` points at an `/etc/letsencrypt/...` path that
does not exist there.

`serve` keeps the opposite policy (`Loopback::MayUseTls`), because an IMAP test client can
be told to skip verification — that is a real workflow and removing it would be a
regression.

Serving HTTPS directly from the binary via `CertReloader` means no reverse proxy: same
certificate, same mtime-triggered reload, no Caddy or nginx as a third moving part on a
box currently running one process.

### 9.2 The frontend during development

`dx serve` runs its own dev server with hot reload on a different port from the API, which
means a cross-origin request in development and not in production.

Proxy `/api` from the Dioxus dev server to `127.0.0.1:8000` rather than enabling CORS.
Same-origin in both environments keeps `SameSite=Lax` honest and avoids a permissive CORS
policy existing in the codebase at all — the kind of setting that gets copied into
production later by someone in a hurry.

### 9.3 Port 80 must stay free — the failure that surfaces in 90 days

`certbot --standalone` binds port 80 to answer the HTTP-01 challenge **at every renewal**,
not just at issuance. Binding 80 for an HTTP→HTTPS redirect would work for about three
months and then silently fail renewal; the first symptom would be clients refusing an
expired certificate.

So **`serve-web` binds 443 only** in production. Users type `https://`. The
`tls-certificate` row in `status` is the safety net if this is ever gotten wrong.

If a redirect is wanted later, the fix is moving certbot to `--webroot` served by the app —
not stop/start hooks around renewal, which take the archive down unattended at an hour
nobody chose.

### 9.5 Frontend toolchain — two things that bite on Windows

**`dx` and `dioxus` must be the same version.** The CLI checks, and on a mismatch prints
`dx and dioxus versions are incompatible` — and then *carries on building*. An error that
does not stop the build is one you scroll past, so `web-ui/Cargo.toml` pins dioxus
exactly (`=0.7.3`) to the installed CLI rather than using a caret range that cargo can
drift. Bumping it means installing the matching CLI in the same commit:

```
cargo install dioxus-cli --version <same> --locked
```

**`wasm-opt` crashes on this machine** — `exit code: 0xc0000409`, a Windows stack-buffer
crash inside the copy bundled with `dx`. `dx` logs it as an ERROR, then ships the
*unoptimised* wasm and reports success, so the build is fine but ~2 MB instead of ~1.2 MB.
Setting `[web.wasm_opt] level = "0"` does not help; it still invokes the tool.

Rather than chase it, the server gzips: **2,040 KB → 520 KB on the wire**, which recovers
more than wasm-opt would have. The ERROR line remains and is expected. If it is ever worth
fixing, a newer `dx` (with its own newer wasm-opt) is the thing to try, and the dioxus pin
moves with it.

The build directory also accumulates old content-hashed files across builds, so a deploy
should bundle from a clean output directory rather than whatever has piled up.

### 9.4 Deployment

Its own systemd unit from the same binary, same `email-archiver` service user, so the
certbot renewal hook that opens the certificate to that group already covers it. Same
hardening, plus `443` needs `CAP_NET_BIND_SERVICE` — already granted.

443 is already open in UFW from the original cloud-init.

The existing `deploy email-archiver` grows a second unit rather than gaining a second
deployer: one binary, one upload, two units.

---

## 10. What this deliberately does not do

- **No sending.** The archive has no submission path and this does not add one.
- **No deleting, moving or flagging** beyond `\Seen`. Enforced by the schema.
- **No folder management.**
- **No multi-account merging.** Folders stay namespaced by account label, as in
  Thunderbird.
- **No mobile layout initially.** Three panes do not fold onto a phone, and doing it well
  is its own piece of work. Worth knowing before someone opens it on a phone and reports
  it broken.

---

## 11. Risks

| # | Risk | Handling |
|---|---|---|
| W1 | **HTML rendering** — XSS or tracking via a message body | §6: structural-only allowlist, **no `style` at all**, opaque-origin sandbox, hash-pinned CSP, remote images off. The one to get right. |
| W1b | **Strict sanitisation breaks legitimate mail** | Accepted and documented (§6.9). Loosening later with a list of real broken messages is easy; tightening after a leak is not. |
| W2 | **Cross-user leakage** from a query missing its `user_id` | `UserScope` newtype (§4.4); per-user buckets as backstop |
| W3 | **Attachment content-type confusion** | Our extension mapping, never the message's claim; always `attachment` (§7) |
| W4 | **Renewal breaks when something takes port 80** | §9.3; `status` shows days remaining |
| W5 | **Deep pagination slow at 53k messages** | Keyset pagination from the start; index only if measured (§5.2) |
| W6 | **Dioxus version churn** | Pin it; upgrades are scheduled work, not incidental |
| W7 | **A plaintext listener reaching production** | Loopback-only default, explicit `--allow-plaintext` with a warning, `Secure` cookie tied to TLS (§9.1) |
| W8 | **HTTP handlers destabilise the IMAP server** — same process, shared pool | Separate systemd units, so a web restart never interrupts Thunderbird; `database.max_connections` covers both |

---

## 12. Phasing

| Phase | Work | Gate |
|---|---|---|
| **1** | ✅ `serve-web`: axum, static assets, `127.0.0.1:8000` plaintext | Done — `/api/health` reports database reachability; unknown API routes 404 rather than returning index.html; non-loopback plaintext refused; IMAP unaffected (35 tests) |
| **2** | ✅ Login, session cookie, `UserScope`, login throttle | Done — `/api/session` 401s without a valid cookie; forged and expired cookies rejected; unknown-user and wrong-password responses byte-identical; throttle engages on the 10th failure |
| **3** | ✅ Folder pane + message list, keyset pagination | Done — Dioxus 0.7 app served by the binary; keyset paging flat with depth (§5.2), no index needed |
| **4a** | ✅ Reading pane, **plain text only** | Done — headers, `text/plain` body, part list. HTML-only messages say so rather than showing an empty pane |
| **4b** | ✅ Sanitised HTML (§6) | Done — structural allowlist, no author styles, opaque-origin sandbox, hash-pinned CSP, remote images blocked and counted, cid: images served same-origin by magic-byte sniff. 21 sanitiser tests |
| **5** | ✅ Attachments; `\Seen` on read | Done — always-attachment downloads with sanitised filenames, optimistic read state, paperclip column |
| **6** | ✅ Search over subject and sender | Done — **without** `pg_trgm`: the extension cannot be created on this database (§8). ~570 ms over 151,518 messages, measured |
| **7** | ✅ TLS on 443, systemd unit, `status` rows | Done — TLSv1.3 verified locally against a throwaway certificate; `Secure` cookie confirmed present under TLS and absent on loopback |

Phase 4 is split deliberately: plain text is genuinely useful on its own, so the HTML work
never blocks anyone's access to their mail.

Phases 1–6 are all doable against a local plaintext server, so TLS is needed only at the
end rather than being setup friction on day one.

---

## 13. Open questions

**Q1 — RESOLVED: Dioxus 0.7.** `dx 0.7.3` is already installed on the development
machine, and 0.8 is still an alpha. Pin to 0.7 and treat the 0.8 upgrade as scheduled
work (W6).

**Q2 — Hostname.** Reuse `archive.thebackroom420.ca` on 443 (IMAPS is on 993, so no
clash), or a separate `mail.thebackroom420.ca`? Reusing means one certificate and no DNS
change, and is the assumption above.

**Q3 — Folder presentation.** Thunderbird shows the raw namespaced path (`work/INBOX`).
The web client could group by account instead. Cosmetic, but decide before Phase 3.

**Q4 — RESOLVED: derived, not separate.** The plan leaned toward a second secret so that
invalidating sessions would not touch stored passwords. Deriving turns out to win on both
counts: `blake3::derive_key` with a versioned context string
(`session::KEY_CONTEXT`) gives independent session invalidation by bumping the version in
code, and a second secret would be another value to generate, deliver through Terraform,
and lose. There is no added exposure either — if `encryption_key` leaks, stored
source-mailbox passwords are readable, and forged session cookies are not the marginal
harm at that point.
