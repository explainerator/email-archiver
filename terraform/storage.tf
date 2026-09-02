# -----------------------------------------------------------------------------
# Blob storage — one S3 bucket per user
# -----------------------------------------------------------------------------
# Each user gets their own bucket AND their own credential scoped to it. Not a
# shared bucket with per-user prefixes: a separate bucket is a structural
# boundary rather than one enforced by policy strings, and with 4-5 users the
# overhead is negligible.
#
# The archiver must load each user's credential separately. If it held one
# credential able to read every bucket, this separation would only protect
# against credential leak, not against a bug in our own code. See
# ARCHIVE-PLAN.md section 2.4.
#
# NEVER mount these with s3fs, goofys, rclone mount, or any FUSE layer. FUSE
# presents POSIX semantics S3 cannot honour and fails as silent corruption.
# -----------------------------------------------------------------------------

locals {
  # ken@twoducks.ca -> ken-twoducks-ca
  #
  # S3 allows only lowercase alphanumerics, dots and hyphens. Dots are legal but
  # break virtual-hosted-style HTTPS (the wildcard cert does not match a
  # multi-level subdomain), which would force path-style addressing on every
  # tool that touches these buckets. Hyphens avoid that entirely.
  user_buckets = {
    for key, address in var.users :
    key => lower(replace(replace(address, "@", "-"), ".", "-"))
  }
}

resource "ovh_cloud_project_storage" "user" {
  for_each = local.user_buckets

  service_name = var.ovh_project_id
  region_name  = var.storage_region
  name         = each.value

  # Versioning is the only deletion backstop — Object Lock was deliberately not
  # used, since compliance-mode retention makes a bad import permanent and
  # billed forever. Read-only is enforced structurally by the archiver, which
  # has no delete path at all.
  versioning = {
    status = "enabled"
  }

  # SSE-S3 at rest. OVH manages the keys; transparent to the client. Not a
  # defence against a leaked credential — that still reads plaintext through
  # the API — but it covers the disks.
  encryption = {
    sse_algorithm = "AES256"
  }

  lifecycle {
    # Each bucket is the only copy of one person's mail. Removing a user from
    # var.users will fail the plan rather than silently deleting their archive;
    # lifting this block is a deliberate, reviewable step.
    prevent_destroy = true
  }
}

# -----------------------------------------------------------------------------
# Per-user S3 identities
# -----------------------------------------------------------------------------
# NOTE ON STATE: ovh_cloud_project_user_s3_credential GENERATES the secret key,
# so every user's key is stored in terraform.tfstate in plaintext. Marking
# outputs sensitive only suppresses console display.
#
# Mitigations: .gitignore excludes *.tfstate; each credential is scoped by
# policy to exactly one bucket; rotation is
#   terraform taint 'ovh_cloud_project_user_s3_credential.user["ken"]'
#   terraform apply
# -----------------------------------------------------------------------------

resource "ovh_cloud_project_user" "user" {
  for_each = local.user_buckets

  service_name = var.ovh_project_id
  description  = "email-archiver — blob store access for ${each.value}"
  role_name    = "objectstore_operator"
}

resource "ovh_cloud_project_user_s3_credential" "user" {
  for_each = local.user_buckets

  service_name = var.ovh_project_id
  user_id      = ovh_cloud_project_user.user[each.key].id
}

# Each identity can touch exactly one bucket. This is the enforcement that makes
# the per-user split meaningful.
resource "ovh_cloud_project_user_s3_policy" "user" {
  for_each = local.user_buckets

  service_name = var.ovh_project_id
  user_id      = ovh_cloud_project_user.user[each.key].id

  policy = jsonencode({
    Statement = [
      {
        Sid    = "ArchiveBucketAccess"
        Effect = "Allow"
        Action = ["s3:*"]
        Resource = [
          "arn:aws:s3:::${each.value}",
          "arn:aws:s3:::${each.value}/*"
        ]
      }
    ]
  })
}
