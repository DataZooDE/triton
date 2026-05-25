# my-tool-agent — an upstream agent for Triton

A minimal tool-bearing agent that Triton dispatches into. Fork it,
rename the crate, replace the tool logic.

## What it does

- Exposes `POST /` — the only path Triton calls (it routed to you via
  Consul, so there's no per-tool path on your side). See
  `references/01`.
- Verifies Triton's `Authorization: Bearer` token before doing work
  (`references/04`).
- Returns a canonical A2UI `surface`; Triton builds v0.8/v0.9 or a
  chat `PlatformMessage` from it (`references/02`). Return plain JSON
  instead and Triton wraps it.
- Exposes `GET /healthz` for the Consul check.

## Run it locally (dev-token, no issuer)

```sh
cargo run
# in another shell:
curl -s -X POST http://localhost:8080/ \
  -H "Authorization: Bearer dev-token" \
  -H "Content-Type: application/json" \
  -d '{"subject":"Berlin"}' | jq
```

You get back `{ "surface": { "components": [ … ] } }`.

## Environment

| Var | Meaning |
|---|---|
| `AGENT_PORT` | Listen port (default 8080; in prod comes from the Nomad `network` stanza). |
| `AGENT_OIDC_ISSUER` | Substrate OIDC issuer URL. **Set this in prod** to verify real tokens; unset → dev-token path. |
| `AGENT_OIDC_AUDIENCE` | Expected `aud` claim (your agent's identity). Optional; unset skips `aud` checking. |

## Building for production

```sh
cargo build --release --no-default-features
```

`--no-default-features` compiles out the `dev-token` path entirely, so
a release binary cannot accept `dev-token` even by misconfiguration
(ADR-10). Set `AGENT_OIDC_ISSUER` in the deployment.

## Before you ship

- Replace the `verify_oidc` sketch with the cached JWKS recipe from
  `substrate-platform/references/11-oidc-verification.md` — the
  skeleton fetches JWKS per request, which is fine for a demo, not
  for production.
- Widen the algorithm allowlist to match the real substrate issuer
  (RS256/ES256), not just EdDSA.
- Register the tool: Consul `tag:agent:my-tool` (see
  `templates/agent.nomad.hcl`) and the operator's `adapter.yaml`
  entry (see `templates/adapter-manifest.yaml`).
- Never log the bearer token; log the verified `sub` only
  (`references/09`).
