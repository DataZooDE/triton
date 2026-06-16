# 06 — Calling Triton from a frontend / client

This is the *caller* side: you're building an MCP host, an A2A peer,
or a plain REST/SDK client that drives tools through Triton. You hit
one stable public URL (`agents.<env>.<domain>` on `:443`, the
substrate's ingress proxy in front) and get back A2UI in the wire
format you used. Source: FR-A-3..7,
`doc/requirements.md` §5.1.

## Bootstrap discovery (anonymous)

`GET /v1/runtime` needs **no auth** and returns what a SPA needs before
login: `{ env, image_sha, package_version, binary_sha, oidc_issuer,
oidc_audience, oidc_client_id }`. When `oidc_issuer` is null the gateway
runs without OIDC (dev-token / sidecar mode); when set, drive a PKCE
login against that issuer. `GET /healthz` (also anonymous) is a liveness
check. `GET /version` returns build metadata.

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

The body wraps the A2UI envelope under `result`:
`{ latency_ms, trace_id, result: { "version": "0.9", "stream": [...] } }`.
**Unwrap `result` before rendering** — a client that reads top-level
`version`/`stream` mis-renders (→ `references/02` for the gotcha and the
per-protocol read paths). Omit the `Accept` header and `result` is the
tool's raw JSON instead of a `{version, stream}` surface.

## MCP

Streamable HTTP, JSON-RPC 2.0 over HTTP, plain JSON responses (SSE
not required). Methods: `initialize`, `tools/list`, `tools/call`,
`resources/read` (FR-A-6).

- `initialize` negotiates a `protocolVersion` (rejecting unknown ones)
  and advertises `capabilities: { tools, resources }` + `serverInfo`.
- `tools/call` returns `{ content: [{type:"text",…}], structuredContent,
  isError, _meta }`. The A2UI envelope is at
  **`result.structuredContent.result`** (`{version, stream}`); the trace
  is at `result._meta.trace_id`. Set the version per call via
  `params._meta.a2ui_version: "v0.9"`.
- `resources/read` of `ui://triton/runtime.html` is a **stub** on the
  substrate deployment — don't depend on it serving a real Lit runtime.

## A2A

`POST /message:send` with `Message{parts:[Part{data:{tool, args}}]}`;
request v0.9 via `Message.metadata.a2ui_version: "v0.9"` (FR-A-7). The
response part carries the dispatcher envelope at **`parts[0].data`**
(so the A2UI surface is `parts[0].data.result`), and `metadata` carries
`{ trace_id, task_state }`. `task_state` is `"completed"` on success;
on error the part is replaced by `metadata: { error, message, trace_id }`
with **no** `task_state`. Triton tracks state in an `InMemoryTaskStore`
— restart-clean, so don't rely on task durability across restarts.

## Error model

All three protocols map the same dispatcher error variants
(`doc/architecture.md` §8.3; `crates/triton-core/src/error.rs`):

| Variant | REST | A2A `metadata.error` | MCP code |
|---|---|---|---|
| Auth | 401 | `auth` | `-32001` |
| Validation | 400 | `validation` | `-32602` |
| Tool | 502 / 504 timeout / 503 `circuit_open` | `tool` | `-32000` |
| Provider | 502 | `provider` | `-32000` |
| RateLimited | 429 | `ratelimit` | `-32002` |

REST/A2A failures ride the HTTP status (REST body
`{ error: <class>, message, trace_id }`); MCP rides the JSON-RPC code
in an HTTP-200 envelope, so inspect `result.error.code`, not the HTTP
status. Handle `503 circuit_open` as "this tool is temporarily down,
others are fine" — it's per-tool, not gateway-wide (FR-U-4).

## Auth — three inbound modes

Triton authenticates the caller one of three ways, in this strict
precedence (`crates/triton-adapters-http/src/identity.rs`):

1. **OIDC bearer (production).** When `TRITON_OIDC_ISSUER` is set, the
   *only* accepted credential is a valid substrate JWT
   (`Authorization: Bearer <jwt>`). dev-token and the sidecar header
   below are both rejected in this mode — a stale trust flag can never
   override real PKCE.
2. **Forwarded-auth sidecar.** When no issuer is configured **and**
   Triton is booted with `TRITON_TRUST_FORWARDED_AUTH=true`, it trusts
   an `X-Forwarded-Email` header injected by a **co-located
   oauth2-proxy sidecar** (ADR-0011 / issue #67). The browser never
   sends a bearer — it authenticates to the sidecar via SSO and the
   sidecar forwards the header on loopback inside the same container /
   netns. The synthesized principal is `{ sub: <email>, scopes:
   ["sso-ops"], tenant: "ops" }` with **no** raw token, so
   upstream-agent routing (the per-call RS256 mint) is unavailable on
   this path — it's for in-process / demo tools. This is exactly how
   the explorer + API
   deploy on the substrate. Only safe because Triton binds loopback and
   only the sidecar shares its netns; never enable it on a
   publicly-bound listener.
3. **dev-token (local / CI).** With no issuer and no trust flag, the
   literal `Bearer dev-token` maps to a fixed dev principal
   (→ `references/07`).

### Cross-origin browser SPAs (CORS)

A browser app served from a different origin than the REST adapter
needs Triton booted with `TRITON_CORS_ALLOWED_ORIGINS` listing the
SPA's exact `scheme://host[:port]` (comma-separated; `*` is refused).
Triton then echoes `Access-Control-Allow-Origin` and 204s the
`OPTIONS` preflight. It also sends `Access-Control-Allow-Credentials:
true`, so to carry the oauth2-proxy session cookie cross-origin set
`credentials: "include"` (fetch) / `withCredentials = true` (Dio web).
With the list unset (default) **no CORS layer is mounted** —
production parity. Allowed methods: GET/POST/OPTIONS; allowed headers:
`authorization`, `content-type`, `accept`
(`crates/triton-adapters-http/src/cors.rs`).

## Chat-channel callers

If your "client" is actually a chat platform (Telegram, Discord,
etc.), you don't call Triton's HTTP trio — the platform delivers
webhooks/socket events to Triton's chat adapters, and the surface
mapper handles presentation. That path is operator-configured via
`adapter.yaml` (→ `references/03`, `references/05`), not something a
client author wires up call-by-call.

## Testing your client

Spin a real Triton with the consumer harness and point your client at
its `rest_url()` / `mcp_url()` / `a2a_url()`. Start a `FakeAgent` and
name it in `TRITON_STATIC_UPSTREAMS` so a full
`frontend → triton → app-agent` round-trip runs in your CI with no
mocks (→ `references/08`).
