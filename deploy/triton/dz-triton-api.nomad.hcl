// dz-triton-api — internal-only Triton agent-ingress gateway. The
// REST adapter is fronted by an oauth2-proxy sidecar that authenticates
// the operator against Vault's `ops` realm (ADR-0011 / same idiom as
// auth-portal-dz). MCP / A2A ports are not currently exposed via the
// shared internal ingress; they register as separate Consul services
// for future service-to-service callers.
//
// exposure = internal (ADR-0010 / substrate-platform reference 15):
// the `intprefix-` tag below makes fabio-internal route
// https://triton-api.<env>.int.data-zoo.de here over the tailnet,
// terminating TLS with the per-env `*.<env>.int.data-zoo.de` wildcard
// cert. NO urlprefix- (deploy gate rejects a mismatch).
//
// Pre-flight: substrate Terraform must have provisioned the
// `ops-triton-api` internal OIDC client (`identity/oidc/client/`,
// `kv/data/internal-sso/clients/ops-triton-api`, jwt-nomad role
// `internal-sso-ops-triton-api`). See
// hetzner-agent-substrate/infra/env/<env>.tfvars `internal_oidc_clients`.
//
// Image lives in GAR (substrate-platform reference 18 — GAR-only):
//   europe-west3-docker.pkg.dev/hetzner-agent-backplane/substrate/dz-triton:<sha>
//
// Deploy (operator-side, from substrate repo):
//   /release-app dz-triton-api nonprod
//   # or, manually:
//   nomad job run -var datacenter=nonprod -var env=nonprod \
//     -var image=europe-west3-docker.pkg.dev/.../dz-triton:<sha> \
//     nomad/dz-triton-api.nomad.hcl

variable "datacenter" {
  type        = string
  description = "Nomad datacenter (env): nonprod | prod."
}

variable "env" {
  type        = string
  description = "Environment name: nonprod | prod (used in the FQDN). Defaults to var.datacenter so /deploy-base, which passes only -var datacenter=<env>, works without a second flag."
  default     = ""
}

variable "version" {
  type        = string
  description = "Build version. Convention: <YYYY-MM-DD>-<git-short-sha>."
  default     = "unset"
}

variable "image" {
  type        = string
  description = "Triton container image, pinned by SHA tag. NEVER :latest. e.g. europe-west3-docker.pkg.dev/hetzner-agent-backplane/substrate/dz-triton:<12-char-sha>"
}

locals {
  env         = var.env != "" ? var.env : var.datacenter
  fqdn        = "triton-api.${local.env}.int.data-zoo.de"
  sso_realm   = "ops"
  sso_client  = "ops-triton-api"
  oidc_issuer = "http://${local.env}-srv-1:8200/v1/identity/oidc/provider/${local.sso_realm}"
  // Cross-origin SPA at the explorer FQDN; CORS allow-list echoes this
  // origin so the browser preflight from triton-explorer.* clears.
  explorer_origin = "https://triton-explorer.${local.env}.int.data-zoo.de"
}

job "dz-triton-api" {
  type        = "service"
  datacenters = [var.datacenter]
  namespace   = "default"

  meta {
    env = local.env
    // exposure = internal (ADR-0010): routed by fabio-internal via the
    // intprefix- tag below; NEVER on the public LB. Enforced by
    // ops/scripts/check-exposure.sh in /deploy-base.
    exposure = "internal"
  }

  // Plain rolling update — internal apps have no public-vs-canary
  // traffic split, so canary blue/green is the wrong shape.
  update {
    auto_revert       = true
    max_parallel      = 1
    min_healthy_time  = "20s"
    healthy_deadline  = "3m"
    progress_deadline = "5m"
  }

  group "api" {
    count = 1

    network {
      mode = "bridge"
      // `http` is the oauth2-proxy SIDECAR's listener (4180) —
      // fabio-internal routes here. Triton listens on 127.0.0.1:8003
      // in the shared netns and is only reachable via the sidecar.
      port "http" { to = 4180 }
      // MCP and A2A ports registered for future service-to-service
      // callers via Consul DNS (`dz-triton-mcp.service.consul`,
      // `dz-triton-a2a.service.consul`). No intprefix- on these so
      // they don't hit fabio-internal; peers reach them direct.
      port "mcp" { to = 8001 }
      port "a2a" { to = 8002 }
    }

    service {
      name     = "dz-triton-api"
      port     = "http"
      provider = "consul"

      tags = ["intprefix-${local.fqdn}"]

      // oauth2-proxy health endpoint (no auth) — same idiom as
      // auth-portal's /ping.
      check {
        type     = "http"
        path     = "/ping"
        interval = "10s"
        timeout  = "2s"
      }
    }

    // MCP + A2A Consul registrations (service-to-service peers reach
    // these via Consul DNS, not via the sidecar; the v0 demo's SPA
    // talks REST only).
    service {
      name     = "dz-triton-mcp"
      port     = "mcp"
      provider = "consul"
    }
    service {
      name     = "dz-triton-a2a"
      port     = "a2a"
      provider = "consul"
    }

    task "triton" {
      driver = "docker"

      config {
        image = var.image
        // No host port: Triton listens on 127.0.0.1:8003 in the
        // shared netns; only the oauth2-proxy sidecar reaches it.
        // MCP / A2A bind 0.0.0.0 so the alloc-level ports forward.
      }

      env {
        TRITON_HOST                  = "0.0.0.0"
        TRITON_REST_PORT             = "8003"
        TRITON_MCP_PORT              = "8001"
        TRITON_A2A_PORT              = "8002"
        TRITON_ENV                   = local.env
        TRITON_CORS_ALLOWED_ORIGINS  = local.explorer_origin
        TRITON_DRAIN_DEADLINE_SECS   = "10"
        RUST_LOG                     = "info"
      }

      resources {
        cpu    = 500
        memory = 256
      }
    }

    task "oauth2-proxy" {
      driver = "docker"

      config {
        image = "quay.io/oauth2-proxy/oauth2-proxy:v7.6.0"
        ports = ["http"]
        // Pinning the args list verbatim against auth-portal-dz keeps
        // the SSO behaviour identical across internal apps. Notable
        // delta from auth-portal: cookie-samesite=none (the SPA hits
        // us cross-origin), and CORS flags so the OPTIONS preflight
        // from triton-explorer.* clears at the sidecar before Triton
        // even sees the request.
        args = [
          "--http-address=0.0.0.0:4180",
          "--upstream=http://127.0.0.1:8003",
          "--reverse-proxy=true",
          "--provider=oidc",
          "--provider-display-name=DataZoo Vault",
          "--oidc-issuer-url=${local.oidc_issuer}",
          "--scope=openid ${local.sso_realm}-profile",
          "--email-domain=*",
          "--skip-provider-button=true",
          "--pass-user-headers=true",
          "--set-xauthrequest=true",
          "--redirect-url=https://${local.fqdn}/oauth2/callback",
          "--cookie-domain=.${local.env}.int.data-zoo.de",
          "--whitelist-domain=.${local.env}.int.data-zoo.de",
          "--cookie-secure=true",
          // SameSite=None required for cross-origin XHR from the SPA
          // to carry the session cookie. Browsers also require
          // Secure (above) on a None cookie.
          "--cookie-samesite=none",
          // The SPA at triton-explorer.<env>.int.data-zoo.de calls us
          // cross-origin. Echo its origin + Allow-Credentials so the
          // preflight clears.
          "--cors-allow-origin=${local.explorer_origin}",
          "--cors-allow-credentials=true",
        ]
      }

      identity {
        name        = "vault"
        aud         = ["vault"]
        file        = true
        change_mode = "noop"
        ttl         = "5m"
      }

      vault {
        role = "internal-sso-${local.sso_client}"
      }

      template {
        destination = "secrets/oauth2.env"
        env         = true
        change_mode = "restart"
        data        = <<-EOH
          {{ with secret "kv/data/internal-sso/clients/${local.sso_client}" -}}
          OAUTH2_PROXY_CLIENT_ID={{ .Data.data.client_id }}
          OAUTH2_PROXY_CLIENT_SECRET={{ .Data.data.client_secret }}
          {{- end }}
          {{ with secret "kv/data/internal-sso/cookie-secret" -}}
          OAUTH2_PROXY_COOKIE_SECRET={{ .Data.data.cookie_secret }}
          {{- end }}
        EOH
      }

      resources {
        cpu    = 100
        memory = 64
      }
    }
  }
}
