// dz-triton-demo-agent — demo upstream agent for the Triton demo.
//
// Triton discovers it via the Consul tag `agent:demo-stats` and
// dispatches the `demo-stats` tool to it (over the tailnet, with a
// per-call Vault-minted OIDC bearer). Tailnet-only: NO Fabio tag.
//
// Deploy (operator-side, from the substrate repo):
//   nomad job run -var=version=<sha> \
//     -var=image=ghcr.io/datazoo/dz-triton-demo-agent@sha256:<digest> \
//     apps/demo-agent/deploy/demo-agent.nomad.hcl

variable "datacenter" {
  type        = string
  default     = "nonprod"
  description = "Nomad datacenter: nonprod | prod."
}

variable "version" {
  type        = string
  description = "Deploy version."
}

variable "image" {
  type        = string
  description = "Container image, pinned by SHA. NEVER :latest."
  // Example: "ghcr.io/datazoo/dz-triton-demo-agent@sha256:abc..."
}

variable "count" {
  type        = number
  default     = 1
  description = "Replica count. Demo: 1."
}

job "dz-triton-demo-agent" {
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

  group "agent" {
    count = var.count

    network {
      mode = "bridge"
      port "http" { to = 8080 }
    }

    service {
      name     = "dz-triton-demo-agent"
      port     = "http"
      provider = "consul"

      // THE DISCOVERY KEY: Triton's upstream router resolves
      // `agent:<tool>` in Consul. No urlprefix- tag → tailnet-only.
      tags        = ["agent:demo-stats"]
      canary_tags = ["agent:demo-stats"]

      check {
        type     = "http"
        path     = "/healthz"
        interval = "10s"
        timeout  = "2s"
      }
    }

    task "agent" {
      driver = "docker"

      // For OIDC discovery (the substrate issuer URL lives in Consul
      // KV). Full token verification is a follow-up; the demo agent
      // accepts + logs the bearer for now.
      vault {
        policies = ["apps-dz"]
      }

      template {
        destination = "local/oidc.env"
        env         = true
        change_mode = "restart"
        data        = <<-EOT
          AGENT_OIDC_ISSUER={{ key "substrate/oidc/issuer_url" }}
        EOT
      }

      config {
        image = var.image
        ports = ["http"]
      }

      env {
        APP        = "dz-triton-demo-agent"
        ENV        = "${var.datacenter}"
        AGENT_PORT = "8080"
      }

      resources {
        cpu    = 200  // MHz
        memory = 128  // MiB
      }
    }
  }
}
