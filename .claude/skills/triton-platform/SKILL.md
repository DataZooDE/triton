---
name: triton-platform
version: 0.2.0
description: Use when developing an application that integrates with the DataZoo Triton agent-ingress gateway — either an upstream agent Triton dispatches into (registered in Consul as `tag:agent:<name>`) or a frontend/client that calls Triton's HTTP trio (REST/MCP/A2A). Provides the upstream-agent wire contract, A2UI envelope shapes, the `adapter.yaml` manifest fragment, the OIDC verification recipe, the `crates/triton-tests` consumer test harness, and ready-to-fork templates. Triggers on phrases like "agent for Triton", "tool that Triton calls", "A2UI envelope", "adapter.yaml entry", "TritonProcess test", "verify Triton's bearer", "register a new tool", "chat-channel surface", "render_a2ui_to_png". DO NOT use for Triton-internal work (writing adapters, the dispatcher, the surface mapper, the identity boundary) — that is work inside the Triton repo itself, not consumer-facing.
---

# triton-platform — build apps that integrate with Triton

You are helping someone build an **application that talks to Triton**,
the DataZoo agent-ingress gateway. Triton itself is a single Rust
binary in its own repo (`DataZooDE/triton`); this skill is the
**developer-facing contract** for integrating with it. There are two
integration roles, and most apps are one of them:

- **Upstream-agent author** (the common case). You are building a
  Nomad job that implements one or more *tools*. Triton discovers it
  via Consul (`tag:agent:<tool_name>`), dispatches inbound calls to
  it, and ships the audit trail. Your deliverable receives a
  Vault-minted OIDC bearer and returns JSON or an A2UI surface.
- **Frontend / client author**. You are building something that
  *calls* Triton — over REST, MCP, or A2A — and consumes the A2UI
  envelopes it returns.

This skill is **read-only documentation + templates**. It makes no
live API calls, holds no credentials, and runs no operator commands.
Anything that requires changing Triton itself (a new adapter, a new
A2UI version, a manifest schema change) is a **PR against the Triton
repo**, not a workaround in your app — see
`references/10-out-of-bounds.md`.

## How this skill is installed

The Triton repo is checked out locally and this skill directory is
**symlinked** into the consumer repo's `.claude/skills/`:

```sh
# in the consumer repo root, with the triton repo checked out somewhere
ln -s ../path/to/triton/.claude/skills/triton-platform \
      .claude/skills/triton-platform
```

The Triton repo's checked-out git ref is the version pin. Check
`VERSION` and `CHANGELOG.md` for what you're on; move forward by
`git pull` (or fast-forward to a tag) inside the Triton checkout.

## Progressive-disclosure index

Read only the references relevant to the task at hand. Each is small
(2–4 KB) and self-contained. They *navigate to* the canonical spec in
`doc/` and the source in `crates/` rather than restating it.

| File | Read when… |
|---|---|
| `references/00-what-is-triton.md` | First contact. The nine adapters, the dispatcher pivot, where your app sits in the picture. |
| `references/01-upstream-agent-contract.md` | Building a tool Triton calls. The exact HTTP wire shape, bearer, request/response body. |
| `references/02-a2ui-envelopes.md` | Your tool returns a UI surface. The `{surface:{components:[…]}}` shape, v0.8 vs v0.9, who builds what. |
| `references/03-tool-registration.md` | Making a tool reachable: Consul `tag:agent:<name>` + the operator-side `adapter.yaml` entry. |
| `references/04-oidc-verification.md` | Your agent verifies Triton's per-call OIDC bearer. Issuer, audience, JWKS caching; defers crypto to substrate-platform. |
| `references/05-surface-and-degrade.md` | Your surface targets chat channels. `degrade` rules, `SurfaceLimits` caps, the rasteriser for dashboards. |
| `references/06-frontend-client.md` | Building the *caller* side (REST/MCP/A2A): tool discovery, content negotiation, error model. |
| `references/07-dev-token-mode.md` | Local dev / CI with no OIDC issuer. The `dev-token` affordance and its production safety net. |
| `references/08-consumer-test-harness.md` | Writing `frontend → triton → app-agent` integration tests against a real Triton process. The `triton-tests` `pub` surface. |
| `references/09-audit-and-logging.md` | What your app logs, what Triton audits, and what must NEVER appear in either. |
| `references/10-out-of-bounds.md` | Before writing anything that feels like it belongs *inside* Triton. Hard prohibitions. |
| `references/11-substrate-crossref.md` | Where the `substrate-platform` skill covers the same surface (Nomad/Vault/Consul/OIDC) from the platform side. |

## Templates (`templates/`)

Fork the one that matches your task — don't write from scratch. Each
encodes the conventions documented in the references. See
`templates/README.md` for a "fork this if…" matrix.

| Template | Use for |
|---|---|
| `templates/upstream-agent-axum/` | A working Rust upstream-agent skeleton: axum handler, OIDC-bearer verification, a tool returning an A2UI v0.9 surface. |
| `templates/consumer-integration-test/` | A drop-in `tests/triton_e2e.rs` (+ Cargo snippet) that boots a real Triton with `FakeConsul` + `FakeAgent` and drives it end-to-end. |
| `templates/adapter-manifest.yaml` | The `adapter.yaml` fragment an operator pastes to register your tool and its chat-channel `degrade` rules. |
| `templates/agent.nomad.hcl` | The Nomad job stanza: `tag:agent:<name>` Consul registration, Vault verifier wiring, tailnet binding. |

## Hard prohibitions

- **No Triton-internal code.** Don't write adapters, dispatcher
  logic, surface-mapper rules, or audit emitters in your app — those
  live in the Triton binary.
- **No bypassing Triton.** Don't call chat platforms or other agents
  directly; everything goes through the dispatcher so audit symmetry
  holds.
- **No raw-token forwarding.** Your agent receives a fresh
  Vault-minted token scoped to it, never the inbound caller's token.
  Don't try to recover or reuse the original.
- **No static credentials, no on-disk user state.** Vault references
  only; stateless across restarts.

See `references/10-out-of-bounds.md` for the complete list and the
escalation path.
