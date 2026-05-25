// dz-triton — the Triton agent-ingress gateway, DEMO deployment.
//
// Tailnet-only (NO Fabio `urlprefix-` tag): the gateway is reachable
// only via Consul DNS from tailnet peers, the same lockdown the
// explorer and Triton's /metrics listener use. The DEMO image ships
// with the `dev-token` feature ON, so this MUST stay tailnet-only and
// nonprod — never give it a public route, never run in prod.
//
// Forked from apps/explorer/deploy/explorer.nomad.hcl +
// ~/.claude/skills/substrate-platform/templates/web-service.nomad.hcl
// with the public-Fabio bits stripped.
//
// Deploy (operator-side, from the substrate repo):
//   nomad job run -var=version=<sha> \
//     -var=image=ghcr.io/datazoo/dz-triton@sha256:<digest> \
//     deploy/gateway/triton.nomad.hcl
//
// Reach it from the tailnet:
//   REST: http://dz-triton.service.consul:8003   (/healthz, /version, /v1/*)
//   MCP : http://dz-triton.service.consul:8001
//   A2A : http://dz-triton.service.consul:8002

variable "datacenter" {
  type        = string
  default     = "nonprod"
  description = "Nomad datacenter: nonprod | prod. DEMO image is nonprod-only."
}

variable "version" {
  type        = string
  description = "Deploy version. Surfaced via /version (image_sha)."
}

variable "image" {
  type        = string
  description = "Container image, pinned by SHA. NEVER :latest."
  // Example: "ghcr.io/datazoo/dz-triton@sha256:abc..."
}

variable "count" {
  type        = number
  default     = 1
  description = "Replica count. Demo: 1. canary == count gives blue/green."
}

job "dz-triton" {
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

  group "gateway" {
    count = var.count

    network {
      mode = "bridge"
      port "mcp" { to = 8001 }
      port "a2a" { to = 8002 }
      port "rest" { to = 8003 }
    }

    service {
      name     = "dz-triton"
      port     = "rest"
      provider = "consul"

      // Tailnet-only: NO urlprefix-* tag (Fabio cannot route public
      // traffic here). Peers reach it via dz-triton.service.consul.
      tags        = []
      canary_tags = []

      check {
        type     = "http"
        path     = "/healthz"
        interval = "10s"
        timeout  = "2s"
      }
    }

    // The MCP + A2A ports are addressable on the same alloc IP; we
    // register only the REST port for the Consul health check. MCP/A2A
    // peers dial dz-triton.service.consul on 8001/8002 (same host).

    task "gateway" {
      driver = "docker"

      // apps-dz policy grants read on kv/data/apps/dz/* — Triton's
      // VaultKvResolver reads the Telegram credentials referenced by
      // /etc/triton/adapter.yaml, and the upstream router mints
      // per-call agent tokens via the agent-oidc-swap role.
      vault {
        policies = ["apps-dz"]
      }

      config {
        image = var.image
        ports = ["mcp", "a2a", "rest"]
      }

      // Hand Triton the Nomad-issued Vault token. Triton's
      // VaultClient takes a token string (TRITON_VAULT_TOKEN); the
      // `vault{}` stanza above provisions one for the alloc and Nomad
      // exposes it for templating. CONFIRM the exact token path with
      // the operator (it may be ${VAULT_TOKEN} or a secrets file).
      template {
        destination = "secrets/vault.env"
        env         = true
        change_mode = "restart"
        data        = <<-EOT
          TRITON_VAULT_TOKEN={{ env "VAULT_TOKEN" }}
        EOT
      }

      env {
        APP = "dz-triton"
        ENV = "${var.datacenter}"

        TRITON_ENV       = "${var.datacenter}"
        TRITON_HOST      = "0.0.0.0"
        TRITON_MCP_PORT  = "8001"
        TRITON_A2A_PORT  = "8002"
        TRITON_REST_PORT = "8003"
        TRITON_IMAGE_SHA = "${var.version}"

        // Tailnet-only: metrics off, no chat webhook listener
        // (Telegram uses long_poll), no OIDC issuer (dev-token).
        TRITON_METRICS_PORT      = "0"
        TRITON_CHAT_WEBHOOK_PORT = "0"

        // Chat + upstream wiring.
        TRITON_MANIFEST_PATH    = "/etc/triton/adapter.yaml"
        TRITON_TELEGRAM_API_BASE = "https://api.telegram.org"
        TRITON_CONSUL_URL       = "http://consul.service.consul:8500"
        TRITON_VAULT_URL        = "http://vault.service.consul:8200"
        TRITON_VAULT_OIDC_ROLE  = "agent-oidc-swap"

        // Let the tailnet explorer (separate job) call the REST trio
        // cross-origin.
        TRITON_CORS_ALLOWED_ORIGINS = "http://dz-triton-explorer.service.consul:8080"
      }

      resources {
        cpu    = 500  // MHz
        memory = 256  // MiB
      }
    }
  }
}
