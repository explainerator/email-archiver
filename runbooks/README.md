# Runbooks

One file per job. Design reasoning lives in `../ARCHIVE-PLAN.md`,
`../WEBAPP-PLAN.md` and `../RLS-PLAN.md` — these are for doing things.

## Setting up

| | |
|---|---|
| [new-user.md](new-user.md) | **Add a person and their first Gmail mailbox.** The whole thing, in order — start here for a new colleague. |
| [add-user.md](add-user.md) | Add a person on their own: bucket, login, password, aliases. |
| [import-gmail.md](import-gmail.md) | Archive a Google Workspace mailbox. |
| [import-imap.md](import-imap.md) | Archive any other IMAP mailbox. |

## Running it

| | |
|---|---|
| [deploy.md](deploy.md) | Ship a new build to the instance. |
| [keeping-current.md](keeping-current.md) | How the archive stays up to date, and the `follow` flag that controls it. |
| [routine-checks.md](routine-checks.md) | What to run to confirm things are healthy. |
| [certificates.md](certificates.md) | Renewal, and the one thing that would break it. |

## When something is wrong

| | |
|---|---|
| [troubleshooting.md](troubleshooting.md) | Symptom → cause → fix. Try here first. |
| [source-will-not-connect.md](source-will-not-connect.md) | A mailbox that will not authenticate, or hangs. |
| [empty-messages.md](empty-messages.md) | Messages archived with no content, and how to re-fetch them. |
| [database-by-hand.md](database-by-hand.md) | psql and DBeaver, and why they show empty tables at first. |

---

## Conventions used throughout

Commands appear as typed in **gern-shell**, where `email` is the verb:

```
email users
```

Outside gern-shell the same command is `email-archiver users`, with
`EMAIL_ARCHIVER_CONFIG` pointing at `config.toml`. gern-shell sets that for you,
and rebuilds the binary before every command so it is never running a stale one.

**Where things run.** Ingest runs from a workstation — it needs reach to the
source mail servers, not to the archive. Serving (IMAPS 993, HTTPS 443) runs on
the instance. Both use the same binary and the same database.

**The archive is read-only after import.** The only thing any client can change
is whether a message is marked read, and the schema enforces that. No procedure
here can lose mail.
