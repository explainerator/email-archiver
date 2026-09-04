# Reading the database by hand

psql and DBeaver connect as `gern` and are subject to the same row-level policies as the
application, so **a fresh session sees empty tables**. Declare an identity first:

```sql
SET archive.user_id = '1';         -- 1 = ken, 2 = jaqui; see: email users
SET archive.google_access = 'yes'; -- only for google_domains
```

`SET` rather than `SET LOCAL`: the application must use `SET LOCAL` so an identity cannot
outlive its transaction on a pooled connection, but an interactive session owns its
connection and wants the setting to persist.
