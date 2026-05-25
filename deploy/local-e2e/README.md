# Local end-to-end testing

Drive a real Triton process end to end — MCP + A2UI first, since
that is the core gateway surface (the chat adapters are wrappers on
top of the same dispatcher). Two scripts:

| Script | What it proves | Network |
|---|---|---|
| `mcp-smoke.sh` | Wire-level: a JSON-RPC MCP client can `initialize` → `tools/list` → `tools/call` and get a real A2UI surface back (v0.8 **and** v0.9), plus the error model. Boots Triton, asserts, tears down. | localhost only |
| `funnel-mcp.sh` | A **real remote MCP host** (MCP Inspector, Claude Desktop, an SDK client) drives Triton over Tailscale Funnel. | public HTTPS |

Both run Triton in **minimum-viable boot** (FR-T-1): no manifest, no
Consul, no Vault, no OIDC issuer. The in-process tools (`demo_panel`,
`echo`, `narrate`, …) stand in for upstream agents, so no backing
services are needed. Auth is the literal `Bearer dev-token`.

## 1. Wire smoke (no network setup)

```sh
./deploy/local-e2e/mcp-smoke.sh
```

Expected tail:

```
✓ MCP + A2UI end-to-end smoke passed against a real Triton process.
```

This is the fastest confidence check and runs in CI-like isolation.
`demo_panel` exercises every A2UI component (text, narration,
dashboard, selection, form, button) through both version builders.

## 2. Real MCP host over Tailscale Funnel

> **Public exposure.** Funnel publishes to the open internet. In
> dev-token mode, anyone who sends `Authorization: Bearer dev-token`
> can call your tools while the funnel is up. Keep the window short
> and supervised; Ctrl-C tears it down. For longer-lived exposure,
> set `TRITON_OIDC_ISSUER` (dev-token is then rejected) or switch the
> script to `tailscale serve` (tailnet-only).

Prerequisites: `tailscale` logged in, and **Funnel enabled** for the
tailnet (admin console → Access Controls → `nodeAttrs` grants
`funnel`).

```sh
./deploy/local-e2e/funnel-mcp.sh
```

It prints the public `https://<node>.<tailnet>.ts.net` URL and three
ready-to-paste client recipes:

- **MCP Inspector** — `npx @modelcontextprotocol/inspector`, transport
  *Streamable HTTP*, the funnel URL, header
  `Authorization: Bearer dev-token`. Interactive list/call UI.
- **curl** — a one-liner `tools/list` against the funnel URL.
- **Claude Desktop / Code** — the `mcpServers` HTTP-transport stanza.

## What this does NOT cover yet

- **Real upstream agent.** These runs dispatch to *in-process* tools.
  A `frontend → triton → real-agent` round-trip needs a Nomad agent
  registered in Consul (`tag:agent:<name>`) + Vault for per-call
  OIDC. See the `triton-platform` skill (`references/01`, `03`) and
  `templates/upstream-agent-axum/`.
- **Real OIDC.** dev-token only. Production auth is a substrate OIDC
  bearer; see `triton-platform` `references/04`.
- **Chat platforms.** Telegram/Discord/etc. are operator-configured
  via `adapter.yaml`; a separate runbook will cover the tunnel +
  webhook-registration flow when we wire a real bot.
- **Substrate deploy.** These scripts run the binary directly. The
  Nomad/Packer wrapper is operator work (`substrate-platform` skill).
