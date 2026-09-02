# -----------------------------------------------------------------------------
# Outputs
# -----------------------------------------------------------------------------

output "s3_endpoint" {
  description = "S3 endpoint for the Standard storage class. Goes into Stalwart's [store.\"s3\"] endpoint setting."
  value       = "https://s3.${lower(var.storage_region)}.io.cloud.ovh.net"
}

output "s3_region" {
  description = "Region name for Stalwart's [store.\"s3\"] region setting (lowercase)."
  value       = lower(var.storage_region)
}

output "user_buckets" {
  description = "Map of user key -> bucket name."
  value       = local.user_buckets
}

output "user_s3_access_keys" {
  description = "Map of user key -> S3 access key id. Read one with: terraform output -json user_s3_access_keys"
  value       = { for k, c in ovh_cloud_project_user_s3_credential.user : k => c.access_key_id }
  sensitive   = true
}

output "user_s3_secret_keys" {
  description = "Map of user key -> S3 secret key. Plaintext in terraform.tfstate — see storage.tf."
  value       = { for k, c in ovh_cloud_project_user_s3_credential.user : k => c.secret_access_key }
  sensitive   = true
}

# Read the secrets with:
#   terraform output -raw s3_access_key
#   terraform output -raw s3_secret_key

# -----------------------------------------------------------------------------
# Compute
# -----------------------------------------------------------------------------

output "instance_ipv4" {
  description = "Public IPv4. Create the archive.thebackroom420.ca A record at ClouDNS pointing here (plan Q2)."
  value = one([
    for a in ovh_cloud_project_instance.stalwart.addresses : a.ip if a.version == 4
  ])
}

output "instance_id" {
  description = "Instance ID"
  value       = ovh_cloud_project_instance.stalwart.id
}

output "ssh_command" {
  description = "SSH into the instance"
  value = format("ssh ubuntu@%s", one([
    for a in ovh_cloud_project_instance.stalwart.addresses : a.ip if a.version == 4
  ]))
}

# -----------------------------------------------------------------------------
# Deployment
# -----------------------------------------------------------------------------
# Read by `deploy email-archiver` (tools/gern-shell/archiver.sh) so the domain
# is written in exactly one place. The deploy checks the name resolves to
# instance_ipv4 before asking certbot to prove control over it -- a failed
# challenge counts against Let's Encrypt's rate limit, so it is worth one DNS
# lookup to avoid.

output "archive_domain" {
  description = "Hostname clients connect to, and the name on the certificate."
  value       = var.archive_domain
}

output "certbot_email" {
  description = "Contact address for Let's Encrypt expiry notices."
  value       = var.certbot_email
}

output "tls_cert_path" {
  description = "Certificate path on the instance, as written into config.toml."
  value       = local.tls_cert_path
}

output "tls_key_path" {
  description = "Private key path on the instance, as written into config.toml."
  value       = local.tls_key_path
}
