# Adding a person and their Gmail mailbox

Start to finish, in order. Every step, no branching except where marked.

The other files here each cover one task. This one spans several of them, so it
is written out in full rather than as a trail of pointers.

**Example throughout:** adding *Sam Chen* (`sam@twoducks.ca`), archiving their
Google Workspace mailbox `sam@thebackroom420.ca`.

Commands are as typed in **gern-shell**. Outside it, `email X` is
`email-archiver X` with `EMAIL_ARCHIVER_CONFIG` pointing at `config.toml`.

---

## Before you start

Three things decided up front, because two of them are painful to change later.

| | Example | Notes |
|---|---|---|
| Their archive address | `sam@twoducks.ca` | Their login. Can be renamed later. |
| Terraform key | `sam` | **Never renameable.** See step 1. |
| Source mailbox | `sam@thebackroom420.ca` | The Gmail account being archived. |

You also need: Terraform access, SSH to the instance, and — **only if this is
the first mailbox on its Google domain** — Workspace admin rights.

---

## Step 1 — Create their bucket (Terraform)

Each user owns one S3 bucket. Terraform makes it.

Edit `terraform/terraform.tfvars`:

```hcl
users = {
  ken   = "ken@twoducks.ca"
  jaqui = "art@jduck.ca"
  sam   = "sam@twoducks.ca"      # added
}
```

> **Never rename an existing key.** `storage.tf` keys the bucket, S3 user,
> credential and policy off it with `for_each`, so a rename reads as
> destroy-and-recreate. The bucket's `prevent_destroy` will refuse the plan —
> which is the safety net working, but it means a rename is a manual migration,
> not an edit. The key is internal; only the address is ever displayed.

Apply, then re-render the config:

```
cd terraform
terraform apply -var-file=secrets.tfvars
terraform output -raw archiver_config > ../config.toml
```

The bucket name is derived from the address — `@` and `.` become `-`, all
lowercase — so `sam@twoducks.ca` gives `sam-twoducks-ca`. Confirm with:

```
terraform output user_buckets
```

---

## Step 2 — Register the user

```
email add-user sam@twoducks.ca sam-twoducks-ca "Sam Chen"
email set-password sam@twoducks.ca
```

The bucket argument must match step 1 exactly. `set-password` prompts twice
without echoing; until it is set the account cannot be logged into at all.

**Do not pass the password as an argument.** The shell rewrites `!`, `$`,
backticks and quotes before the binary sees them, so the stored hash can cover
a string you are unable to retype.

If they use more than one address:

```
email alias sam@twoducks.ca s.chen@twoducks.ca
```

An alias works everywhere the primary does — login, and every command that
names a user.

---

## Step 3 — Google domain key

**Skip this entire step if the domain already has a key.** Check:

```
email sources
```

If `thebackroom420.ca` is listed under *Google Workspace domains*, go to step 4.
One key covers every mailbox in the domain.

### 3a. Google side — once per domain, as the Workspace admin

All four sub-steps are required.

1. **Cloud console** → new project → enable the **Gmail API**.
2. Create a **service account** → **Keys** → create a **JSON key**. Save the file.
3. On the service account's details page, copy its **Client ID** — the long
   number, *not* the `@…gserviceaccount.com` address.
4. **Admin console** → Security → Access and data control → **API controls** →
   **Domain-wide delegation** → *Add new*:
   - Client ID: from step 3
   - OAuth scope: `https://mail.google.com/`

> Step 4 is singled out because it is the only one whose omission is not
> obvious at the time. Steps 1–3 fail visibly and immediately if you get them
> wrong. Skipping step 4 leaves everything looking finished — the key
> downloads, `set-google` accepts it, `add-account` succeeds — and the first
> sign of trouble is `unauthorized_client` when you finally run `ingest`, which
> reads like a bad key rather than a missing authorisation.
>
> `https://mail.google.com/` is broader than it looks and is not avoidable: it
> is the only scope Gmail's IMAP endpoint accepts. `gmail.readonly` covers the
> REST API, and IMAP refuses it.

### 3b. Store the key

```
email set-google thebackroom420.ca /path/to/service-account.json
```

Windows paths work as typed:
`email set-google thebackroom420.ca C:\Users\you\Downloads\key.json`

Read once, then kept encrypted in the database. **Delete the file afterwards** —
nothing needs it again, and it can read every mailbox in the domain.

---

## Step 4 — Register the mailbox

```
email add-account sam@twoducks.ca sam@thebackroom420.ca backroom gmail
```

| Argument | Meaning |
|---|---|
| `sam@twoducks.ca` | The archive **user** who owns this mail, and whose bucket it lands in |
| `sam@thebackroom420.ca` | The **mailbox** being archived |
| `backroom` | Folder prefix — folders arrive as `backroom/INBOX`, `backroom/Sent` |
| `gmail` | Authenticate with the domain key |

**No `set-source`.** Workspace accounts store no host and no password; that is
what the delegation is for.

The command prints a reminder that the account is not yet followed. That is
deliberate — step 5 explains why.

---

## Step 5 — First import, by hand

```
email ingest sam@thebackroom420.ca
```

Expect **hours** for a large mailbox, and watch the first few minutes to see
that it authenticates and starts fetching.

> **Why by hand, and why now.** New accounts are registered *unfollowed*. The
> scheduled sweep runs accounts one after another, so an un-imported account
> joining it would spend hours pulling its whole mailbox while every other
> account waited behind it. Importing first, following after, keeps that out of
> the rotation.

Resumable — re-running continues where it stopped, so an interruption costs
nothing. If it stalls with no output, `email probe sam@thebackroom420.ca` shows
the raw IMAP conversation with deadlines on every read.

---

## Step 6 — Start following

Only once the import has finished:

```
email follow sam@thebackroom420.ca on
```

From here the service keeps it current by itself: INBOX every 10 minutes, every
folder every 30. Nothing further to schedule.

---

## Step 7 — Deploy

**Required, not optional.** Step 1 rewrote `config.toml` with the new bucket's
S3 credentials, and the instance is still running the old one — which has no
credentials for this bucket at all.

The services still start, because credentials are looked up per bucket when
first read rather than validated up front. So the failure waits until Sam
actually opens a message, and then reads
`no S3 credentials configured for bucket "sam-twoducks-ca"`. Everything looks
fine until the one person who cares tries to use it.

```
deploy email-archiver
```

If gern-shell has been open since before the last change to `archiver.sh`, it
will refuse and tell you to reload. Do that and re-run.

---

## Step 8 — Confirm

```
email users                        # Sam listed, with a message count
email accounts sam@twoducks.ca     # shows: google (domain key)
email check sam@twoducks.ca        # Postgres and S3 agree
email repair sam@thebackroom420.ca # any messages that fetched empty
status                             # both units running, cert valid
```

`email repair` is worth running after any first import: a fetch that returns no
body is archived as an empty message rather than failing the run, and this finds
them exactly. Add `--fix` to re-fetch.

Then have Sam sign in at `https://archive.thebackroom420.ca` with
`sam@twoducks.ca` and the password from step 2.

---

## If something goes wrong

| Symptom | Cause | Fix |
|---|---|---|
| `unauthorized_client` on ingest | Domain-wide delegation not granted | Step 3a, sub-step 4 |
| `no Google service account is configured for <domain>` | Key never stored | Step 3b |
| `no account "<address>" in the database` | Not registered | Step 4 |
| Terraform refuses to plan | An existing `users` key was renamed | Restore the key; it is internal and never displayed |
| Ingest hangs with no output | The IMAP conversation stalled | `email probe <address>` |
| Sam logs in, then `no S3 credentials configured for bucket "sam-twoducks-ca"` | Instance is on the old config | Step 7 |
| Mail arrives but never updates | Account never followed | Step 6 |

The archive is read-only after import, so none of the above risks the mail
itself. [troubleshooting.md](troubleshooting.md) covers the rest.
