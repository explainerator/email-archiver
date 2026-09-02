# -----------------------------------------------------------------------------
# Compute — OVHcloud Public Cloud instance
# -----------------------------------------------------------------------------
# Same resource class as the k8s node pool (d2-4). Monthly billing.
#
# Deliberately NO prevent_destroy: the instance is meant to be rebuildable.
# The volume in volume.tf carries prevent_destroy instead, because that is
# where the RocksDB metadata and FTS index live. See plan section 0.1.
# -----------------------------------------------------------------------------

# Flavor lookup by name. Avoids hardcoding a UUID and fails loudly if d2-4
# stops being offered in this region.
data "ovh_cloud_project_flavors" "available" {
  service_name = var.ovh_project_id
  region       = var.instance_region
  name_filter  = var.instance_flavor
}

# Image lookup. This data source has no name filter, so the match is done in
# HCL below.
data "ovh_cloud_project_images" "linux" {
  service_name = var.ovh_project_id
  region       = var.instance_region
  os_type      = "linux"
}

locals {
  matching_flavors = [
    for f in data.ovh_cloud_project_flavors.available.flavors :
    f if f.name == var.instance_flavor
  ]

  matching_images = [
    for i in data.ovh_cloud_project_images.linux.images :
    i if i.name == var.instance_image_name
  ]
}

resource "ovh_cloud_project_instance" "stalwart" {
  service_name   = var.ovh_project_id
  region         = var.instance_region
  name           = var.instance_name
  billing_period = var.instance_billing_period

  boot_from {
    image_id = local.matching_images[0].id
  }

  flavor {
    flavor_id = local.matching_flavors[0].id
  }

  network {
    public = true
  }

  # Existing key by name, or create one from a supplied public key.
  # Exactly one of instance_ssh_key_name / instance_ssh_public_key must be set.
  dynamic "ssh_key" {
    for_each = var.instance_ssh_key_name != null ? [1] : []
    content {
      name = var.instance_ssh_key_name
    }
  }

  dynamic "ssh_key_create" {
    for_each = var.instance_ssh_public_key_path != null ? [1] : []
    content {
      name       = "${var.instance_name}-key"
      public_key = trimspace(file(pathexpand(var.instance_ssh_public_key_path)))
    }
  }

  # cloud-init replaces the SSH provisioner chain used by the old Bichon
  # stack. Declarative, runs on first boot, and re-applies identically when
  # the instance is rebuilt — which is what makes the recovery path in the
  # plan (section 8.2) work without manual steps.
  user_data = file("${path.module}/files/cloud-init.yaml")

  lifecycle {
    precondition {
      condition     = length(local.matching_flavors) > 0
      error_message = "Flavor '${var.instance_flavor}' not found in region ${var.instance_region}. List them with: echo 'data.ovh_cloud_project_flavors.available.flavors[*].name' | terraform console"
    }

    precondition {
      condition     = length(local.matching_images) > 0
      error_message = "Image '${var.instance_image_name}' not found in region ${var.instance_region}. List available names with: echo 'data.ovh_cloud_project_images.linux.images[*].name' | terraform console"
    }

    precondition {
      condition     = (var.instance_ssh_key_name != null) != (var.instance_ssh_public_key_path != null)
      error_message = "Set exactly one of instance_ssh_key_name or instance_ssh_public_key_path."
    }
  }

  timeouts {
    create = "20m"
  }
}
