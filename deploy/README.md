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

### Rotating a correlation key (triton #287)

`triton-*-correlation-key` accepts a **comma-separated list**. Tokens are
signed with the FIRST key and verified against every key on the list, so a
rotation never breaks the buttons already sitting in people's
conversations:

```
# 1. Prepend the new key. Both values live in the one secret.
triton-telegram-correlation-key = "<new>,<old>"
# 2. Deploy. New buttons are signed with <new>; old ones still verify.
# 3. Wait out the longest token TTL (cards: 24h; leave a day's margin),
#    then drop <old> and deploy again.
triton-telegram-correlation-key = "<new>"
```

Step 3 is what actually revokes the old key — until it runs, the old key
is still accepted. A ring left at two keys is an unfinished rotation.

Whitespace around each key is trimmed, so `<new>, <old>` is fine. A list
from which no key survives (`""`, `" , "`) refuses boot rather than
starting an adapter that can verify nothing.

### Revoking a principal (triton #287)

`TRITON_DENIED_PRINCIPALS` is a comma-separated list of `tenant/sub`
entries. A listed principal's dispatches are refused 403 and audited
`error:forbidden`, on every protocol at once:

```
TRITON_DENIED_PRINCIPALS=acme/alice,globex/bob
```

This is the only lever that revokes a principal FASTER than its token
expires — everything else is boot-time-only, and rotating a signing key
takes out everyone. It takes effect on the next deploy.

Entries must carry a tenant, with exactly one `/`. A bare `alice` is
ignored with a warning rather than applied to every tenant, and
`acme/al/ice` is ignored as ambiguous — it could mean tenant `acme/al`
or sub `al/ice`, and guessing can revoke a different principal across a
customer boundary. Check the boot log: an active denylist warns with the
entries it ACCEPTED and the count, which is what tells you a typo was
dropped.

### `TRITON_AUDIT_OPERATORS` — who may read across tenants

**This is a behaviour change on upgrade.** `/v1/audit` and `/v1/trace`
are now tenant-scoped, and the cross-tenant view needs BOTH the
`audit:read-all` scope AND membership of this list:

```
TRITON_AUDIT_OPERATORS=ops/alice@example.com,ops/bob@example.com
```

Outside `local`, leaving it unset means **nobody** holds that view — the
scope alone no longer grants it, because that claim namespace belongs to
the issuer rather than to this deployment. An issuer where a client can
request a scope, or an admin can add one, could otherwise mint a caller
the whole audit trail.

The symptom of forgetting it is an operator seeing only their own rows,
which does not name its cause; the pod warns at boot instead. In `local`
the scope alone still suffices, so a dev loop needs no extra config.

### `TRITON_PAIRING_TOOLS` — what an un-enrolled sender may reach

Comma-separated tool names. A principal holding the `pairing` scope may
invoke only these. Empty (the default) restricts nothing, which keeps the
gate default-allow: Triton authenticates and propagates, leaving
authorization to the resource owner.

The standalone binary also derives this from the manifest's
`identity.pairing_tool`, which is richer — it knows which adapter named
the tool — and the manifest REPLACES this variable rather than merging
with it. A restriction is an allow-set, so merging could only widen it,
and a stale entry here would silently keep another adapter's enrolment
tool reachable.

### If you embed triton rather than running this binary

`triton-embed`'s `serve`/`serve_dispatcher` read all of the above for
you. A host that builds its own router and calls `Dispatcher::new`
directly must pass `triton_config::DeploymentConfig::from_env().controls`
— `DispatchControls::unenforced()` compiles and enforces nothing.

That is not hypothetical: the revocation lever originally shipped wired
in this binary only, and the deployment that actually runs embeds the
library. It booted healthy and revoked nobody.

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
