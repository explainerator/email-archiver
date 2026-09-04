# Add a person

> **Adding a person AND their first mailbox in one go?** [new-user.md](new-user.md)
> is that whole procedure written out in order, including the deploy this file
> does not cover. This is the reference for the user-creation half on its own.

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

4. Then add their mailboxes: [import-gmail.md](import-gmail.md) for Google
   Workspace, [import-imap.md](import-imap.md) for anything else.

**Logins are email addresses**, and a person may have several:

```
email alias new@example.com other@example.com
```

An alias works everywhere the primary does — login, and every command that names a user.
