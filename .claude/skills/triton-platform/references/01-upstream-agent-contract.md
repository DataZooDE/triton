# 01 — The upstream-agent wire contract

This is the contract your tool-bearing agent MUST implement. Source
of truth: `crates/triton-upstream/src/lib.rs` (`UpstreamRouter`), spec
FR-U-1..5 in `doc/requirements.md`.

## The request Triton makes

For each inbound tool call, Triton's upstream router:

1. Resolves your agent via Consul: `tag:agent:<tool_name>` (FR-U-1).
2. Mints a fresh short-lived OIDC token via Vault role
   `agent-oidc-swap`, TTL ≤ 5 min (FR-U-2, NFR-S-3).
3. **POSTs to `/` on your agent** over HTTP/1.1 on the tailnet:

```
POST / HTTP/1.1
Host: <your-agent-host:port>
Authorization: Bearer <vault-minted-oidc-jwt>
Content-Type: application/json

<the tool's args JSON, verbatim>
```

Key facts, all load-bearing:

- **The path is always `/`.** Triton does not route by tool name on
  your side — Consul already resolved the right agent. You own your
  internal routing if you serve more than one tool from one binary.
  (`crates/triton-upstream/src/lib.rs` `do_dispatch`: `let url =
  format!("http://{endpoint}/")`.)
- **The body is the raw args object**, exactly what the dispatcher
  validated against your tool's schema. No envelope, no metadata
  wrapper.
- **The bearer is NOT the inbound caller's token.** It is Vault-minted
  and scoped to your agent. Verify it (→ `references/04`); never try
  to recover the original caller's token — there is no path to it,
  by design (ADR-3).
- **Transport is cleartext HTTP/1.1 over Tailscale.** No TLS in your
  agent; the tailnet provides transport security. (`doc/requirements.md`
  §3.2.)

## The response Triton expects

Return HTTP `200` with a JSON body. Two shapes are valid:

### (a) Raw JSON result

Any JSON object. If the inbound caller negotiated A2UI, Triton wraps
your raw result into an A2UI envelope; if not, it passes the JSON
through. This is the simplest path — return your domain result and
let Triton handle presentation. (FR-U-5.)

```json
{ "mean": 42.0, "stddev": 3.1, "n": 17 }
```

### (b) A pre-shaped A2UI surface

If your tool drives a UI, return a `surface` object and Triton builds
the version the caller negotiated (v0.8 or v0.9). Shape and component
vocabulary: `references/02` and `crates/triton-core/src/a2ui/mod.rs`.

```json
{ "surface": { "components": [
  { "kind": "text", "value": "Stats for last 7 days" },
  { "kind": "narration", "text": "Traffic is up 12%." }
] } }
```

Triton's `extract_surface` parses the `surface` field; if your tool is
declared as returning A2UI but the shape doesn't deserialise, the call
surfaces as a `Tool` error at the API boundary (intentional — the bug
shows up loudly, not as a silent downgrade).

## Failure semantics

- Non-2xx from your agent → Triton raises `TritonError::Tool`; the
  client sees REST 502 / A2A `metadata.error: "tool"` / MCP
  `-32000`. (`doc/architecture.md` §8.3.)
- Slow agent → after N consecutive timeouts (default 5) the per-tool
  circuit-breaker opens; calls return `circuit_open` for a cooldown
  (default 30 s). Other tools are unaffected. (FR-U-3/U-4.) Keep your
  handler's own timeout budget under Triton's `upstream_timeout`
  (default 10 s) so the breaker reflects real agent health.
- Your error body is **not** parsed for status mapping — only the
  HTTP status matters. Put diagnostics in your own stdout logs
  (→ `references/09`).

## A minimal conformant agent

`templates/upstream-agent-axum/` is a working skeleton: one axum
route at `/`, an OIDC-bearer extractor, and a tool returning an A2UI
surface. Fork it rather than hand-rolling the wiring.

## What you do NOT implement

- No audit emission — Triton audits the dispatch (`phase: upstream`).
- No Consul registration *in code* — that's a Nomad job tag
  (→ `references/03`, `templates/agent.nomad.hcl`).
- No A2UI version branching — return the canonical `surface`;
  Triton's builders own v0.8 vs v0.9 (ADR-4).
