# email-archiver runbook

Operational procedures. Design reasoning lives in `ARCHIVE-PLAN.md`, `WEBAPP-PLAN.md` and
`RLS-PLAN.md` — this file is for doing things.

Commands are shown as they are typed in **gern-shell**, where `email` is the verb:

```
email users
```

Outside gern-shell the same command is `email-archiver users`, with
`EMAIL_ARCHIVER_CONFIG` pointing at `config.toml`. gern-shell sets that for you.

**Where things run.** Ingest runs from a workstation — it needs reach to the source mail
servers, not to the archive. Serving (IMAPS 993, HTTPS 443) runs on the instance. Both use
the same binary and the same database.

---

## 1. Import a Google Workspace mailbox

The example is `ken@thebackroom420.ca` landing in `ken@twoducks.ca`'s archive.

### 1.1 Google side — once per domain, as the Workspace admin

1. **Cloud console** → new project → enable the **Gmail API**.
2. Create a **service account** → **Keys** → create a **JSON key**. Save the file.
3. On the service account's details page, copy its **Client ID** — the long number, *not*
   the `@…gserviceaccount.com` address.
4. **Admin console** → Security → Access and data control → **API controls** →
   **Domain-wide delegation** → *Add new*:
   - Client ID: from step 3
   - Scopes: `https://mail.google.com/`

> **All four steps are required.** Step 4 is called out because it is the only one whose
> omission is not obvious at the time: steps 1–3 fail visibly and immediately if you get
> them wrong, whereas skipping step 4 leaves everything looking finished — the key
> downloads, `set-google` accepts it, `add-account` succeeds — and the first sign of
> trouble is `unauthorized_client` when you finally run `ingest`, which reads like a bad
> key rather than a missing authorisation.
>
> `https://mail.google.com/` looks broader than it should be and is not avoidable: it is
> the only scope Gmail's IMAP endpoint accepts. `gmail.readonly` covers the REST API and
> IMAP refuses it.

### 1.2 Store the key — once per domain

```
email set-google thebackroom420.ca /path/to/service-account.json
```

Read once and kept encrypted in the database. **Delete the file afterwards**; nothing needs
it again, and it can read every mailbox in the domain.

One key covers the whole domain, so this is not repeated for the other mailboxes on it.

### 1.3 Register the mailbox — once per mailbox

```
email add-account ken@twoducks.ca ken@thebackroom420.ca backroom gmail
```

- `ken@twoducks.ca` — the archive **user** who will own this mail, and whose bucket it
  lands in. Aliases work here.
- `ken@thebackroom420.ca` — the **mailbox** being imported.
- `backroom` — the folder prefix. Folders arrive as `backroom/INBOX`, `backroom/Sent`.
- `gmail` — selects XOAUTH2 via the domain key.

**No `set-source`.** Workspace accounts store no host and no password; that is the point of
the delegation.

### 1.4 Import

```
email ingest ken@thebackroom420.ca
```

Resumable — re-running continues where it stopped, so an interruption costs nothing.

Then start keeping it current:

```
email follow ken@thebackroom420.ca on
```

### 1.5 Confirm

```
email accounts ken@twoducks.ca      # should show: google (domain key)
email check ken@twoducks.ca         # Postgres and S3 agree
```

---

## 2. Import a generic IMAP mailbox

```
email add-account ken@twoducks.ca info@kenduck.ca kenduck imap
email set-source info@kenduck.ca mail.kenduck.ca info@kenduck.ca
email ingest info@kenduck.ca
email follow info@kenduck.ca on
```

`set-source` prompts for the password without echoing. **Never pass it as an argument** —
the shell rewrites `!`, `$`, backticks and quotes, so the stored value would differ from
what you typed, and it lands in shell history and the process list.

If the server's certificate cannot be fixed:

```
email insecure-tls info@kenduck.ca on
```

Encrypted but **not authenticated** — anyone on the route can present their own
certificate, take the mailbox password, and hand back whatever they like as mail. Accounts
in this state are flagged `[certs not verified]` in `email accounts` and `email sources`.

---

## 3. Add a person

New users need a bucket, which is Terraform's job.

1. Add them to the `users` map in `terraform/terraform.tfvars`:

   ```hcl
   users = {
     ken   = "ken@twoducks.ca"
     jaqui = "art@jduck.ca"
     newby = "new@example.com"       # key is internal; the ADDRESS matters
   }
   ```

   **Never rename an existing key.** `storage.tf` keys the S3 user, credential and policy
   off it via `for_each`, so a rename reads as destroy-and-recreate and the bucket's
   `prevent_destroy` will refuse the plan.

2. `terraform apply -var-file=secrets.tfvars`, then re-render the config:

   ```
   terraform output -raw archiver_config > config.toml
   ```

3. Register them and set a password:

   ```
   email add-user new@example.com new-example-com "Their Name"
   email set-password new@example.com
   ```

   The bucket name comes from `terraform output user_buckets`.

4. Then add their mailboxes per §1 or §2.

**Logins are email addresses**, and a person may have several:

```
email alias new@example.com other@example.com
```

An alias works everywhere the primary does — login, and every command that names a user.

---

## 4. Deploy

```
deploy email-archiver
```

Builds the binary and the frontend, ships both, obtains or renews the Let's Encrypt
certificate, installs the config Terraform rendered, and restarts both units. It verifies
before declaring success: IMAP must answer with a greeting and `/api/health` must return
200 over TLS.

**Deploy after any schema change.** The database is shared between your workstation and the
instance, so a migration applied locally leaves the instance running a binary that does not
know about it. sqlx refuses to start in that state rather than guessing — loud, but the
service is down until you deploy.

Check afterwards:

```
status
```

Look for the `archive` rows: both units `Running`, `imaps 993 open`, `https 443 open`, and
`cert Nd left`.

---

## 4a. Keeping the archive current

Automatic. The IMAP service sweeps **INBOX every 10 minutes** and **every folder
every 30**, from inside the process it already runs -- no cron entry, no timer, no
second copy of the binary scheduled from outside.

Only accounts with the **follow** flag are swept, and a newly registered account
does **not** have it. That is deliberate: its first sweep would not be a catch-up
of a few seconds but a full import of the whole mailbox, and because the sweep is
sequential every other account would wait hours behind it.

So the last step of adding any account is to start following it:

```
email ingest new@example.com      # the first import, by hand
email follow new@example.com on   # now its sweeps are the cheap kind
```

`email accounts <user>` marks anything unfollowed, which is how you catch one
that was imported and then forgotten.

When a source is shut down -- a lapsed domain, a closed account -- stop reaching
for it:

```
email follow old@example.com off
```

Nothing is deleted. Every message, folder and credential stays exactly as it is;
only the checking for new mail stops, and `email accounts` marks the account
`[not followed]`. Reversible with `on`.

To sweep immediately rather than waiting for the next tick:

```
email refresh
```

That does every folder of every followed account and reports which, if any,
failed. One account failing never stops the others.

**Where it runs.** The scheduler is enabled on the IMAP unit only, by
`serve --refresh` in the unit file. Both units run the same binary against the
same archive, so enabling it on the web unit as well would sweep every mailbox
twice. Watch it with:

```
ssh ubuntu@<instance> 'journalctl -u email-archiver -f'
```

---

## 5. Routine checks

```
email users                     # who exists, aliases, how much mail each holds
email sources                   # every source, and whether it can actually be ingested
email accounts <user email>     # one user's mailboxes
email check <user email>        # Postgres and S3 agree; samples 5 blobs
email check <user email> --deep # re-hashes every message body. Slow.
```

`email sources` flags accounts registered but with no credentials — the usual reason an
ingest will not start.

---

## 5a. When a source will not connect

```
email probe <address>
```

Shows the raw IMAP conversation — greeting, authentication, folder listing — with every
read on a deadline, so a server that goes quiet produces an error rather than a hang. It
resolves the source exactly as ingest does, including the certificate policy, so it cannot
succeed where ingest would fail.

For Gmail it decodes the base64 error challenges, which is where Google puts the actual
reason and which is otherwise unreadable.

---

## 6. Reading the database by hand

psql and DBeaver connect as `gern` and are subject to the same row-level policies as the
application, so **a fresh session sees empty tables**. Declare an identity first:

```sql
SET archive.user_id = '1';         -- 1 = ken, 2 = jaqui; see: email users
SET archive.google_access = 'yes'; -- only for google_domains
```

`SET` rather than `SET LOCAL`: the application must use `SET LOCAL` so an identity cannot
outlive its transaction on a pooled connection, but an interactive session owns its
connection and wants the setting to persist.

---

## 7. When something goes wrong

| Symptom | Cause | Fix |
|---|---|---|
| `unauthorized_client` on a Google ingest | Domain-wide delegation not granted | §1.1 step 4 |
| `no Google service account is configured for <domain>` | Key never stored | §1.2 |
| `no account "<address>" in the database` | Account not registered, or credentials missing | `email sources` |
| Tables look empty in psql/DBeaver | Row-level security, working correctly | §6 |
| A service will not start after a migration | Instance binary predates the schema | `deploy email-archiver` |
| Everything reads empty in the app | A code path is not declaring an identity | Not a data loss — check `email check <user>`, then the logs |
| Ingest stops partway | Network blip | Re-run it; ingest is resumable |
| Ingest hangs with no output after `ingesting …` | The IMAP conversation stalled | `email probe <address>` — prints what the server actually said |
| A Google ingest reports a different address than the one registered | The mailbox's real address differs from the alias domain you registered | Cosmetic; re-register under the real address if you want the two to agree |
| Folder shows fewer messages than the source | Usually byte-identical duplicates collapsing | `email diagnose <address> <folder>` |

**An empty archive is not lost data.** Every policy-covered read returns nothing when the
caller has not said who it is. `email check <user email>` reads through a declared identity
and will show the true counts.

---

## 8. Certificate renewal

Automatic — certbot's systemd timer, with a renewal hook that keeps the certificate
readable by the service. The archiver re-reads it when the file changes, so renewal needs
no restart.

**Port 80 must stay free.** `certbot --standalone` binds it at *every* renewal, not only at
issuance. Nothing listens there by design; putting an HTTP redirect on it would work for
about ninety days and then fail silently.

`status` shows days remaining. If it stops falling, renewal has stopped working:

```
ssh ubuntu@<instance> 'sudo certbot renew --dry-run'
```
