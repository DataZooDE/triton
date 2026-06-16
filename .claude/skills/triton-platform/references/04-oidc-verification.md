# 04 — Verifying Triton's bearer in your agent

Triton calls your agent with a **short-lived RS256 OIDC JWT** (TTL ≤ 5
min) that **Triton itself mints and signs** per call, scoped to your
agent — never the inbound caller's token (FR-U-2, ADR-3, NFR-S-3).
Triton is its own issuer here: it serves the public key at
`/.well-known/jwks.json` (and `/.well-known/openid-configuration` for
discovery), and your agent verifies the token against that JWKS.
(Before the Kamal migration this token was minted by Vault via the
`agent-oidc-swap` role; Vault is gone — Triton signs directly. The
wire shape and TTL cap are unchanged.)

This reference names the **callee shape** Triton expects. The actual
cryptography — JWKS discovery, caching, rotation, the per-language
verify recipe — is documented once in the substrate skill:
**`substrate-platform/references/11-oidc-verification.md`**. Read that
for the code; read this for what's Triton-specific.

## What's specific to verifying Triton's token

- **Issuer** = Triton's own self-issuer, configured on the Triton
  deploy via `TRITON_SELF_ISSUER` (the signer is enabled with
  `TRITON_JWT_SIGNING_KEY` + `TRITON_SELF_ISSUER` + `TRITON_JWT_JWKS`,
  all-or-nothing; key id from `TRITON_JWT_KID`). Discover the issuer's
  JWKS at `<issuer>/.well-known/jwks.json` via OIDC discovery, not
  hardcoded. This is Triton's own URL, **not** the substrate's inbound
  OIDC issuer.
- **Audience** = the agents-environment identity Triton signs into the
  `aud` claim. It defaults to `agents-<env>` (e.g. `agents-nonprod`)
  and the operator can override it (`TRITON_STATIC_UPSTREAM_AUD`); it
  may be a comma-separated list when a token is meant to be forwarded
  to a further downstream (each hop pins its own `aud`). Enforce your
  own `agents-<env>` value if you can; otherwise verify issuer + sig +
  exp and treat a valid Triton-issued token as sufficient (the tailnet
  ACL already restricts who can reach you).
- **Algorithms**: accept only RS256/384/512, ES256/384, EdDSA.
  Reject `none` and symmetric algs — same allowlist Triton enforces
  on the inbound side (FR-I-3). Triton signs with RS256.
- **JWKS caching**: cache per-`kid`, refetch on miss, don't crash on
  a single fetch failure. Fetch the set from Triton's
  `/.well-known/jwks.json`. (`substrate-platform/references/11` §2.)

## Minimal stance

Your agent is reachable **only** over the tailnet, only by Triton
(the substrate ACL grants `tag:cli → tag:agents` and nothing else,
G-S5). So the OIDC check is defence-in-depth, not your sole gate. But
verify it anyway:

- It confirms the caller really is Triton holding a fresh Triton-
  signed token, not a lateral-movement attempt from a compromised
  neighbour.
- It gives you a verified `sub` for your own audit lines
  (→ `references/09`).

## The dev-token escape hatch (local / CI only)

In local dev and CI you usually run with no signing key configured.
When Triton has no signer, it sends the static
`TRITON_STATIC_UPSTREAM_TOKEN` bearer (default `dev-token`) instead of
a minted JWT. The `templates/upstream-agent-axum/` skeleton accepts
the literal `Bearer dev-token` in that case, mirroring Triton's own
`dev-token` mode (→ `references/07`). This is gated so it cannot
ship: compile the dev path out of release builds (a Cargo feature),
exactly as Triton does (ADR-10). Never let a dev-token affordance
reach a production binary.

## Don't

- Don't forward the inbound caller's token onward — you never receive
  it (lethal-trifecta cut). Your agent's identity to *anything it
  calls* is its own minted token, not a relayed one. If your agent
  calls another substrate service, mint your own — see
  `substrate-platform/references/14-app-to-app-auth.md`. (One
  exception: when Triton signs a multi-`aud` token specifically so a
  downstream like Escurel can verify its own audience, your agent
  forwards *that* Triton-minted token unchanged; it never forwards the
  original *inbound caller's* token.)
- Don't cache a verified principal across requests — each call is a
  fresh `(tool, args, principal)` (statelessness, G-8).
- Don't log the token. Log the verified `sub` only (→ `references/09`).
