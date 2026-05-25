# 10 — Out of bounds: what belongs in Triton, not your app

This skill helps you *consume* Triton. Some things look like they
belong in your app but actually belong inside the Triton binary or
the substrate. Building them in your app is the most common way an
integration goes wrong. When you hit one of these, the answer is a
**PR against the Triton repo** (or the substrate repo), not a
workaround.

## Never build these in your app

- **Adapters.** MCP/A2A/REST and the six chat adapters are
  Triton-internal. If you need a new protocol surface, that's a new
  adapter in the Triton repo (ADR-1). Don't reimplement wire parsing
  in your agent.
- **The dispatcher / audit pivot.** One `ToolRegistry::invoke` path
  is the single audit pivot (ADR-6). Your agent is *downstream* of
  it; never emit the dispatcher/upstream audit lines yourself
  (→ `references/09`).
- **The surface mapper or `degrade` rules.** Projection from A2UI to
  `PlatformMessage` is the mapper's job (ADR-12). You emit the
  canonical `surface`; you don't decide how Telegram renders it. A
  new `degrade` behaviour is a Triton change.
- **A2UI version logic.** Don't branch on version anywhere. Return
  the version-agnostic `surface`; the builders own v0.8/v0.9 (ADR-4).
  A new A2UI version = a new builder file in Triton, not an `if`.
- **The identity boundary.** OIDC verification, JWKS caching, the
  four chat identity strategies, the five signature schemes — all
  Triton-internal. Your agent only verifies the token Triton hands
  *it* (→ `references/04`).

## Never do these anywhere

- **Forward the inbound caller's token.** You never receive it. Your
  agent's identity to anything it calls is its own Vault-minted token
  (ADR-3, lethal-trifecta cut). No relaying.
- **Bypass Triton to reach a chat platform or another agent.** Calls
  go through the dispatcher so audit symmetry holds. The courier owns
  the platform credential; your agent does not call Telegram/Discord
  directly.
- **Static credentials.** No tokens in code, HCL, or env literals.
  Vault references only (NFR-S-1, M-SECRETS-1). A literal credential
  in a manifest fragment makes the operator's prod boot fail.
- **On-disk user state.** Stateless across restarts (G-8). No Redis
  "just for one tool", no session files. Each call is a fresh
  `(tool, args, principal)`.
- **A log shipper in your binary.** No Loki/Vector/OTel exporter.
  Emit JSON to stdout; the substrate collector ships it (ADR-7).
- **`:latest` images, on-the-fly provisioning.** Pin image SHAs;
  buckets/Vault policies/DNS/Tailscale tags are substrate-repo PRs
  (→ `references/11`).

## The escalation path

If your app genuinely needs a capability Triton doesn't expose:

1. **A new tool surface concept** (a component type, a richer chat
   degradation) → open an issue/PR against the **Triton repo**;
   reference the spec clause it extends (`doc/requirements.md`).
2. **A new backing-service capability** (a bucket, a Vault policy, a
   public hostname, a Tailscale tag) → PR against the **substrate
   repo** (see `substrate-platform/references/09-out-of-bounds.md`).
3. **A breaking change to the test harness `pub` surface** → it's
   governed by ADR-16's one-release deprecation cycle; file a
   follow-up rather than depending on un-deprecated internals.

Working *with* these boundaries is what keeps your app decoupled from
Triton's release cycle — the whole point of the gateway (the
"Agent author" stakeholder concern in `doc/requirements.md` §4).
