# Deploy

```
deploy email-archiver
```

Builds the binary and the frontend, ships both, obtains or renews the Let's Encrypt
certificate, installs the config Terraform rendered, and restarts both units. It verifies
before declaring success: IMAP must answer with a greeting and `/api/health` must return
200 over TLS.

If gern-shell has been open since `archiver.sh` last changed, `deploy` refuses and
tells you to reload it. That is deliberate: these functions are sourced into a
long-lived shell, and a deploy once wrote a systemd unit missing a flag added
hours earlier. The service came up healthy and silently did not do the thing.

```
source tools/gern-shell/archiver.sh
```

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
