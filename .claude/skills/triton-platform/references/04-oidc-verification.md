# 04 — Verifying Triton's bearer in your agent

Triton calls your agent with a **Vault-minted, short-lived OIDC
token** (TTL ≤ 5 min), scoped to your agent via the `agent-oidc-swap`
Vault role — never the inbound caller's token (FR-U-2, ADR-3,
NFR-S-3). Your agent verifies this token at its boundary before doing
any work.

This reference names the **callee shape** Triton expects. The actual
cryptography — JWKS discovery, caching, rotation, the per-language
verify recipe — is documented once in the substrate skill:
**`substrate-platform/references/11-oidc-verification.md`**. Read that
for the code; read this for what's Triton-specific.

## What's specific to verifying Triton's token

- **Issuer** = the substrate OIDC issuer (same one Triton verifies
  inbound tokens against). Discover it from Consul KV / OIDC
  discovery, not hardcoded. (`substrate-platform/references/11` §1.)
- **Audience** = your agent's own substrate identity. The
  `agent-oidc-swap` role mints a token scoped to the resolved agent;
  enforce `aud` if your Vault role binds it. If you're using the
  default workload role without an `aud` claim, verify issuer + sig +
  exp and treat the presence of a valid substrate token as
  sufficient (the tailnet ACL already restricts who can reach you).
- **Algorithms**: accept only RS256/384/512, ES256/384, EdDSA.
  Reject `none` and symmetric algs — same allowlist Triton enforces
  on the inbound side (FR-I-3).
- **JWKS caching**: cache per-`kid`, refetch on miss, don't crash on
  a single fetch failure. The substrate rotates keys every 6h with a
  24h verification TTL — expect ≥ 2 keys in the set.
  (`substrate-platform/references/11` §2.)

## Minimal stance

Your agent is reachable **only** over the tailnet, only by Triton
(the substrate ACL grants `tag:cli → tag:agents` and nothing else,
G-S5). So the OIDC check is defence-in-depth, not your sole gate. But
verify it anyway:

- It confirms the caller really is Triton holding a fresh substrate
  token, not a lateral-movement attempt from a compromised neighbour.
- It gives you a verified `sub` for your own audit lines
  (→ `references/09`).

## The dev-token escape hatch (local / CI only)

In local dev and CI you usually run with no OIDC issuer. The
`templates/upstream-agent-axum/` skeleton accepts the literal
`Bearer dev-token` when no issuer is configured, mirroring Triton's
own `dev-token` mode (→ `references/07`). This is gated so it cannot
ship: compile the dev path out of release builds (a Cargo feature),
exactly as Triton does (ADR-10). Never let a dev-token affordance
reach a production binary.

## Don't

- Don't forward the inbound caller's token onward — you never receive
  it (lethal-trifecta cut). Your agent's identity to *anything it
  calls* is its own minted token, not a relayed one. If your agent
  calls another substrate service, mint your own via the Nomad
  `identity` stanza — see
  `substrate-platform/references/14-app-to-app-auth.md`.
- Don't cache a verified principal across requests — each call is a
  fresh `(tool, args, principal)` (statelessness, G-8).
- Don't log the token. Log the verified `sub` only (→ `references/09`).
