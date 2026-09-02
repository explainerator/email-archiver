# -----------------------------------------------------------------------------
# Stalwart read-only email archive — OVHcloud provider
# -----------------------------------------------------------------------------
# See ../STALWART-ARCHIVE-PLAN.md for the design and the decision record.
# -----------------------------------------------------------------------------

terraform {
  required_providers {
    ovh = {
      source  = "ovh/ovh"
      version = "~> 2.0"
    }
  }
}

provider "ovh" {
  endpoint           = var.ovh_endpoint
  application_key    = var.ovh_application_key
  application_secret = var.ovh_application_secret
  consumer_key       = var.ovh_consumer_key
}
