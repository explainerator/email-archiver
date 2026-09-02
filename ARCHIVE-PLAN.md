# email-archiver — Design Plan

**Status:** DRAFT. Supersedes an earlier Stalwart-based plan, which was abandoned and is not
in this repository's history (the repo was re-initialised).
**Repo:** `email-archiver` — Terraform and the Rust program both live here.
**Last revised:** 2026-09-01

> **Governing constraint: one part-time maintainer.** Every component justifies its
> ongoing attention cost, not just its capability. See §10 for the honest tally.

---

## 1. What this is

A **read-only email archive**: a small Rust program that ingests mail over IMAP from
existing accounts, stores message bodies in OVH S3, indexes them in OVH Managed Postgres,
and serves them back over **IMAPS to Thunderbird**. Nothing else.

Not a mail server. It does not send, receive, relay, or accept delivery.

### 1.1 Why not off-the-shelf — settled, do not re-litigate

| | S3 as primary store | IMAP to clients | Verdict |
|---|---|---|---|
| **Stalwart** | ✅ | ✅ | Tried it. A full MTA, spam filter, CalDAV/Sieve/JMAP server — we used ~15% and fought its config model. Settings live in a database behind a web UI with no config-as-code path. |
| Bichon | ❌ architecturally local | ❌ web UI only | Docs forbid network filesystems. Local-first by design. |
| MailPiler OSS | ❌ Enterprise-only | ❌ web UI only | Best archival features, wrong storage and access model. |
| Dovecot OSS | ❌ obox is commercial | ✅ | |
| Apache James | ✅ | ✅ | S3 needs the Distributed profile: Cassandra + OpenSearch + RabbitMQ. Won't fit 4 GB. |

The purpose-built archivers all chose *local disk + web UI*. Our two hard requirements —
S3 as primary store, IMAP to clients — are exactly what that category declined to build.

### 1.2 What we gain by building it

- **Read-only is structural, not configuration.** There is no `APPEND`, no `EXPUNGE`, no
  delete path — not disabled, *absent*. Under Stalwart this was an ACL you could
  misconfigure. Here a stray drag-and-drop cannot mutate history because the code does not
  exist. **The single exception is `\Seen` flags** (§3.2): read state is mutable, message
  content and placement are not. Nothing can add, alter or remove a message.
- **Rebuild-from-S3 is a designed property.** We control the object format, so the Postgres
  index is provably derivable from the bucket alone. This was the largest risk in the
  previous plan; it is now a design decision rather than a hope.

---

## 2. Architecture

```
   Thunderbird                        Existing mail accounts
        |                              (Gmail / IMAP hosts)
   IMAPS :993                                   |
        |                                  ingest (IMAP)
        v                                       v
   +--------------------------------------------------+
   |       email-archiver (single Rust binary)        |
   |  read-only IMAP server  |  ingest worker         |
   +------------+----------------------+--------------+
                |                      |
        index & metadata          message bodies
                v                      v
     OVH Managed Postgres          OVH S3 Standard — one bucket per user
     qw300972-001.ca.clouddb       ken-twoducks-ca
     .ovh.net:35628                art-jduck-ca   ...
     database: archive             (both in Canada, same region)
```

One binary, run directly under systemd on the instance. **No Docker** — there is nothing
to isolate and nothing to compose; a single static-ish binary with a config file is simpler
to operate and to reason about.

**No local state at all**, which is what keeps a future move into a pod cheap: configuration
arrives as a file, everything durable is in S3 or Postgres, and the process can be killed
and restarted anywhere that can reach both.

### 2.1 Storage split

| Data | Where | Why |
|---|---|---|
| Raw RFC 5322 messages | **S3**, content-addressed, **one bucket per user** | Immutable once written — exactly S3's model. Cheap, unbounded growth, and a hard privacy boundary (§2.2). |
| Envelope, headers, MIME structure, flags, folder tree, UIDs | **Postgres** | Hot path for every IMAP operation. |
| Full-text body index | **Not built** — see §3.2 | Thunderbird indexes locally once messages are downloaded. |

Both Postgres and S3 are OVH Canada, so metadata lookups stay in-region.

### 2.2 One bucket per user

**Each user gets their own bucket.** Not a shared bucket with per-user prefixes — a separate
bucket, with its own S3 credential scoped to it alone. 4–5 users, so the overhead is
negligible and the isolation is structural rather than enforced by application logic.

A **user** is a person. A user may have several **source accounts** (e.g. a work address and
a personal one) that all feed into their single bucket.

```
ken-twoducks-ca      <- user bucket
art-jduck-ca
...
```

**Naming.** S3 allows only lowercase alphanumerics, hyphens and dots (3–63 chars) — no `@`,
no underscores, no uppercase. The address is transformed deterministically:

```
ken@twoducks.ca   ->   ken-twoducks-ca
```

`@` and `.` both become `-`, and the result is lowercased. **No prefix** — every bucket in
this project belongs to the archive, so "archive-" would be noise.

**Why hyphens rather than dots.** `ken.twoducks.ca` would be a legal bucket name and closer to
the address, but dots break virtual-hosted-style HTTPS: the wildcard certificate
`*.s3.bhs.io.cloud.ovh.net` does not match a multi-level subdomain. That would force path-style
addressing on every tool touching these buckets, including ad-hoc `aws`/`rclone` debugging —
a confusing TLS failure exactly when you least want one. Hyphens avoid it entirely.

The transform lives in Terraform (`locals.user_buckets`), not in application code, so it
cannot drift.

*Caveat: bucket names are not secret. Encoding addresses in them reveals who the users are to
anyone who can list the project's buckets — which is the account owner, so the exposure is
acceptable.*

**Readable names are a deliberate choice for this deployment.** Knowing whose bucket is whose
at a glance is worth more right now than hiding the addresses from an operator who can already
read the mail. **If this is ever released or run for anyone else, switch to hashes of the
address** — that removes the identity leak from bucket listings.

Note the cost of changing later: S3 buckets cannot be renamed, so switching naming scheme
means new buckets and a copy. `prevent_destroy` will refuse the rename outright, which is the
correct failure — loud rather than silent. Not a reason to pre-emptively hash now; just the
price of the decision, recorded.

### 2.3 Object layout, within each user's bucket

```
messages/<blake3-of-raw-message>          the raw RFC 5322 bytes
manifest/<account>/<uuid>.json            sidecar: account, folder, internaldate, flags,
                                          source UID, assigned UID, message hash
```

**The manifest is what makes rebuild work.** Listing `manifest/` and reading each object
yields everything needed to reconstruct that user's rows in Postgres without parsing GB of
message bodies.

**Duplicates within a folder collapse into one message — decided, keep it.**
Two byte-identical messages in the same folder share a placement, so the folder
shows one copy where the source had two. Verified in practice: 18 such cases
across ~14,000 messages in three folders, all confirmed duplicates rather than
loss. Two identical messages are indistinguishable in a client, so the second is
noise. Use `diagnose` to tell duplicates from real loss; ingest's completeness
note points at it rather than claiming loss it cannot demonstrate.

**Deduplication is per-user, deliberately.** Content-addressing means a message that arrived
in two of *one user's* accounts is stored once. A message shared between *two different
users* is stored twice — once in each bucket. That is the correct trade: cross-user dedup
would mean one person's storage backing another person's mail, which is exactly the coupling
this separation exists to prevent. The extra copies are a rounding error at this scale.

*Rebuild is not a recovery script bolted on later — it is the only way the index is ever
populated. Ingest writes to S3; the indexer reads from S3. Same code path in normal
operation and in disaster recovery, so it cannot rot.*

### 2.4 Where isolation is, and isn't, enforced

Being precise about this, because per-user buckets can create false confidence:

| Layer | Isolation | Strength |
|---|---|---|
| S3 | Separate bucket + separate credential per user | **Structural.** A leaked credential exposes one user. |
| Postgres | Shared schema, separated by `WHERE user_id = ?` | **Logical.** A query bug crosses users. |
| IMAP server | Authenticated user selects both the bucket credential and the SQL filter | **Logical.** |

The bucket split is real defence in depth, but **Postgres remains a shared boundary enforced
by correct queries**. Mitigations: every query path goes through a single accessor that takes
the authenticated `user_id`, never raw table access from handlers; and the server holds each
user's S3 credential separately rather than one credential that can read everything.

That last point matters — if the server used one credential with access to all buckets, the
bucket separation would only protect against credential leak, not against a bug in our code.

---

## 3. Implementation

Rust. Crate choices, with what each removes from scope:

| Concern | Crate | Notes |
|---|---|---|
| IMAP wire protocol | **`imap-codec`** | Complete IMAP4rev1 parse/serialize. Removes the highest-bug-density work: literals, quoting, grammar. |
| Protocol flow | **`imap-next`** | Sans-I/O flow layer. **Caveat: ecosystem is client-oriented** — no server type, no examples, no known server users. See R1. |
| MIME parsing | **`mail-parser`** | Produces the tree needed for `BODYSTRUCTURE` / `ENVELOPE`. |
| IMAP client (ingest) | **`async-imap`** | Includes OAuth2/XOAUTH2 for Gmail. |
| TLS | `tokio-rustls` | |
| ACME | `rustls-acme` | TLS-ALPN-01 on :443. |
| S3 | `aws-sdk-s3` | Custom endpoint + region for OVH. **Never FUSE.** |
| Postgres | `sqlx` | Compile-time checked queries; migrations in-tree. Matches `database-service`, which already uses sqlx 0.8. |

### 3.1 Postgres

Shared OVH Managed Postgres cluster, **isolated in its own database**:

```
host      qw300972-001.ca.clouddb.ovh.net
port      35628
database  archive          <- ours; siblings are defaultdb (game services) and backroom
user      gern
sslmode   require
```

This is stronger than the schema-in-`defaultdb` arrangement originally planned: the game
services' tables are not in an adjacent namespace, they are in a different database.

`gern` cannot `CREATE` on the database itself, but **can create tables in `public` within
`archive`** — verified against the live cluster — so there is **no privileged bootstrap
step**. Applying the schema is:

```
DATABASE_URL="postgres://gern:PASSWORD@qw300972-001.ca.clouddb.ovh.net:35628/archive?sslmode=require"   cargo run
```

The binary asserts `current_database() = 'archive'` before migrating, so a misconfigured
URL cannot write into `defaultdb`.

```sql
users      (id, login, bucket, display_name)      -- one person; owns one S3 bucket
accounts   (id, user_id, address, provider)       -- a user may have several source accounts
folders    (id, account_id, name, uidvalidity, uidnext)
messages   (id, user_id, blake3, size, internaldate, envelope JSONB, bodystructure JSONB,
            UNIQUE (user_id, blake3))             -- dedup is scoped per user, per section 2.2
placements (folder_id, uid, message_id, flags)    -- a message may sit in several folders
```

`placements` separates *the message* from *where it appears*, which is what makes
deduplication work without breaking per-folder UIDs.

**`users` sits above `accounts`.** IMAP login authenticates a *user*, and that user sees the
folder trees of all their accounts under one connection — typically namespaced as
`work/INBOX`, `personal/INBOX`. The `UNIQUE (user_id, blake3)` constraint is what enforces
per-user dedup at the database level, matching the bucket boundary.

**UIDs are assigned at index time and never change.** They are written to the manifest, not
regenerated, so a rebuild reproduces them exactly. If UIDs shifted, every client would
silently re-download everything.

**Role: reuse `gern`** rather than creating a dedicated one — deliberate, to avoid managing
another credential. Because the archive now has its own *database*, this costs less isolation
than it would have under the shared-schema plan: `gern` can still reach `defaultdb`, but
nothing here writes there, and the `current_database()` assertion enforces that.

The connection pool is capped at 3 so a bulk ingest cannot starve the game services of
connections on the shared cluster (R4).

### 3.2 IMAP subset — target Thunderbird

Implement only what Thunderbird needs:

`CAPABILITY` · `AUTHENTICATE PLAIN` / `LOGIN` · `NAMESPACE` · `LIST` · `LSUB` ·
`SELECT` / `EXAMINE` · `STATUS` · `FETCH` / `UID FETCH` · `SEARCH` / `UID SEARCH` ·
`IDLE` · `NOOP` · `LOGOUT` · `CLOSE`

Advertise `IMAP4rev1`, `SPECIAL-USE`, `LITERAL+`, `IDLE`.

**Not implemented, by design:** `APPEND`, `COPY`, `MOVE`, `EXPUNGE`, `CREATE`, `DELETE`,
`RENAME`, `SETACL`. Return `NO [CANNOT]` rather than dropping the connection — Thunderbird
handles a clean refusal far better than a closed socket.

**`FETCH` is the bulk of the work**: `BODY[]`, `BODY[HEADER]`, `BODY[HEADER.FIELDS (...)]`,
`BODY[n.m]`, partial `<offset.length>`, `BODYSTRUCTURE`, `ENVELOPE`, `RFC822.SIZE`,
`INTERNALDATE`, `FLAGS`, `UID`.

#### Search: metadata yes, full-text no

Two things get conflated under "search", and only one is expensive:

| | Cost | Decision |
|---|---|---|
| `SEARCH` on headers/metadata — `FROM`, `TO`, `SUBJECT`, `SINCE`, `BEFORE`, `LARGER`, `HEADER` | Plain SQL on indexed columns | **Implement.** Nearly free. |
| `SEARCH TEXT` / `SEARCH BODY` — full-text over message bodies | `tsvector` + GIN, plus extracting text from every MIME part at ingest | **Do not implement.** |

Thunderbird downloads messages for offline use and indexes them locally (Gloda), so
body search is already served client-side. Dropping the full-text index removes a table,
a GIN index, and body-text extraction from the ingest path.

**Unsupported criteria must return `NO [CANNOT]`, never an empty result set.** An empty
result reads as "no such mail" — a silently wrong answer about your own history is worse
than an error. Full-text remains backfillable from S3 later if it turns out to be wanted.

**`\Seen` flags are permitted.** Thunderbird will try to set them, and persisting them in
`placements.flags` makes the archive pleasant to use. This is the *only* mutable state in the
system, and it is scoped to that one column — no other `STORE` operation is accepted, and
flags never touch S3, so the stored mail itself stays untouched. A wrongly-flagged message is
a cosmetic annoyance; a lost one is not.

### 3.3 Ingestion

Built into the binary (`email-archiver ingest --account …`), not a separate tool.

- IMAP with OAuth2/XOAUTH2 for Gmail, app passwords elsewhere
- **Resumable**: records the highest source UID seen per folder; a re-run continues rather
  than restarting. Non-negotiable — ~15 GB over a throttled Gmail connection is a multi-day
  pull that *will* be interrupted.
- Writes to S3 first, then indexes. A crash between the two leaves an orphan blob, which is
  harmless and reclaimed by the next index pass.

#### Source account inventory

Carried over from a superseded migration document that is no longer in this repository. This
is the part that drives ingest, because the provider determines the authentication method —
**it is now recorded only here, so do not delete it lightly:**

| Domain(s) | Provider | Auth for ingest | Real mailboxes |
|---|---|---|---|
| `thequeensflorist.ca`, `thebackroom420.ca`, `theatlanticgrowshop.ca`, `backroom420.ca` + minor alias domains | **Google Workspace** | XOAUTH2, or app password | **3**, all on `thebackroom420.ca` |
| `twoducks.ca` | Generic IMAP | App password | A few |
| `kenduck.ca` | Generic IMAP | App password | A couple |
| `jduck.ca` | Generic IMAP | App password | A couple |

**Aliases produce no ingest work.** `twoducks.ca` has *hundreds* of aliases and the
`thebackroom420.ca` group has group-addresses and former-employee redirects — but none of
them hold mail. Redirected mail already landed in a real mailbox and gets archived there.
**Ingest targets real mailboxes only**, which is roughly 8–10 accounts, not hundreds of
addresses.

**Size and deadline pressure sit on different accounts** — this drives the ordering:

- **The ~15 GB mailbox is on self-hosted generic IMAP**, not Google. No third-party
  throttling, and the source server is under our control, so it can be ingested at whatever
  rate the box tolerates. It is also **not** subject to the migration deadline, because
  nobody else decides when it shuts down.
- **The Google Workspace mailboxes are small** (a fraction of 15 GB) but carry the OAuth2
  complexity *and* the hard deadline — they disappear when that migration completes.

Consequences:

- Within the Google Workspace group, the mirror domains (`thequeensflorist.ca`,
  `theatlanticgrowshop.ca`, `backroom420.ca`) map usernames 1:1 onto `thebackroom420.ca`.
  Archive the `thebackroom420.ca` mailbox once; do not treat each mirror as a separate
  account or the same mail is ingested repeatedly. Content-addressing (§2.2) would
  deduplicate the blobs, but the `placements` rows would still be duplicated.
- **Possible shortcut for the 15 GB mailbox:** since it is self-hosted, the mail store is
  reachable directly on disk (Maildir/mbox) rather than only over IMAP. A direct reader
  would be far faster than IMAP for 15 GB. **Not planned** — it is a second ingest path to
  write and maintain, which conflicts with §10. Hold it in reserve for if IMAP ingest proves
  too slow, rather than building it speculatively.

---

## 4. Infrastructure

Built and verified in the earlier phases; unchanged by this pivot except the volume.

| Resource | State |
|---|---|
| `d2-4` instance, BHS5, Ubuntu 24.04, monthly | Running, `51.79.93.209` |
| `unattended-upgrades`, UFW 22/80/443/993 | Configured via cloud-init |
| Docker | **Installed by cloud-init but not used** — the binary runs directly under systemd. Harmless; removing it means editing `user_data`, which may force instance replacement and change the IP. Clear it out on the next legitimate rebuild. |
| Bucket `backroom-mail-archive`, versioning + AES256 | **To be replaced** by per-user buckets (§2.2). Empty, so no migration cost. |
| S3 user + credential, scoped to that bucket | Likewise — becomes one user/credential per bucket |
| `archive.thebackroom420.ca` → instance | Resolving |
| **50 GB Block Storage volume** | **Being removed — §4.1** |

### 4.1 The volume is no longer needed

With Postgres remote and bodies in S3, nothing local has to survive a rebuild. The only
persistent local state is the ACME certificate cache — a few KB, and cheap to re-issue.
The `d2-4`'s own 50 GB disk covers the OS, the binary, and ingest staging.

Removing it deletes the volume resource, the attachment, the mount, the format guard in
cloud-init, and the mountpoint check in the deploy step. Saves ~$2/month and a meaningful
amount of moving parts.

**Two-step removal**, because the resource carries `prevent_destroy`: first apply removes
the lifecycle block, second removes the resource.

Terraform files retained: `instance.tf`, `storage.tf`, `providers.tf`, `variables.tf`,
`outputs.tf`, `files/cloud-init.yaml`. A `deploy.tf` will be needed again for the new
program. The abandoned Stalwart deployment used an SSH provisioner chain
(`firewall -> docker_install -> transfer_files -> start`); that shape is still the model to
follow, but it is not recoverable from this repository.

---

## 5. TLS

`rustls-acme`, TLS-ALPN-01 on :443, for `archive.thebackroom420.ca`. Certificate cached on
the boot disk; on rebuild it simply re-issues.

Ports: **993** IMAPS (public), **443** ACME only, **80** unused, **25/587** closed.

---

## 6. Verification

**6.1 Blob durability — dropped.** A 200-object probe was written and read back
byte-identical, which confirmed the write path; the wider ack-before-durable test was
judged not worth the ceremony for this deployment. Two things make it less necessary than
it first appeared: `get_message` re-hashes every object on read and rejects a mismatch, so
corruption surfaces on use rather than silently; and mail is a second copy here, not the
only one. Probe objects were purged (version-aware — see below).

**Note for anything that ever deletes from these buckets:** versioning is enabled, so a
plain `DELETE` only writes a delete marker and the bytes remain as a billed noncurrent
version. `Store::list_versions` / `delete_version` exist for genuine removal.

**6.2 Rebuild-from-S3.** Truncate every table in the `archive` database, re-run the indexer
against the buckets, confirm identical message count, identical UIDs, identical folder
structure.
**This is the headline test.** Because rebuild is the normal indexing path (§2.2) it should
pass trivially — and if it doesn't, that is a design defect, not a recovery inconvenience.

**6.3 Thunderbird.** Connect, browse, search by sender and subject, open messages with
attachments. Confirm write operations are refused cleanly and the client stays usable.

---

## 7. Risks

**R1 — No prior art for `imap-next` as a server.** The crate advertises server support but
ships no server type, examples, or known users. Mitigation: prototype `SELECT` + `FETCH`
against Thunderbird **first**. If the abstraction fights us, better to know in week one —
`imap-codec` alone is still usable at a lower level.

**R2 — `FETCH` correctness against real-world MIME.** Twenty years of mail includes
malformed messages. Mitigation: `mail-parser` is battle-tested; never re-encode — always
serve the original bytes from S3 for `BODY[]`.

**R3 — Deadline on the Google Workspace accounts.** Those mailboxes vanish when the mail
migration completes, and unlike the self-hosted ones we do not control that date. They are
small, so the exposure is the *OAuth2 setup*, not transfer time. **Get Google ingest working
early**, even though the bulk of the data is elsewhere. The 15 GB self-hosted mailbox is
under our own control and carries no deadline. Resumability (§3.3) mitigates both.

**R4 — Shared database *and* shared role with production services.** The `archive` schema
lives in the same Postgres instance the game services depend on, and connects as the same
`gern` role (§3.1), so there is no permission boundary between them — only convention. A bulk
ingest could also contend for connections. Mitigations: cap the ingest pool at 2–3
connections, qualify every statement with the `archive.` schema, and watch the first large
ingest. Revisit the dedicated-role decision if the archiver ever runs untrusted input.

**R5 — Postgres is the weaker isolation boundary.** Per-user buckets are structural, but the
database separates users by query predicate (§2.4). A missing `WHERE user_id = ?` would serve
one person's mail to another. Mitigation: a single accessor layer that takes the authenticated
user id; no raw table access from request handlers; a test that asserts cross-user reads
return nothing.

**R6 — Scope creep into a mail server.** The temptation to add SMTP, or writes, or a web UI.
Refuse. The value here is that it does one thing.

---

## 8. Phasing

| Phase | Work | Gate |
|---|---|---|
| **0** | ✅ Volume removed. Per-user buckets created. DB allowlist updated. Q1–Q3 settled. | Done |
| **1** | ✅ `archive` database + sqlx migrations | Done — 5 tables, all FKs RESTRICT, constraints verified against the live database |
| **2** | ✅ S3 write/read + manifest format | Done — content-addressed put/get with read-time hash verification, paginated listing, version-aware delete |
| **3** | Ingest one *small generic-IMAP* account end to end (`kenduck.ca` / `jduck.ca`) | Objects in S3, rows in Postgres |
| **4** | ✅ IMAP spike against Thunderbird | Done — `imap-next` has a real `Server` type; R1 resolved, no `imap-codec` fallback needed |
| **5** | ✅ LIST, LSUB, SELECT, FETCH · ⬜ STATUS, SEARCH | Thunderbird logs in, browses 47 folders and displays bodies. **ENVELOPE and BODYSTRUCTURE turn out not to be needed** — Thunderbird builds its list from `BODY.PEEK[HEADER.FIELDS (...)]` |
| **6** | §6.2 rebuild test — drop the schema, reindex from S3 | Identical count, UIDs, structure |
| **7** | ACME, `deploy.tf`, **systemd unit running the binary directly** (no Docker) | Running under TLS on the instance |
| **8a** | **Google Workspace accounts** — OAuth2 path, small data, *hard deadline* (R3) | Business mail archived before those accounts close |
| **8b** | Self-hosted `twoducks.ca` ~15 GB, then the remainder | Complete archive |

Phase 4 is deliberately early: it is the riskiest unknown, and everything after it is
conventional work.

---

## 9. Open questions

**Q1 — RESOLVED.** Reuse the `gern` role; see §3.1 for the discipline that compensates.
Remaining action: **add `51.79.93.209` to the database IP allowlist.** *Blocks Phase 1.*

**Q2 — RESOLVED.** `\Seen` flags are allowed; see §3.2.

**Q3 — The user list.** Needed to create the buckets: for each of the 4–5 people, their
IMAP login, the bucket-name slug, and which source accounts feed into it. This is the only
input Terraform needs for §2.2. *Blocks Phase 0.*

---

## 10. Maintenance burden

| Task | Frequency | Effort |
|---|---|---|
| Certificate renewal | Automatic | None |
| OS patching | Automatic (`unattended-upgrades`) | Occasional reboot |
| Postgres backup | Managed by OVH; also rebuildable from S3 (§6.2) | None |
| Dependency updates | Quarterly | `cargo update`, run tests |
| Our own bugs | **This is the new cost** | The honest trade for owning the code |

Owning the program removes a vendor's config model from the maintenance surface and adds
our own defects to it. That trade is only sound because the scope is small and fixed —
which is why R5 matters more than it looks.

---

## 11. Cost

| Item | Cost |
|---|---|
| `d2-4` instance, monthly-billed | ~$12 |
| S3 Standard, ~30 GB | ~$0.30 |
| Requests, egress | $0 — OVH bills neither |
| Postgres | $0 marginal — existing instance |
| **Total** | **~$12/month** |

Down from ~$15 with the volume removed.
