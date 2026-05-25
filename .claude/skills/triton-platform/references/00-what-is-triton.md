# 00 — What Triton is, and where your app sits

Triton is the public **agent-ingress gateway** of the DataZoo Hetzner
substrate. One Rust binary exposes **nine adapters**, verifies every
inbound credential, and dispatches each tool call to an **upstream
agent** discovered via Consul. It returns either plain JSON or an
A2UI surface, in the wire format the caller used.

Full picture: `doc/architecture.md` §3 (context) and §5.1 (Level-1
whitebox). The one-paragraph version:

```
clients ─▶ Fabio :443 ─▶ Triton ─▶ upstream agents (your tools)
            (MCP/A2A/REST + 6 chat channels)   (tag:agent:<name> in Consul)
                              │
                              ├─ identity boundary (OIDC / platform sig)
                              ├─ dispatcher  ◀── the single audit pivot
                              ├─ upstream router (Consul + Vault swap + breaker)
                              └─ audit → stdout → substrate collector
```

## The nine adapters

- **HTTP trio**: MCP (`:8001`), A2A (`:8002`), REST (`:8003`). Three
  TCP listeners, semantically identical A2UI across all three.
- **Six chat channels** (v0.2): Telegram, WhatsApp Web, Signal, MS
  Teams, Discord, Google Chat. Each splits into an inbound listener
  (unwrap) and an outbound courier (wrap), with the dispatcher
  between them.

You do **not** write adapters — they are internal to the Triton
binary. You write the thing on either end of the arrows.

## Where your app sits

| Your app is… | You build… | Triton sees you as… |
|---|---|---|
| an **upstream agent** | a Nomad job implementing tools | a Consul service `tag:agent:<tool>` it dispatches into → `references/01` |
| a **frontend / client** | an MCP host, A2A peer, or REST caller | a caller hitting `:443` → `references/06` |

Most DataZoo "agentic apps" are the first kind: a tool-bearing agent.
The dispatcher hands you `(tool, args, principal)`; you return a
result. You never see the wire protocol the original client used —
that is the adapter's job, and the whole point of Triton is that your
agent is protocol-agnostic.

## Three invariants that shape everything you build

1. **Audit symmetry.** Every call produces a linked audit pair
   (inbound dispatcher record + outbound router/courier record)
   sharing one `trace_id`. You don't emit these — Triton does. Don't
   duplicate them. (`doc/architecture.md` §8.2, FR-AU-1.)
2. **Statelessness.** No user data persists across restarts; each
   interaction is a fresh `(tool, args, principal)` invocation. Your
   agent must be stateless too (G-8). (`doc/realizations.md` §1.)
3. **Lethal-trifecta cut.** Triton never forwards the inbound token
   to you. You get a fresh Vault-minted OIDC token scoped to your
   agent, TTL ≤ 5 min. (NFR-S-3, ADR-3 → `references/04`.)

## When NOT to use this skill

If you are editing the Triton binary itself — adding an adapter,
changing the dispatcher, writing surface-mapper rules — this skill
does not apply. That is repo-internal work governed by Triton's own
`CLAUDE.md`. This skill is for **consumers** of Triton.
