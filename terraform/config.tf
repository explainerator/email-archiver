# -----------------------------------------------------------------------------
# Runtime configuration for the archiver
# -----------------------------------------------------------------------------
# Rendered here and delivered to the instance by the deploy step (Phase 7),
# never written to disk in the repository. It contains live S3 secret keys, so
# it is exposed only as a sensitive output:
#
#   terraform output -raw archiver_config > config.toml    # gitignored
#
# Terraform owns this file. Editing it on the instance is pointless — the next
# apply overwrites it.
# -----------------------------------------------------------------------------

variable "database_url" {
  description = <<-EOT
    Postgres connection string for the `archive` database. Contains a password,
    so set it in secrets.tfvars, not terraform.tfvars.

    postgres://gern:PASSWORD@qw300972-001.ca.clouddb.ovh.net:35628/archive?sslmode=require
  EOT
  type        = string
  sensitive   = true
  default     = ""

  validation {
    # Cheap guard against pointing the archiver at the game services' database.
    condition     = var.database_url == "" || can(regex("/archive(\\?|$)", var.database_url))
    error_message = "database_url must target the `archive` database, not defaultdb."
  }
}

variable "source_accounts" {
  description = <<-EOT
    How to log in to each SOURCE mailbox being archived, keyed by address.
    Contains passwords, so set it in secrets.tfvars.

    Postgres remains authoritative for which accounts exist (accounts.address);
    this only says how to authenticate to one.

    source_accounts = {
      "ken@twoducks.ca" = {
        host     = "mail.example.com"
        port     = 993
        username = "ken@twoducks.ca"
        password = "..."
      }
    }
  EOT
  type = map(object({
    host     = string
    port     = optional(number, 993)
    username = string
    password = string
  }))
  sensitive = true
  default   = {}
}

locals {
  archiver_config = templatefile("${path.module}/files/config.toml.tftpl", {
    database_url = var.database_url
    s3_endpoint  = "https://s3.${lower(var.storage_region)}.io.cloud.ovh.net"
    sources      = var.source_accounts
    s3_region    = lower(var.storage_region)

    # bucket -> credentials. Keyed by bucket to match how the archiver looks
    # them up (users.bucket in Postgres), not by our internal user key.
    buckets = {
      for key, bucket in local.user_buckets :
      bucket => {
        access_key = ovh_cloud_project_user_s3_credential.user[key].access_key_id
        secret_key = ovh_cloud_project_user_s3_credential.user[key].secret_access_key
      }
    }
  })
}

output "archiver_config" {
  description = "Rendered config.toml. Write it with: terraform output -raw archiver_config > config.toml"
  value       = local.archiver_config
  sensitive   = true
}
