// <co>-hello — the adk-rust hello-world agent Triton dispatches into.
//
// Shape: tailnet-only (NO urlprefix- tag — agents are invisible to
// Fabio, G-7/G-S6), discovered by Triton via the Consul tag
// `agent:hello`. Verifies Triton's per-call OIDC bearer.
//
// This template focuses on the TRITON-SPECIFIC bits (the Consul tag, the
// OIDC verifier wiring) plus the one adk-rust-specific need: the
// ANTHROPIC_API_KEY for the LLM brain. For the rest of the substrate job
// shape (update/canary stanza, resource sizing, objstore creds, deploy
// commands) fork substrate-platform/templates/llm-agent.nomad.hcl and
// merge. See references/03 and references/11.

variable "datacenter" {
  type        = string
  description = "Nomad datacenter (env): nonprod | prod."
}

variable "version" {
  type        = string
  description = "Build version. Convention: <YYYY-MM-DD>-<git-short-sha>."
}

variable "image" {
  type        = string
  description = "Container image, pinned by SHA. No :latest."
}

job "<co>-hello" {
  type        = "service"
  datacenters = [var.datacenter]

  group "agent" {
    count = 1

    network {
      mode = "bridge"
      port "http" { to = 8080 }
    }

    service {
      name     = "<co>-hello"
      port     = "http"
      provider = "consul"

      // ── THE DISCOVERY KEY ──────────────────────────────────────
      // Triton resolves `agent:hello` (FR-U-1). This tag is how the
      // tool becomes reachable; it matches the /v1/tools/hello path.
      tags = ["agent:hello"]

      // NO `urlprefix-` tag. Agents are tailnet-only; only Triton is
      // public. Peers resolve this as <co>-hello.service.consul.

      check {
        type     = "http"
        path     = "/healthz"
        interval = "10s"
        timeout  = "2s"
      }
    }

    task "agent" {
      driver = "docker"

      // Vault policy lets the agent (a) verify Triton's OIDC bearer
      // (references/04) and (b) read the Anthropic key for its LLM brain.
      vault {
        policies = ["apps-<co>"]
      }

      template {
        destination = "secrets/oidc.env"
        env         = true
        change_mode = "restart"
        data        = <<-EOT
          AGENT_OIDC_ISSUER={{ key "substrate/oidc/issuer_url" }}
          # EDIT: this agent's expected audience (its substrate identity).
          AGENT_OIDC_AUDIENCE=agents-{{ env "NOMAD_DC" }}
        EOT
      }

      // The adk-rust brain reads ANTHROPIC_API_KEY. Without it the agent
      // falls back to a deterministic static greeter (handy for smoke
      // tests, but you want the real key in prod).
      template {
        destination = "secrets/anthropic.env"
        env         = true
        change_mode = "restart"
        data        = <<-EOT
          ANTHROPIC_API_KEY={{ with secret "kv/data/apps/<co>/hello/<env>" }}{{ .Data.data.anthropic_api_key }}{{ end }}
        EOT
      }

      config {
        image = var.image
        ports = ["http"]
      }

      env {
        APP        = "<co>-hello"
        ENV        = "${var.datacenter}"
        VERSION    = "${var.version}"
        AGENT_PORT = "8080"
      }

      resources {
        cpu    = 500 // MHz
        memory = 256 // MiB
      }
    }
  }
}
