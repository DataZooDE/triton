# Deploying dz-triton to the Hetzner substrate

Triton ships as a single static binary in a container image and runs on
the DataZoo **Kamal** substrate (Hetzner cattle hosts + a GCP backplane).
The Nomad/Consul/Vault stack this repo originally targeted has been
**decommissioned** — see the `substrate-platform` skill for the current
contract. The actual deploy config (`kamal/<app>/deploy.yml`,
`apps/registry.yml`) lives in the **substrate repo**, not here; this
directory holds only the image build inputs.

## Images (built here, deployed from the substrate repo)

| Image | Built from | What |
|---|---|---|
| `ghcr.io/datazoode/dz-triton` | `deploy/triton/Dockerfile` | the gateway binary + the baked `env://` chat manifest at `/etc/triton/adapter.yaml` |
| `ghcr.io/datazoode/dz-triton-gateway` | `deploy/triton-gateway/Dockerfile` | the same image re-stamped with `LABEL service="dz-triton-gateway"` so Kamal accepts it for the public WhatsApp/Telegram ingress |

The manifest is **opt-in**: a pure-REST deploy leaves `TRITON_MANIFEST_PATH`
unset and ignores `/etc/triton/adapter.yaml`. The gateway deploy sets
`TRITON_MANIFEST_PATH=/etc/triton/adapter.yaml` to enable the chat adapters.

### `TRITON_OPTIONAL_ADAPTERS` — skip an adapter whose secret this image lacks

The same baked `adapter.yaml` runs in two images. `dz-triton` (the internal
upstream-dispatcher, which needs only the WhatsApp adapter for the outbound
courier) does **not** carry the Telegram secret — and must not, or it could
hijack the gateway's Telegram webhook. So it sets
`TRITON_OPTIONAL_ADAPTERS=telegram` (comma-separated; also `--optional-adapters`,
case-insensitive). When a listed adapter fails to build **specifically because
a declared `env://` credential is unset**, Triton logs a `warn!` (naming the
adapter + the missing var) and skips it, booting the rest.

The opt-in is narrow and fail-safe:

- It fires **only** for a missing `env://` secret. Any other build failure
  (malformed manifest, bad value, a non-env missing credential, a `vault://`
  ref) stays fatal even for a listed adapter.
- An adapter **not** in the set stays fatal on every failure.
- Default (unset/empty) ⇒ today's behaviour: any adapter build error aborts
  boot. The public gateway sets nothing here, so if its Telegram secret ever
  goes missing it still fails loudly rather than silently dropping ingress.

## Secrets — `env://`, from GCP Secret Manager (no Vault)

Every credential in `deploy/triton/adapter.yaml` is an `env://VARNAME`
reference (triton #120). The substrate injects the values as container
env from **GCP Secret Manager** via kamal `.kamal/secrets`. Vault is gone;
a `vault://` ref now fails boot closed. Literals are refused outside
`local` env (M-SECRETS-1). Seed in Secret Manager (names per the
manifest comments), e.g. `triton-whatsapp-app-secret`,
`triton-telegram-bot-token`, `triton-*-correlation-key`, …

## Upstream agents — `TRITON_STATIC_UPSTREAMS` (no Consul)

Triton routes a tool name to a fixed `host:port` from the static map:

```
TRITON_STATIC_UPSTREAMS=carl=carl.<tailnet>.ts.net:8001,resolve_identity=carl.<tailnet>.ts.net:8001
```

There is no service discovery — the map is the only mechanism. Per-call
workload→workload auth is a short-TTL **RS256 JWT** that Triton mints and
agents verify against Triton's own JWKS (`/.well-known/jwks.json`), so no
Vault token-swap is involved. Configure the signer with
`TRITON_JWT_SIGNING_KEY` (PEM or base64-PEM), `TRITON_SELF_ISSUER`,
`TRITON_JWT_JWKS`, and `TRITON_JWT_KID`; the signing key comes from GCP
Secret Manager. Without a signer, dispatch falls back to a static
`TRITON_STATIC_UPSTREAM_TOKEN` bearer (dev only).

## Exposure

The gateway is the **public** WhatsApp/Telegram ingress (registry
exposure `external` in prod) so Meta/Telegram can reach the inbound
webhooks. Upstream agents stay tailnet-only and are reached by their
`*.ts.net` names in `TRITON_STATIC_UPSTREAMS`.

## Build (pin by SHA, never `:latest`)

```sh
docker build -f deploy/triton/Dockerfile -t ghcr.io/datazoode/dz-triton:$VER .
docker build -f deploy/triton-gateway/Dockerfile -t ghcr.io/datazoode/dz-triton-gateway:$VER .
docker push ghcr.io/datazoode/dz-triton:$VER   # … and dz-triton-gateway
```

`$VER` convention: `<YYYY-MM-DD>-<git-short-sha>`. Hand the pushed
`@sha256:…` digest to the operator, who updates the image ref in the
substrate repo's `kamal/dz-triton-gateway/deploy.yml` and runs the
substrate `/apply` flow.

## Local end-to-end

`deploy/local-e2e/` carries dev harnesses (`mcp-smoke.sh`,
`explorer-rodney.sh`, …) that boot a local Triton with `dev-token` and a
local agent over `TRITON_STATIC_UPSTREAMS` — no substrate access needed.
