# 07 — dev-token mode for local dev and CI

Triton has a "minimum-viable boot" mode for development: no upstream
signing key, no OIDC issuer, an empty manifest, and the literal bearer
`"dev-token"` accepted as a fixed dev principal. This is the FR-T-1
contract and the foundation of the consumer test harness
(→ `references/08`). (There is no Consul or Vault to stand up either —
both were removed in the Kamal migration.)

Source: `doc/requirements.md` FR-T-1, ACC-13; `doc/architecture.md`
§8.8, ADR-10; the canonical example at `examples/consumer-smoke/`.

## The contract

- The literal `Bearer dev-token` maps to
  `Principal{sub: "dev-user", scopes: ["dev"], tenant: "dev"}`.
- **On by default in debug builds** via the `dev-token` Cargo feature
  on `triton-bin`.
- **Compiled out of release builds** (`--no-default-features`), so the
  affordance cannot leak into a shipped image (ADR-10). This is the
  dev/prod-parity trade-off: a debug convenience that is physically
  absent from production.
- **`TRITON_OIDC_ISSUER` flips it off.** When an issuer is configured,
  dev-token is rejected with 401 — identical to production. This is
  the safety net: a production-shaped deploy that points at a real
  issuer will not also honour dev-token
  (`crates/triton-tests/tests/consumer_smoke.rs`,
  `dev_token_is_rejected_when_oidc_issuer_is_configured`).

## Using it

### As a client / frontend author

Point at a Triton booted with no issuer and send `Bearer dev-token`:

```sh
curl -H "Authorization: Bearer dev-token" http://localhost:8003/v1/tools
# → 200, a tools array
```

### As an upstream-agent author

Two layers of dev-token in play, don't conflate them:

1. **Inbound to Triton** — your *test's* client sends `dev-token` to
   Triton (above).
2. **Triton → your agent** — with no signing key configured, Triton
   sends the static `TRITON_STATIC_UPSTREAM_TOKEN` bearer (default
   `dev-token`) rather than a minted JWT. The
   `templates/upstream-agent-axum/` skeleton accepts `dev-token`
   directly when no issuer is set, so you can run it standalone with
   no signer to verify against. (Configure
   `TRITON_JWT_SIGNING_KEY` + `TRITON_SELF_ISSUER` + `TRITON_JWT_JWKS`
   to exercise the real RS256 path instead → `references/04`.)

## The minimum-viable boot, verified

ACC-13 pins this exact path: an external crate depending on
`triton-tests` boots Triton with nothing configured, posts
`dev-token` to `GET /v1/tools`, and gets 200. The live proof is
`examples/consumer-smoke/` — a crate *outside* the Triton workspace
that does precisely this. Copy its shape; it's the smallest possible
"is my wiring right?" check.

## Sibling: forwarded-auth sidecar mode

dev-token is one of two issuer-less auth modes. The other is the
**oauth2-proxy sidecar** path: with `TRITON_TRUST_FORWARDED_AUTH=true`
(and still no OIDC issuer), Triton trusts an `X-Forwarded-Email` header
from a co-located sidecar instead of a bearer (ADR-0011 / issue #67).
That's the path the substrate demo deploys behind real SSO; dev-token
is the path for local/CI with no sidecar. Both are disabled the moment
`TRITON_OIDC_ISSUER` is set. Full client-side detail is in
`references/06` → "Auth — three inbound modes".

## Don't ship it

- Never set the `dev-token` feature on a release build.
- Never hardcode `dev-token` as a fallback in your *agent's*
  production path — gate it behind a build-time `cfg`, as Triton does.
- Treat `dev-token` strictly as a CI/local affordance. The moment a
  real issuer exists in an environment, it stops working — by design.
