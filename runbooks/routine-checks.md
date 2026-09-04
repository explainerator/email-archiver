# Routine checks

```
email users                     # who exists, aliases, how much mail each holds
email sources                   # every source, and whether it can actually be ingested
email accounts <user email>     # one user's mailboxes
email check <user email>        # Postgres and S3 agree; samples 5 blobs
email check <user email> --deep # re-hashes every message body. Slow.
```

`email sources` flags accounts registered but with no credentials — the usual reason an
ingest will not start.
