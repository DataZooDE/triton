# Deploy

Substrate deployment artefacts for `dz-triton-explorer`. **Tailnet-only.**

## Files

- `Dockerfile` — multi-stage: Flutter web build + `nginx-unprivileged`.
- `nginx.conf` — SPA fallback + `/healthz` + `/version.json`.
- `explorer.nomad.hcl` — Nomad service jobspec with `tags = []`
  (no `urlprefix-`, so Fabio cannot route public traffic here).

## Access pattern

The container listens on `:8080` inside the Nomad alloc. Consul
registers the service as `dz-triton-explorer` with no Fabio tags;
tailnet peers (operators on `tag:ops`) reach it via Consul DNS:

```
http://dz-triton-explorer.service.consul:8080
```

This mirrors how Triton's `/metrics` listener stays locked to the
tailnet — same idiom, different content.

## CORS

The SPA in this container talks to Triton's HTTP trio cross-origin.
Triton must be running with its origin in the allow-list so the
browser preflight passes:

```
TRITON_CORS_ALLOWED_ORIGINS=http://dz-triton-explorer.service.consul:8080
```

For nonprod operators on the tailnet, the same env can include the
local dev origins:

```
TRITON_CORS_ALLOWED_ORIGINS=http://dz-triton-explorer.service.consul:8080,http://localhost:5000
```

See PR #16 for the CORS layer; PR #18 for the `/v1/runtime`
discovery endpoint that bootstraps OIDC PKCE.

## Building the image

The substrate CI builds + pushes the image. For local smoke:

```bash
# From the repo root:
docker build -f apps/explorer/deploy/Dockerfile -t dz-triton-explorer:dev .
docker run --rm -p 8080:8080 dz-triton-explorer:dev
# Browse http://localhost:8080
```

The Dockerfile copies only `apps/explorer/{lib,web,pubspec.*,
analysis_options.yaml}` into the build stage, so changes outside
the explorer don't bust the layer cache.

## Deploy

From the substrate repo (operator-side):

```bash
nomad job run \
  -var=datacenter=nonprod \
  -var=version=2026-05-24-abc1234 \
  -var=image=ghcr.io/datazoo/dz-triton-explorer@sha256:... \
  deploy/explorer.nomad.hcl
```

`auto_promote = false` + `canary = count` gives blue/green; manually
`/promote` once the canaries pass `/healthz`.
