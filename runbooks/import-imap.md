# Import a generic IMAP mailbox

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
