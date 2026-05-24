// dz-triton-explorer — tailnet-only static-content SPA for exploring
// Triton's features. Forked from
// ~/.claude/skills/substrate-platform/templates/web-service.nomad.hcl
// with the public-Fabio bits stripped so the service is reachable
// ONLY via Consul DNS from tailnet peers, mirroring how Triton's
// /metrics listener is locked down.
//
// Deploy (operator-side, from substrate repo):
//   nomad job run -var=version=<sha> -var=image=ghcr.io/datazoo/dz-triton-explorer@sha256:<digest> \
//     deploy/explorer.nomad.hcl
//
// Access from the tailnet:
//   http://dz-triton-explorer.service.consul:8080

variable "datacenter" {
  type        = string
  default     = "nonprod"
  description = "Nomad datacenter: nonprod | prod."
}

variable "version" {
  type        = string
  description = "Deploy version. Surfaced via /version.json to the Dashboard footer."
}

variable "image" {
  type        = string
  description = "Container image, pinned by SHA. NEVER :latest."
  // Example: "ghcr.io/datazoo/dz-triton-explorer@sha256:abc..."
}

variable "count" {
  type        = number
  default     = 1
  description = "Replica count. Internal tool — 1 is enough; canary == count gives blue/green."
}

job "dz-triton-explorer" {
  type        = "service"
  datacenters = [var.datacenter]

  update {
    canary            = var.count
    auto_promote      = false
    auto_revert       = true
    max_parallel      = var.count
    min_healthy_time  = "10s"
    healthy_deadline  = "2m"
    progress_deadline = "5m"
  }

  group "web" {
    count = var.count

    network {
      mode = "bridge"
      port "http" {
        to = 8080
      }
    }

    service {
      name     = "dz-triton-explorer"
      port     = "http"
      provider = "consul"

      // Tailnet-only: NO urlprefix-* tag, so Fabio cannot route
      // public traffic here. Same pattern as Triton's /metrics
      // listener — peers reach the service via Consul DNS:
      //   dz-triton-explorer.service.consul:8080
      tags        = []
      canary_tags = []

      check {
        type     = "http"
        path     = "/healthz"
        interval = "10s"
        timeout  = "2s"
      }
    }

    task "app" {
      driver = "docker"

      config {
        image = var.image
        ports = ["http"]
      }

      // Render /version.json so the Dashboard footer shows the
      // deployed Nomad job version + image SHA. The SPA fetches
      // this at boot.
      template {
        destination = "local/version.json"
        change_mode = "noop"
        data        = <<-EOT
          { "version": "${var.version}", "image": "${var.image}" }
        EOT
      }

      env {
        APP     = "dz-triton-explorer"
        ENV     = "${var.datacenter}"
        VERSION = "${var.version}"
      }

      resources {
        // Nginx + a few hundred KB of Flutter web assets: the
        // substrate template's 200 MHz / 128 MiB defaults are
        // comfortable for an internal tool.
        cpu    = 200
        memory = 128
      }
    }
  }
}
