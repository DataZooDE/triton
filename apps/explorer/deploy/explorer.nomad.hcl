// dz-triton-explorer — INTERNAL Flutter SPA for poking at every Triton feature
// (REST/MCP/A2A trio, /v1/tools, /v1/manifest, A2UI v0.8/v0.9 builders).
//
// exposure = internal (ADR-0010 / substrate-platform reference 15). A normal
// Consul service fronted by the shared `fabio-internal` ingress
// (tailscale0:443) which terminates TLS with the per-env wildcard cert
// `*.<env>.int.data-zoo.de`. NO per-app Tailscale sidecar, NO public Fabio
// route. Tailnet peers (operators on tag:ops) reach the SPA at
// https://triton-explorer.<env>.int.data-zoo.de (canonical) — the legacy
// `intprefix-explorer.*` tag stays for one cycle so existing bookmarks
// don't break before consumers migrate.
//
// Browser-served session cookies need HTTPS; plain HTTP via Consul DNS
// silently drops `Secure` cookies, so an internal SPA MUST be fronted by
// fabio-internal — not addressed via `<svc>.service.consul:8080`.
//
// Operator-SSO via oauth2-proxy sidecar (ADR-0011 / same idiom as
// auth-portal-dz). The sidecar authenticates the operator against Vault's
// `ops` realm; the SPA itself is unchanged. A login here also covers the
// paired triton-api.<env>.int.data-zoo.de (shared cookie domain).
//
// Pre-flight: substrate Terraform must have provisioned the
// `ops-triton-explorer` internal OIDC client (`identity/oidc/client/`,
// `kv/data/internal-sso/clients/ops-triton-explorer`, jwt-nomad role
// `internal-sso-ops-triton-explorer`). See
// hetzner-agent-substrate/infra/env/<env>.tfvars `internal_oidc_clients`.
//
// Image lives in GAR (substrate-platform reference 18 — GAR-only registry):
//   europe-west3-docker.pkg.dev/hetzner-agent-backplane/substrate/dz-triton-explorer
//
// Deploy (operator-side, from substrate repo):
//   /release-app dz-triton-explorer nonprod

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
  description = "Build version. Convention: <YYYY-MM-DD>-<git-short-sha>. Surfaced via /version.json to the Dashboard footer."
  default     = "unset"
}

variable "image" {
  type        = string
  description = "Container image, pinned by SHA tag. NEVER :latest. e.g. europe-west3-docker.pkg.dev/hetzner-agent-backplane/substrate/dz-triton-explorer:<12-char-sha>"
}

locals {
  env         = var.env != "" ? var.env : var.datacenter
  fqdn        = "triton-explorer.${local.env}.int.data-zoo.de"
  legacy_fqdn = "explorer.${local.env}.int.data-zoo.de"
  sso_realm   = "ops"
  sso_client  = "ops-triton-explorer"
  oidc_issuer = "http://${local.env}-srv-1:8200/v1/identity/oidc/provider/${local.sso_realm}"
}

job "dz-triton-explorer" {
  type        = "service"
  datacenters = [var.datacenter]
  namespace   = "default"

  meta {
    env = local.env
    // exposure=internal (ADR-0010): routed by fabio-internal via the
    // intprefix- tag below; never on the public LB. Enforced by
    // ops/scripts/check-exposure.sh inside /deploy-base.
    exposure = "internal"
  }

  // Plain rolling update: internal apps have no public-vs-canary traffic
  // split, so canary blue/green is the wrong shape. auth-portal uses the
  // same pattern.
  update {
    auto_revert       = true
    max_parallel      = 1
    min_healthy_time  = "20s"
    healthy_deadline  = "2m"
    progress_deadline = "5m"
  }

  group "web" {
    count = 1

    network {
      mode = "bridge"
      // The exposed `http` port is the oauth2-proxy SIDECAR (4180);
      // fabio-internal routes here. nginx (the SPA) listens on
      // 127.0.0.1:8080 in the shared netns, only reachable via the
      // sidecar.
      port "http" {
        to = 4180
      }
    }

    service {
      name     = "dz-triton-explorer"
      port     = "http"
      provider = "consul"

      // Canonical name + legacy alias for one migration cycle. Both
      // intprefix- tags route to the same oauth2-proxy. No urlprefix-
      // (that would be the PUBLIC Fabio); deploy gate rejects a
      // mismatch with meta.exposure.
      tags = [
        "intprefix-${local.fqdn}",
        "intprefix-${local.legacy_fqdn}",
      ]

      // oauth2-proxy's health endpoint (no auth). Same idiom as
      // auth-portal's /ping.
      check {
        type     = "http"
        path     = "/ping"
        interval = "10s"
        timeout  = "2s"
      }
    }

    task "app" {
      driver = "docker"

      config {
        image = var.image
        // No host port: nginx listens on 127.0.0.1:8080 in the shared
        // netns; only the oauth2-proxy sidecar reaches it.
      }

      // /version.json rendered by Nomad's template stanza so the SPA's
      // Dashboard footer can show the deployed Nomad job version + image
      // SHA. Mounted from `local/` into the container via the alias in
      // nginx.conf.
      template {
        destination = "local/version.json"
        change_mode = "noop"
        data        = <<-EOT
          { "version": "${var.version}", "image": "${var.image}" }
        EOT
      }

      env {
        APP        = "dz-triton-explorer"
        ENV        = local.env
        VERSION    = var.version
        PUBLIC_URL = "https://${local.fqdn}"
      }

      resources {
        cpu    = 200
        memory = 128
      }
    }

    task "oauth2-proxy" {
      driver = "docker"

      config {
        image = "quay.io/oauth2-proxy/oauth2-proxy:v7.6.0"
        ports = ["http"]
        // Mirrors auth-portal-dz's sidecar verbatim except: cookie-samesite=none
        // so cross-origin XHR from the SPA to triton-api.* carries the
        // session cookie; CORS flags so OPTIONS preflights from
        // triton-api.* clear at the sidecar.
        args = [
          "--http-address=0.0.0.0:4180",
          "--upstream=http://127.0.0.1:8080",
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
          "--cookie-samesite=none",
          "--cors-allow-origin=https://triton-api.${local.env}.int.data-zoo.de",
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
