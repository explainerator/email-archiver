# -----------------------------------------------------------------------------
# OVHcloud API credentials
# -----------------------------------------------------------------------------
# Supply via secrets.tfvars (git-ignored):
#   terraform apply -var-file=secrets.tfvars
# -----------------------------------------------------------------------------

variable "ovh_endpoint" {
  description = "OVHcloud API endpoint (ovh-ca for Canada)"
  type        = string
  default     = "ovh-ca"
}

variable "ovh_application_key" {
  description = "OVHcloud API application key"
  type        = string
  sensitive   = true
}

variable "ovh_application_secret" {
  description = "OVHcloud API application secret"
  type        = string
  sensitive   = true
}

variable "ovh_consumer_key" {
  description = "OVHcloud API consumer key"
  type        = string
  sensitive   = true
}

variable "ovh_project_id" {
  description = "OVHcloud Public Cloud project ID (service_name)"
  type        = string
}

# -----------------------------------------------------------------------------
# Object Storage — blob store for message bodies
# -----------------------------------------------------------------------------
# Standard storage class. NOT Cold Archive (hours of retrieval latency would
# make IMAP unusable) and NOT Infrequent Access (bills per GiB retrieved, so
# every message a client opens would cost money). See plan section 2.1.
# -----------------------------------------------------------------------------

variable "storage_region" {
  description = "OVHcloud region for the blob bucket, uppercase (e.g. BHS). Must match the instance region's datacentre."
  type        = string
  default     = "BHS"
}

variable "users" {
  description = <<-EOT
    Archive users, keyed by a short identifier. The value is the person's primary
    email address, which derives their bucket name:

        ken@twoducks.ca  ->  ken-twoducks-ca

    One bucket and one scoped S3 credential per entry. A user may have several
    SOURCE accounts feeding their bucket — those are archiver config, not
    infrastructure, and do not belong here.

    Removing an entry will fail the plan (prevent_destroy), which is deliberate.
  EOT
  type        = map(string)
  default     = {}

  validation {
    condition = alltrue([
      for address in values(var.users) :
      can(regex("^[^@\\s]+@[^@\\s]+\\.[^@\\s]+$", address))
    ])
    error_message = "Each users value must be a single email address."
  }

  validation {
    condition = alltrue([
      for address in values(var.users) :
      length(lower(replace(replace(address, "@", "-"), ".", "-"))) >= 3 &&
      length(lower(replace(replace(address, "@", "-"), ".", "-"))) <= 63
    ])
    error_message = "Derived bucket names must be 3-63 characters. Check for unusually long addresses."
  }
}

# -----------------------------------------------------------------------------
# Compute
# -----------------------------------------------------------------------------

variable "instance_region" {
  description = "OVHcloud Public Cloud region for the instance and data volume (e.g. BHS5). Same datacentre as storage_region."
  type        = string
  default     = "BHS5"
}

variable "instance_name" {
  description = "Instance name"
  type        = string
  default     = "stalwart-archive"
}

variable "instance_flavor" {
  description = "Flavor name, resolved to an ID via the flavors data source. d2-4 = 2 vCPU / 4 GB, same class as the k8s node pool."
  type        = string
  default     = "d2-4"
}

variable "instance_image_name" {
  description = "Exact OS image name in the target region. Must match verbatim; list candidates with: echo 'data.ovh_cloud_project_images.linux.images[*].name' | terraform console"
  type        = string
  default     = "Ubuntu 24.04"
}

variable "instance_billing_period" {
  description = "hourly or monthly. Monthly is ~50% cheaper; hourly makes rebuilds cost pennies. See plan section 11.2."
  type        = string
  default     = "monthly"

  validation {
    condition     = contains(["hourly", "monthly"], var.instance_billing_period)
    error_message = "instance_billing_period must be \"hourly\" or \"monthly\"."
  }
}

# SSH access — set exactly one of the two.

variable "instance_ssh_key_name" {
  description = "Name of an SSH key that already exists in the OVH project. Mutually exclusive with instance_ssh_public_key."
  type        = string
  default     = null
}

variable "instance_ssh_public_key_path" {
  description = "Path to a local SSH public key file (.pub), used to create a new key in the project. Takes a path rather than key content because .tfvars files cannot call file(). Mutually exclusive with instance_ssh_key_name."
  type        = string
  default     = null
}

# -----------------------------------------------------------------------------
# Data volume
# -----------------------------------------------------------------------------

# -----------------------------------------------------------------------------
# Deployment
# -----------------------------------------------------------------------------

variable "ssh_private_key_path" {
  description = "Path to the SSH private key matching instance_ssh_public_key_path. Used by the deployment provisioners."
  type        = string
}

variable "instance_ssh_user" {
  description = "SSH user on the instance. Ubuntu cloud images use 'ubuntu'."
  type        = string
  default     = "ubuntu"
}
