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

variable "ingest_concurrency" {
  description = "Messages processed in parallel per fetched batch. Latency-bound work, so this may exceed the core count."
  type        = number
  default     = 8
}

variable "database_max_connections" {
  description = "Postgres pool ceiling. Shared cluster, so this is a courtesy limit as much as a tuning knob. Keep >= ingest_concurrency."
  type        = number
  default     = 8
}

variable "tls_cert_path" {
  description = "Full-chain certificate on the instance, e.g. /etc/letsencrypt/live/archive.thebackroom420.ca/fullchain.pem. Empty serves plaintext, which is only permitted on loopback."
  type        = string
  default     = ""
}

variable "tls_key_path" {
  description = "Private key on the instance, e.g. /etc/letsencrypt/live/archive.thebackroom420.ca/privkey.pem."
  type        = string
  default     = ""
}

variable "encryption_key" {
  description = <<-EOT
    Base64 32-byte key encrypting source mailbox passwords in the database.
    Generate with: email-archiver generate-key

    Set in secrets.tfvars. Changing it makes every stored source password
    unreadable — they would all need re-entering.

    Source credentials themselves are no longer rendered here; they live in the
    accounts table, encrypted, and are set with `email-archiver set-source`.
  EOT
  type        = string
  sensitive   = true
  default     = ""
}


locals {
  archiver_config = templatefile("${path.module}/files/config.toml.tftpl", {
    database_url   = var.database_url
    s3_endpoint    = "https://s3.${lower(var.storage_region)}.io.cloud.ovh.net"
    encryption_key = var.encryption_key
    tls_cert_path  = var.tls_cert_path
    tls_key_path   = var.tls_key_path

    ingest_concurrency       = var.ingest_concurrency
    database_max_connections = var.database_max_connections
    s3_region                = lower(var.storage_region)

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
