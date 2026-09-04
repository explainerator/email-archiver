# Import a Google Workspace mailbox

The example is `ken@thebackroom420.ca` landing in `ken@twoducks.ca`'s archive.

> For a mailbox belonging to a person who does not exist yet, follow
> [new-user.md](new-user.md) instead — it runs both procedures in the right
> order, and includes the deploy that neither covers on its own.

### 1. Google side — once per domain, as the Workspace admin

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

### 2. Store the key — once per domain

```
email set-google thebackroom420.ca /path/to/service-account.json
```

Read once and kept encrypted in the database. **Delete the file afterwards**; nothing needs
it again, and it can read every mailbox in the domain.

One key covers the whole domain, so this is not repeated for the other mailboxes on it.

### 3. Register the mailbox — once per mailbox

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

### 4. Import

```
email ingest ken@thebackroom420.ca
```

Resumable — re-running continues where it stopped, so an interruption costs nothing.

Then start keeping it current:

```
email follow ken@thebackroom420.ca on
```

### 5. Confirm

```
email accounts ken@twoducks.ca      # should show: google (domain key)
email check ken@twoducks.ca         # Postgres and S3 agree
```
