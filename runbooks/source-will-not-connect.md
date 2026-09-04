# When a source will not connect

```
email probe <address>
```

Shows the raw IMAP conversation — greeting, authentication, folder listing — with every
read on a deadline, so a server that goes quiet produces an error rather than a hang. It
resolves the source exactly as ingest does, including the certificate policy, so it cannot
succeed where ingest would fail.

For Gmail it decodes the base64 error challenges, which is where Google puts the actual
reason and which is otherwise unreadable.
