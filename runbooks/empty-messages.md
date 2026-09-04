# Messages archived as empty

```
email repair <address>          # list them
email repair <address> --fix    # re-fetch
```

A fetch that comes back with no body is stored as an empty message rather than
failing the run. That is deliberate: an error would leave the resume point stuck
before it, and one message the server cannot deliver would block every message
behind it in that folder — permanently and quietly.

Every empty body hashes to the same value (blake3 of zero bytes), and no real
message can, so the archive carries an exact list of its own gaps. `repair` reads
that list. Three outcomes:

| Result | Meaning |
|---|---|
| `repaired, N bytes` | The first fetch failed transiently. Now archived properly. |
| `empty at the source too` | The message really is zero bytes on the server. Nothing to repair, and never will be. |
| `FAILED` | Something else — the message is gone, or the server refused. The error names which. |

Worth running after any ingest that reported connection trouble.
