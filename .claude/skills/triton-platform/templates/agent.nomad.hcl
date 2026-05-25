// <co>-my-tool — an upstream agent Triton dispatches into.
//
// Shape: tailnet-only (NO urlprefix- tag — agents are invisible to
// Fabio, G-7/G-S6), discovered by Triton via the Consul tag
// `agent:<tool>`. Verifies Triton's per-call OIDC bearer.
//
// This template focuses on the TRITON-SPECIFIC bits: the Consul tag
// and the OIDC verifier wiring. For the rest of the substrate job
// shape (update/canary stanza, resource sizing, objstore creds,
// deploy commands) fork substrate-platform/templates/llm-agent.nomad.hcl
// and merge. See references/03 and references/11.

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

job "<co>-my-tool" {
  type        = "service"
  datacenters = [var.datacenter]

  group "agent" {
    count = 1

    network {
      mode = "bridge"
      port "http" { to = 8080 }
    }

    service {
      // EDIT: name your service.
      name     = "<co>-my-tool"
      port     = "http"
      provider = "consul"

      // ── THE DISCOVERY KEY ──────────────────────────────────────
      // Triton resolves `agent:<tool_name>` (FR-U-1). This tag is how
      // your tool becomes reachable. EDIT to match your tool name and
      // the /v1/tools/<name> path the frontend calls.
      tags = ["agent:my-tool"]

      // NO `urlprefix-` tag. Agents are tailnet-only; only Triton is
      // public. Peers resolve you as <co>-my-tool.service.consul.

      check {
        type     = "http"
        path     = "/healthz"
        interval = "10s"
        timeout  = "2s"
      }
    }

    task "agent" {
      driver = "docker"

      // Vault policy lets the agent verify Triton's OIDC bearer
      // (references/04). The verifier needs the substrate issuer URL,
      // published in Consul KV by the operator.
      vault {
        policies = ["apps-<co>"]
      }

      template {
        destination = "secrets/oidc.env"
        env         = true
        change_mode = "restart"
        data        = <<-EOT
          AGENT_OIDC_ISSUER={{ key "substrate/oidc/issuer_url" }}
          # EDIT: your agent's expected audience (its substrate identity).
          AGENT_OIDC_AUDIENCE=agents-{{ env "NOMAD_DC" }}
        EOT
      }

      config {
        image = var.image
        ports = ["http"]
      }

      env {
        APP       = "<co>-my-tool"
        ENV       = "${var.datacenter}"
        VERSION   = "${var.version}"
        AGENT_PORT = "8080"
      }

      resources {
        cpu    = 500 // MHz — tune to your tool's workload.
        memory = 256 // MiB
      }
    }
  }
}
