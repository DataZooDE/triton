# 01 — The upstream-agent wire contract

This is the contract your tool-bearing agent MUST implement. Source
of truth: `crates/triton-upstream/src/static_upstream.rs`
(`StaticUpstream`), spec FR-U-1..5 in `doc/requirements.md`.

## The request Triton makes

For each inbound tool call, Triton's upstream router:

1. Resolves your agent by **tool name** to a fixed `host:port` from
   the static `TRITON_STATIC_UPSTREAMS=name=host:port,…` map (FR-U-1;
   no service catalog, no Consul). Tool names are the routing key, so
   they must be globally unique across agents.
2. Mints a fresh short-lived **RS256 OIDC JWT** itself (TTL ≤ 5 min),
   signing with its own key and serving the public key at
   `/.well-known/jwks.json` so your agent can verify it (FR-U-2,
   NFR-S-3 → `references/04`). No Vault. In issuer-less dev, the
   bearer is instead the static `TRITON_STATIC_UPSTREAM_TOKEN`
   (default `dev-token`).
3. **POSTs to `/` on your agent** over HTTP/1.1 on the tailnet:

```
POST / HTTP/1.1
Host: <your-agent-host:port>
Authorization: Bearer <triton-minted-rs256-jwt | dev-token>
X-Triton-Tool: <tool name>
Content-Type: application/json

<the tool's args JSON, verbatim>
```

Key facts, all load-bearing:

- **The path is always `/`.** Triton does not route by tool name in
  the path — the static map already resolved the right agent. You own
  your internal routing if you serve more than one tool from one
  binary; the informational `X-Triton-Tool` header names the tool so
  you can dispatch without sniffing the body.
  (`crates/triton-upstream/src/static_upstream.rs` `invoke`: `let resp
  = self.http.post(format!("http://{ep}/"))`.)
- **The body is the raw args object**, exactly what the dispatcher
  validated against your tool's schema. No envelope, no metadata
  wrapper.
- **The bearer is NOT the inbound caller's token.** Triton mints a
  fresh RS256 JWT scoped to your agent. Verify it (→ `references/04`);
  never try to recover the original caller's token — there is no path
  to it, by design (ADR-3). (This bearer was Vault-minted before the
  Kamal migration; Triton now signs it directly.)
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

## Resolving chat senders: the `upstream` identity strategy (FR-I-7)

A chat adapter normally maps a platform sender id to a subject via an
operator-curated `sender_table`. With `identity.kind: upstream` it
instead delegates resolution to one of YOUR tools, named by
`identity.resolver_tool`. This is a **separate dispatch *before* the
command dispatch**, once per inbound from an unresolved sender, audited
under its own protocol label (e.g. `messenger:whatsapp:identity`) with
its own `trace_id`. The resolver reaches you on the same `POST /` path;
distinguish it by the `X-Triton-Tool` header.

Request body Triton sends:

```json
{ "platform": "whatsapp", "sender": "<platform sender id>" }
```

Response you MUST return to resolve the sender:

```json
{ "sub": "<subject>", "scopes": ["chat"], "tenant": "<tenant>" }
```

`scopes` may be omitted (defaults empty); `sub` and `tenant` must be
non-empty. To refuse a sender, reply non-2xx (or with an empty
`sub`/`tenant`): Triton rejects the inbound `401`, records a rejection,
and **never dispatches the command tool** — no guessed principal. The
command dispatch then runs as the resolved `sub` (today only `sub`
reaches your command-tool bearer; see `doc/upstream-agent-contract.md`
§3 and issue #110).

Manifest (operator side): `identity.kind: upstream` +
`identity.resolver_tool: <one of your tools>` + `tool: <command tool>`
(see `templates/adapter-manifest.yaml`). Normative contract:
`doc/upstream-agent-contract.md` §5; worked round-trip:
`examples/adk-hello-agent/` (`resolve_identity` + `tests/resolver_e2e.rs`).

## MCP-Apps surfaces (optional — #143)

If your agent is an interactive renderer (returns a `ui://` HTML
resource a host renders in a sandboxed iframe), Triton proxies four
extra surfaces to you. All ride the **same** `POST /` endpoint, the
same minted Bearer, and the same principal as a normal tool call —
they're distinguished by an `X-Triton-MCP` header instead of
`X-Triton-Tool`. Triton applies its SSRF guard, circuit breaker, and
one audit line to each, exactly like `tools/call`.

1. **Return a UI resource link.** Put it on your tool *result's*
   `_meta.ui.resourceUri` (plus any sibling `_meta.ui.*` fields):

   ```json
   { "report_id": "r1",
     "_meta": { "ui": { "resourceUri": "ui://peacock/r1" } } }
   ```

   Triton lifts `_meta.ui` onto the `tools/call` response `_meta` so the
   host can load the resource. The authority (`peacock`) is **your
   registry key** — see below.

2. **Serve the resource.** Triton forwards `resources/read` to you as
   `POST /` with `X-Triton-MCP: resources/read` and body
   `{ "uri": "ui://peacock/r1" }`. Reply with
   `{ "contents": [{ "uri", "mimeType", "text"|"blob" }] }`.

3. **Re-render.** An in-iframe `callServerTool('my_tool', {abs params})`
   arrives as an ordinary `tools/call` (header `X-Triton-Tool`). Renders
   are **stateless** — params are absolute, never deltas.

4. **Model-context push.** `updateModelContext` arrives as `POST /` with
   `X-Triton-MCP: updateModelContext` and the iframe's compact
   `{report_id, params, salient_summary}` record as the body, **verbatim**
   (Triton never inspects or expands it).

**Registration gotcha:** the `ui://<authority>/…` authority is resolved
through the *same* `TRITON_STATIC_UPSTREAMS` map as tool dispatch. So
register the authority as its own key alongside your tool keys, both
pointing at your endpoint:
`render_report=peacock.tailnet.ts.net:8080,peacock=peacock.tailnet.ts.net:8080`.

**PNG rasterisation delegation (#143 D):** expose a `render_a2ui_to_png`
tool that takes the dashboard spec `{title, tiles}` and returns
`{ "png_base64": "<base64 PNG>" }`. An operator opts a deployment in with
`TRITON_RASTERIZE_UPSTREAM=render_a2ui_to_png`; Triton's chat surface then
dispatches to you instead of its in-tree sidecar.

## A minimal conformant agent

`templates/upstream-agent-axum/` is a working skeleton: one axum
route at `/`, an OIDC-bearer extractor, a tool returning an A2UI
surface, and an optional `resolve_identity` resolver tool (FR-I-7).
Fork it rather than hand-rolling the wiring.

## What you do NOT implement

- No audit emission — Triton audits the dispatch (`phase: upstream`).
- No discovery registration *in code* — your tool is reachable
  because the operator adds a `name=host:port` entry to Triton's
  `TRITON_STATIC_UPSTREAMS` (→ `references/03`).
- No A2UI version branching — return the canonical `surface`;
  Triton's builders own v0.8 vs v0.9 (ADR-4).
