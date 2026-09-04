# Keeping the archive current

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
ssh ubuntu@<instance> 'sudo journalctl -u email-archiver -f'
```

`sudo` is not optional: the `ubuntu` account is in no privileged group, so an
unprivileged `journalctl` reports **No entries** for a service that is logging
perfectly well — which reads exactly like a service that has gone quiet.
