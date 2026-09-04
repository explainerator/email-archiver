# When something goes wrong

| Symptom | Cause | Fix |
|---|---|---|
| `unauthorized_client` on a Google ingest | Domain-wide delegation not granted | [import-gmail.md](import-gmail.md) — Google side, sub-step 4 |
| `no Google service account is configured for <domain>` | Key never stored | [import-gmail.md](import-gmail.md), step 2 |
| `no account "<address>" in the database` | Account not registered, or credentials missing | `email sources` |
| Tables look empty in psql/DBeaver | Row-level security, working correctly | [database-by-hand.md](database-by-hand.md) |
| A service will not start after a migration | Instance binary predates the schema | `deploy email-archiver` |
| Everything reads empty in the app | A code path is not declaring an identity | Not a data loss — check `email check <user>`, then the logs |
| Ingest stops partway | Network blip | Re-run it; ingest is resumable |
| Ingest hangs with no output after `ingesting …` | The IMAP conversation stalled | `email probe <address>` — prints what the server actually said |
| A Google ingest reports a different address than the one registered | The mailbox's real address differs from the alias domain you registered | Cosmetic; re-register under the real address if you want the two to agree |
| Folder shows fewer messages than the source | Usually byte-identical duplicates collapsing | `email diagnose <address> <folder>` |

**An empty archive is not lost data.** Every policy-covered read returns nothing when the
caller has not said who it is. `email check <user email>` reads through a declared identity
and will show the true counts.
