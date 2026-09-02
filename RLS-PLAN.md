# Row-Level Security for the archive

Status: **design, not started.** Companion to `ARCHIVE-PLAN.md` and `WEBAPP-PLAN.md`.

---

## 1. The question this answers

> Can I *guarantee* that mail owned by one user cannot accidentally be retrieved by
> another?

Today the guarantee is: every query binds `user_id`, and every handler that reads mail
takes a `UserScope` that cannot be constructed without a valid session. That is good, and
it is one layer. `UserScope` proves an id was **passed**; it cannot prove the query
**used it correctly**.

That distinction is not theoretical. A search query shipped in this repository selecting
`f.id` into a field named `folder_id` — wrong at runtime, clean at compile time. A `WHERE`
clause dropped from a `JOIN` would fail the same way and be far quieter: it would return
*more* rows, not none, and nothing would complain.

**Row-Level Security is the layer that catches that.** With a policy in place, a query
that forgets its scope returns nothing rather than everything.

### 1.1 What it does and does not guarantee

| | |
|---|---|
| ✅ A missing or wrong `WHERE` clause | Returns zero rows instead of another user's mail |
| ✅ A new endpoint that forgets to scope | Same |
| ✅ A `JOIN` that accidentally widens the result | Same |
| ❌ The application setting the **wrong** id | Still leaks — the app declares who it is |

RLS does not remove trust from the application; it shrinks the trusted surface from *every
query in the codebase* to *one line that sets the session variable*. That is a large
reduction and an honest one, and it should be stated plainly rather than sold as absolute.

**Message bodies are already better protected than RLS could manage** — per-user S3
buckets with credentials scoped to one bucket each (`ARCHIVE-PLAN.md` §2.2). The gap RLS
fills is Postgres: subjects, senders, folder names, read state, sizes. Metadata, but
metadata that is plenty revealing.

---

## 2. Feasibility — checked, not assumed

Measured against the live database before writing any of this, because the last plan that
assumed a Postgres capability was wrong about it (`pg_trgm`, `WEBAPP-PLAN.md` §8):

| Check | Result |
|---|---|
| PostgreSQL version | 17.11 |
| `gern` is superuser | **No** — superusers bypass RLS entirely, so this matters |
| `gern` has `BYPASSRLS` | **No** |
| `gern` can `CREATE ROLE` | **No** — permission denied |
| `ENABLE ROW LEVEL SECURITY` | ✅ works |
| `FORCE ROW LEVEL SECURITY` | ✅ works |
| `CREATE POLICY` | ✅ works |

So RLS is available and will actually apply to this role. **Creating a second database role
is not possible from SQL** — roles are managed through the OVH console, the same way `gern`
and the `archive` database were created.

### 2.1 Which role the application uses

The textbook setup has the application connect as a role that does not own the tables, so
it cannot turn RLS off. `gern` owns them, and owners bypass RLS unless `FORCE` is set.

**Recommendation: use `FORCE ROW LEVEL SECURITY` with `gern`, and do not add a second
role.** Three reasons:

1. The owner-bypass loophole requires the application to *execute arbitrary DDL* —
   realistically, SQL injection. **There is none available**: every query in this codebase
   is a static string with bound parameters, verified by grep; no user input is ever
   interpolated into SQL. The one place a pattern is built (`search`) builds a *value* that
   is then bound.
2. A second role means a second credential to deliver through Terraform, rotate and lose —
   against a stated constraint, and against the one-part-time-maintainer premise.
3. It cannot be done without an OVH console action anyway, which puts a manual step in the
   middle of a security control.

Revisit if the archive ever grows a query built from user input, or if it is opened to
untrusted callers. Record that trigger; do not pre-build for it.

---

## 3. The schema problem

Policies want a cheap predicate. Only two of the four tables can offer one:

| Table | Has `user_id`? | Policy today would be |
|---|---|---|
| `accounts` | ✅ | `user_id = <session>` |
| `messages` | ✅ | `user_id = <session>` |
| `folders` | ❌ (`account_id`) | subquery through `accounts` |
| `placements` | ❌ (`folder_id`) | subquery through `folders` → `accounts` |

A subquery per row on `placements` — 152,741 rows and the hottest table in the system —
is not acceptable, and nesting policies is subtle: a policy's subquery is itself
RLS-filtered, which is correct but very hard to reason about when something is slow or
empty.

**So: denormalise `user_id` onto `folders` and `placements`.** Every policy then becomes
the same one-column comparison, indexable and obvious.

Cost, stated honestly: a migration, a backfill over ~152,000 placement rows, an index, and
two more columns that ingest must keep correct. The redundancy is real — `placements.user_id`
must always agree with `folders → accounts → users`. That is enforceable with a composite
foreign key, which is the reason to do it properly rather than trusting ingest:

```
folders    (id, account_id, user_id)  UNIQUE (id, user_id)
placements (folder_id, user_id, ...)  FOREIGN KEY (folder_id, user_id)
                                          REFERENCES folders (id, user_id)
```

With that, a placement whose `user_id` disagrees with its folder's is rejected by the
database. The denormalised column cannot drift.

---

## 4. Which tables get RLS

**`accounts`, `folders`, `messages`, `placements`.** All four hold mail or its structure.

**`users` deliberately does not.** Authentication has to find a row *before* an identity
exists — `db::authenticate` searches by login, with no user to scope to. A policy there
would have to be permissive enough to allow that, which would make it decorative. The table
holds password hashes and bucket names, no mail, and the hashes are Argon2id.

`_sqlx_migrations` likewise: no user data, and migrations run before any identity is set.

---

## 5. Setting the identity — the part most likely to go wrong

The policy reads `current_setting('archive.user_id', true)`. Something has to set it, once
per request, and **connection pooling makes the obvious approach dangerous**.

A plain `SET` persists on the pooled connection. The next request to borrow that connection
inherits the previous request's identity — which is *worse than no RLS at all*, because it
converts a scoping bug into a cross-user leak that depends on pool timing and would be
close to impossible to reproduce.

**So: `SET LOCAL`, inside a transaction, always.** `SET LOCAL` is scoped to the
transaction and unwinds at COMMIT or ROLLBACK whether or not our code remembers.

```
BEGIN;
SET LOCAL archive.user_id = '1';
  ...the actual query...
COMMIT;
```

This must be a single helper that every read goes through, not a convention. If it is
possible to run a query outside it, one eventually will be.

**Latency.** Three round trips instead of one. At the 128 ms baseline measured from a
development machine that is ~380 ms per request; from the instance, where the database is
a fraction of a millisecond away, it is noise. `BEGIN` and `SET LOCAL` can be sent as one
statement, keeping it to three rather than four. The cost lands almost entirely on local
development, which is the right place for it to land.

---

## 6. The failure mode to design against

**A half-finished RLS deployment returns an empty archive, not an error.**

`current_setting(..., true)` yields NULL when unset, the policy evaluates false, and every
query returns zero rows. Not a crash, not a log line — an archive that looks empty. If that
reaches production it will read as catastrophic data loss until someone thinks to check the
session variable.

Three things guard against it:

1. **Nothing enables RLS until the session helper is in place and every call site uses
   it.** The order in §7 is not negotiable for this reason.
2. **A test that queries with no identity set and asserts zero rows** — proving the policy
   is actually applied, and pinning the failure to something a test can see.
3. **A startup self-check**: on boot, set an identity, read one known row back, and refuse
   to start if it comes back empty. A server that cannot see mail should not accept
   connections and serve emptiness to its users.

---

## 7. Phasing

Each phase is independently deployable and leaves the system working.

| Phase | Work | Gate |
|---|---|---|
| **1** | Migration: `user_id` on `folders` and `placements`, backfilled, with the composite FK of §3. **No RLS yet.** | Existing tests pass; counts unchanged; a deliberate mismatched insert is rejected |
| **2** | `db::scoped()` helper: transaction + `SET LOCAL`. Route every read through it. Still no RLS, so behaviour is unchanged and any mistake is visible as a normal bug | All existing queries still return what they did; integration tests green |
| **3** | Enable RLS + FORCE + policy on **`accounts` only** — the smallest table, and the one whose breakage is most obvious | Login, folder list and ingest all still work |
| **4** | The same on `folders`, `messages`, `placements` | Full app exercised: browse, read, search, download, ingest |
| **5** | Verification: no-identity queries return zero rows; a second user's ids return zero rows; startup self-check | Tests assert the policy, not just the `WHERE` clause |

Phase 2 is the one that carries real risk and the one worth reviewing carefully. Phases 3–4
are almost anticlimactic if it is right, and unsafe at any speed if it is not.

---

## 8. Risks

| # | Risk | Handling |
|---|---|---|
| R1 | **Silent empty archive** from RLS enabled before scoping is complete | §6: strict phase order, no-identity test, startup self-check |
| R2 | **Identity leaking across pooled connections** | `SET LOCAL` in a transaction only; never a bare `SET` |
| R3 | Denormalised `user_id` drifting from the real ownership | Composite foreign key (§3) makes disagreement impossible |
| R4 | Ingest and the IMAP server also need an identity | Both are already per-account or per-connection; they set it the same way. Neither can bypass — `gern` has no `BYPASSRLS` |
| R5 | Backfilling 152k rows locks the table | Batch the update; the archive is read-only in practice, so a slow backfill inconveniences nobody |
| R6 | Latency of three round trips per read | Negligible from the instance; noticeable only in local development (§5) |
| R7 | A future query built from user input reopens the owner-bypass hole | §2.1 records this as the trigger to revisit the separate-role decision |

---

## 9. Open questions

**Q1 — Is this worth doing at all, for four users who all trust each other?** The honest
case for yes: the users are family and colleagues, but the *mail* includes twenty years of
correspondence, and "accidentally" is the word in the original question. The case for no:
it is a week of careful work on a system that currently has no known leak, and every phase
carries the R1 risk. *Worth deciding deliberately before phase 1, not drifting into.*

**Q2 — Should ingest run under an identity, or is it exempt?** It writes across users over
long sessions. Setting the identity per account is natural, but it means a long-running
transaction or a re-set per batch. *Decide before phase 2; it shapes the helper.*

**Q3 — Startup self-check on which row?** It must not require a particular user to exist.
Counting rows visible under a known identity and asserting non-zero works only for an
archive with content. *A fresh, empty archive must not fail to start.*
