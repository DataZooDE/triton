// dz-triton-explorer — INTERNAL Flutter SPA for poking at every Triton feature
// (REST/MCP/A2A trio, /v1/tools, /v1/manifest, A2UI v0.8/v0.9 builders).
//
// exposure = internal (ADR-0010 / substrate-platform reference 15). A normal
// Consul service with an `intprefix-` route tag: the shared `fabio-internal`
// ingress (tailscale0:443) terminates TLS with the per-env wildcard cert
// `*.<env>.int.data-zoo.de` and proxies to this nginx alloc on :8080. NO
// per-app Tailscale sidecar, NO public Fabio route, NO operator auth. Tailnet
// peers (operators on tag:ops) reach the SPA at
// https://explorer.<env>.int.data-zoo.de.
//
// Browser-served session cookies need HTTPS; plain HTTP via Consul DNS
// silently drops `Secure` cookies, so an internal SPA MUST be fronted by
// fabio-internal — not addressed via `<svc>.service.consul:8080`.
//
// Image lives in GAR (substrate-platform reference 18 — GAR-only registry):
//   europe-west3-docker.pkg.dev/hetzner-agent-backplane/substrate/dz-triton-explorer
// The reusable substrate `build-app-image.yml` workflow lives in the
// hetzner-agent-substrate repo; the triton repo currently builds + pushes
// manually (see apps/explorer/deploy/README.md). The substrate's
// `ops/base-jobs.manifest` pins the SHA tag.
//
// Deploy (operator-side, from substrate repo):
//   /release-app dz-triton-explorer nonprod
//   # or, manually:
//   nomad job run \
//     -var datacenter=nonprod \
//     -var env=nonprod \
//     -var version=2026-05-28-<sha> \
//     -var image=europe-west3-docker.pkg.dev/hetzner-agent-backplane/substrate/dz-triton-explorer:<sha> \
//     nomad/dz-triton-explorer.nomad.hcl

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
  env  = var.env != "" ? var.env : var.datacenter
  fqdn = "explorer.${local.env}.int.data-zoo.de"
}

job "dz-triton-explorer" {
  type        = "service"
  datacenters = [var.datacenter]

  meta {
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
      port "http" {
        to = 8080
      }
    }

    service {
      name     = "dz-triton-explorer"
      port     = "http"
      provider = "consul"

      // intprefix- → fabio-internal at https://explorer.<env>.int.data-zoo.de.
      // No urlprefix- (that would be the PUBLIC Fabio); deploy gate rejects
      // a mismatch with meta.exposure.
      tags = ["intprefix-${local.fqdn}"]

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

      // /version.json rendered by Nomad's template stanza so the SPA's
      // Dashboard footer can show the deployed Nomad job version + image
      // SHA. Mounted from `local/` into the container's nginx docroot.
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
        // nginx + a few hundred KB of Flutter web assets — the substrate's
        // 200 MHz / 128 MiB defaults are comfortable for an internal tool.
        cpu    = 200
        memory = 128
      }
    }
  }
}
