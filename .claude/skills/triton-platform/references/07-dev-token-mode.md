# 07 — dev-token mode for local dev and CI

Triton has a "minimum-viable boot" mode for development: no Consul, no
Vault, no OIDC issuer, an empty manifest, and the literal bearer
`"dev-token"` accepted as a fixed dev principal. This is the FR-T-1
contract and the foundation of the consumer test harness
(→ `references/08`).

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
2. **Triton → your agent** — Triton still mints a token via Vault to
   call you. In the harness, a `FakeVault` mints a stub token, or you
   simply skip verification in dev. The `templates/upstream-agent-axum/`
   skeleton accepts `dev-token` directly when no issuer is set, so
   you can run it standalone without a Vault.

## The minimum-viable boot, verified

ACC-13 pins this exact path: an external crate depending on
`triton-tests` boots Triton with nothing configured, posts
`dev-token` to `GET /v1/tools`, and gets 200. The live proof is
`examples/consumer-smoke/` — a crate *outside* the Triton workspace
that does precisely this. Copy its shape; it's the smallest possible
"is my wiring right?" check.

## Don't ship it

- Never set the `dev-token` feature on a release build.
- Never hardcode `dev-token` as a fallback in your *agent's*
  production path — gate it behind a build-time `cfg`, as Triton does.
- Treat `dev-token` strictly as a CI/local affordance. The moment a
  real issuer exists in an environment, it stops working — by design.
