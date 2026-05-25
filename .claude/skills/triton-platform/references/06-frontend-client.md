# 06 — Calling Triton from a frontend / client

This is the *caller* side: you're building an MCP host, an A2A peer,
or a plain REST/SDK client that drives tools through Triton. You hit
one stable URL (`agents.<env>.<domain>` on `:443`, Fabio-routed) and
get back A2UI in the wire format you used. Source: FR-A-3..7,
`doc/requirements.md` §5.1.

## REST (the simplest path)

- **Discover tools**: `GET /v1/tools` → tool names, input JSON
  schemas, and a `returns_a2ui` flag per tool (FR-A-5). Mirrors the
  Python reference; consume this to build your call shapes.
- **Invoke**: `POST /v1/tools/<tool>` with the args JSON as body and
  a bearer token.
- **Negotiate A2UI**: `Accept: application/json+a2ui` (default →
  v0.8) or `Accept: application/json+a2ui; version=0.9` (FR-A-3).
  Omit it to get the raw JSON result.

```http
POST /v1/tools/compute_stats HTTP/1.1
Authorization: Bearer <token>
Accept: application/json+a2ui; version=0.9
Content-Type: application/json

{ "window": "7d" }
```

The response is a v0.9 envelope: `{ "version": "0.9", "stream": [...] }`
(→ `references/02`).

## MCP

Streamable HTTP, JSON-RPC 2.0 over HTTP, plain JSON responses (SSE
not required). Methods: `initialize`, `tools/list`, `tools/call`,
`resources/read` (FR-A-6). The runtime resource
`ui://triton/runtime.html` is a stub on the substrate deployment —
don't depend on it serving a real Lit runtime. A2UI version follows
the negotiated MCP App.

## A2A

`POST /message:send` with `Message{parts:[Part{data:{tool, args}}]}`;
the response `Message`'s part carries the result. Request v0.9 via
`Message.metadata.a2ui_version: "v0.9"` (FR-A-7). Triton uses an
`InMemoryTaskStore` — no user data persists, so don't rely on task
durability across restarts.

## Error model

All three protocols map the same four dispatcher error variants
(`doc/architecture.md` §8.3):

| Variant | REST | A2A `metadata.error` | MCP code |
|---|---|---|---|
| Auth | 401 | `auth` | `-32001` |
| Validation | 400 | `validation` | `-32602` |
| Tool | 502 / 504 timeout / 503 `circuit_open` | `tool` | `-32000` |
| Provider | 502 | `provider` | `-32000` |

Handle `503 circuit_open` as "this tool is temporarily down, others
are fine" — it's per-tool, not gateway-wide (FR-U-4).

## Auth

You present a substrate OIDC bearer (the issuer is the substrate
identity issuer). In local dev / CI against a Triton with no issuer
configured, use the literal `Bearer dev-token` (→ `references/07`).
When `TRITON_OIDC_ISSUER` is set, dev-token is rejected and you must
present a real JWT.

## Chat-channel callers

If your "client" is actually a chat platform (Telegram, Discord,
etc.), you don't call Triton's HTTP trio — the platform delivers
webhooks/socket events to Triton's chat adapters, and the surface
mapper handles presentation. That path is operator-configured via
`adapter.yaml` (→ `references/03`, `references/05`), not something a
client author wires up call-by-call.

## Testing your client

Spin a real Triton with the consumer harness and point your client at
its `rest_url()` / `mcp_url()` / `a2a_url()`. Register a `FakeAgent`
behind a `FakeConsul` so a full `frontend → triton → app-agent`
round-trip runs in your CI with no mocks (→ `references/08`).
