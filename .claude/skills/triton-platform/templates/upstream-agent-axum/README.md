# my-tool-agent — an upstream agent for Triton

A minimal tool-bearing agent that Triton dispatches into. Fork it,
rename the crate, replace the tool logic.

## What it does

- Exposes `POST /` — the only path Triton calls. Triton resolved you
  by tool name from its static `TRITON_STATIC_UPSTREAMS` map (not
  Consul), so there is no per-tool path on your side; the tool name
  rides the `X-Triton-Tool` request header. See `references/01`.
- Verifies Triton's `Authorization: Bearer` token before doing work.
  Outside dev this is a short-TTL RS256 JWT Triton mints per call,
  verified against Triton's JWKS; in dev it is the static `dev-token`
  (`references/04`).
- Returns a canonical A2UI `surface`; Triton builds v0.8/v0.9 or a
  chat `PlatformMessage` from it (`references/02`). Return plain JSON
  instead and Triton wraps it.
- Exposes `GET /healthz` for a liveness probe.

## Run it locally (dev-token, no issuer)

```sh
cargo run
# in another shell:
curl -s -X POST http://localhost:8080/ \
  -H "Authorization: Bearer dev-token" \
  -H "X-Triton-Tool: my-tool" \
  -H "Content-Type: application/json" \
  -d '{"subject":"Berlin"}' | jq
```

You get back `{ "surface": { "components": [ … ] } }`.

## Environment

| Var | Meaning |
|---|---|
| `AGENT_PORT` | Listen port (default 8080). This is the port the operator names in `TRITON_STATIC_UPSTREAMS=my-tool=<host>:<port>`. |
| `AGENT_OIDC_ISSUER` | Triton's self-issuer URL. **Set this in prod** to verify the RS256 JWTs Triton mints (keys at `<issuer>/.well-known/jwks.json`); unset → dev-token path. |
| `AGENT_OIDC_AUDIENCE` | Expected `aud` claim (your agent's identity, e.g. `agents-nonprod`). Optional; unset skips `aud` checking. |

## Building for production

```sh
cargo build --release --no-default-features
```

`--no-default-features` compiles out the `dev-token` path entirely, so
a release binary cannot accept `dev-token` even by misconfiguration
(ADR-10). Set `AGENT_OIDC_ISSUER` in the deployment.

## Before you ship

- Replace the `verify_oidc` sketch with the cached JWKS recipe from
  `references/04` — the skeleton fetches JWKS per request, which is
  fine for a demo, not for production.
- The algorithm is pinned to RS256 to match the tokens Triton mints —
  leave it RS256.
- Register the tool two ways: the operator adds
  `my-tool=<your-agent-host:port>` to Triton's `TRITON_STATIC_UPSTREAMS`
  env var (no Consul tag), and merges your `adapter.yaml` entry (see
  `templates/adapter-manifest.yaml`). The agent itself is deployed via
  the substrate's Kamal config (see `substrate-platform`), not Nomad.
- Never log the bearer token; log the verified `sub` only
  (`references/09`).
