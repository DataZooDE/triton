# Upstream-agent wire contract

Consumer-facing, normative description of what a registered upstream
agent receives from Triton on a dispatch, how the **sender's identity**
reaches it, what it must return, and how the `upstream`
identity-resolution strategy (FR-I-7) invokes a resolver tool. Written
for agent authors (issue #101); the in-repo reference implementation is
`examples/adk-hello-agent`.

Everything here is pinned by integration tests in
`crates/triton-tests/` — file references are given per section. If this
document and the code disagree, the code + tests win; please file an
issue.

## 1. Dispatch modes

Triton reaches an upstream agent in one of two ways; the wire shape the
agent sees is the same in both:

| Mode | Discovery | Bearer minting |
|---|---|---|
| **Consul router** (`TRITON_CONSUL_URL` + Vault) | Consul service tagged `agent:<name>` | Vault OIDC swap per call (`agent-oidc-swap` role) |
| **Static upstream** (`TRITON_STATIC_UPSTREAMS=name=host:port,…`) | env-configured map, no Consul/Vault | Triton-signed RS256 JWT per call (when the static signer is configured), else the static dev bearer |

Code: `crates/triton-upstream/src/lib.rs` (router),
`crates/triton-upstream/src/static_upstream.rs` (static). Tests:
`tests/upstream.rs`, `tests/static_upstream.rs`.

## 2. The dispatch request (Triton → agent)

```
POST http://<agent-host:port>/
Authorization: Bearer <token>
X-Triton-Tool: <tool-name>
Content-Type: application/json

<tool args, verbatim JSON object>
```

- **Path is always `/`** — agents own any routing inside themselves.
- **`X-Triton-Tool`** names the invoked tool, in both dispatch modes
  (parity pinned by `tests/static_upstream.rs`). It is informational:
  authorisation derives from the bearer, never from this header.
- **The body is the tool args, verbatim.** No envelope, and **no
  sender-identity fields are injected** — identity travels exclusively
  in the bearer (§3). For an HTTP-trio call (`POST /v1/tools/<name>`,
  MCP, A2A) the args are the client's request body passed through. For
  a chat inbound, the args are whatever the adapter's command parser
  built — plain text becomes `{ "message": "<text>" }` (see §6 for the
  current routing limits).
- **Timeout:** `TRITON_UPSTREAM_TIMEOUT_MS` (default 10 000 ms) per
  call. Slow agents are reported as `upstream <tool> timed out`.

## 3. Sender-identity carriage: the bearer, and only the bearer

In signed static-upstream mode (the substrate default — the Consul
router's Vault-minted token is analogous) every dispatch carries a
fresh RS256 JWT minted by Triton itself
(`crates/triton-identity/src/signer.rs`):

| Claim / header | Value |
|---|---|
| `kid` (header) | `TRITON_JWT_KID` (default `triton-static`) — must match a key in the served JWKS |
| `iss` | `TRITON_SELF_ISSUER` — Triton serves discovery at `<iss>/.well-known/openid-configuration` and JWKS at `<iss>/.well-known/jwks.json` |
| `aud` | `TRITON_STATIC_UPSTREAM_AUD` (default `agents-<env>`); comma-separated config becomes a **multi-audience array** so the agent can forward the same token to a named downstream (e.g. `agents-nonprod,escurel-nonprod`) — each hop pins its own audience |
| `sub` | **`Principal.sub` — the resolved sender.** For a chat inbound this is the subject the adapter's identity strategy produced (e.g. the `sender_table` mapping for the sender's `wa_id`); for a trio call it is the verified API caller's subject |
| `tenant` | **Default:** `TRITON_STATIC_UPSTREAM_TENANT` when set, else absent — deployment-level config, *not* the per-sender tenant. **With `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true` (#110):** the resolved sender's `Principal.tenant` instead |
| `scope` | Absent by default. **With `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true` (#110):** the resolved sender's scopes, space-delimited (OAuth2 `scope`); omitted when the sender has none |
| `iat` / `exp` | now / now + TTL, TTL clamped to **≤ 300 s** |

Notes for agent authors:

- **By default `sub` is the only per-sender claim.** The sender's
  resolved `scopes`/`tenant` are not carried unless the operator sets
  `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true` (#110), which adds a
  space-delimited `scope` claim and sources `tenant` from the resolved
  sender. Off → key your own lookup off `sub`. The flag applies to the
  signed static-upstream path only; the Consul/Vault path carries no
  sender identity (it mints a Triton-workload token).
- Verify the token like any OIDC bearer: fetch JWKS from the issuer,
  check `iss`, your `aud`, `exp`, and the algorithm. Recipe:
  `examples/adk-hello-agent/src/main.rs` (`verify_bearer`).
- Never log the raw token; treat it as a credential. With a
  multi-audience token, forward it **only** to the downstream(s) named
  in `aud`.

## 4. The response (agent → Triton)

- Reply `2xx` with a JSON body. Any non-`2xx` surfaces as
  `upstream <tool> returned <status>` (`TritonError::Tool`); the body
  is not parsed for errors.
- The body is either a **canonical A2UI envelope** or raw JSON. Tools
  reached via the upstream router are assumed `returns_a2ui = true`
  (`crates/triton-core/src/tool.rs`, `ToolDescriptor`), so emit the
  envelope unless you have a reason not to:

```json
{
  "surface": {
    "components": [
      { "kind": "narration", "text": "…" },
      { "kind": "button", "label": "…", "tool": "<tool>", "args": { } }
    ]
  }
}
```

- Emit the **canonical** surface, not a versioned wire shape — Triton
  builds the negotiated A2UI version and the per-platform chat
  rendering from it (`degrade` rules in `adapter.yaml`).
- Triton owns the dispatch/post audit pair (ADR-6). Agents emit their
  own logs but must not assume their response body is audited.

## 5. The `upstream` identity-resolution strategy (FR-I-7)

For senders the adapter cannot resolve locally, FR-I-7's `upstream`
strategy delegates resolution to a **resolver tool** the agent itself
serves, reached through the upstream router. This is implemented today
in the **Google Chat adapter** (`crates/triton-chat-googlechat`);
the other chat adapters accept only `sender_table` so far (§6).

**Manifest declaration** (`adapter.yaml`):

```yaml
adapters:
  google_chat:
    identity:
      kind: upstream
      credentials:
        resolver_tool: <tool-name>     # Vault ref or literal (dev)
```

Boot-time rules (`triton-chat-googlechat/src/lib.rs`,
`crates/triton-manifest`):

- `identity.credentials.resolver_tool` is required and must be
  non-empty.
- The resolver **must be an upstream tool**: if its name collides with
  an in-process tool, Triton refuses to boot (otherwise the dispatch
  would silently run locally and bypass the upstream auth path).

**Invocation** — a separate dispatch *before* the message dispatch,
once per inbound from an unresolved sender:

- Request args:

```json
{ "platform": "google_chat", "sender": "<platform sender id>" }
```

- The resolve call runs under a bootstrap principal
  (`sub: "identity-resolver"`, `scopes: ["resolve"]`,
  `tenant: "system"`) and audits under its own protocol label
  (`messenger:google_chat:identity`), with its own `trace_id` —
  distinct from the message's audit pair.

- Expected response — the resolved principal claims:

```json
{ "sub": "<subject>", "scopes": ["…"], "tenant": "<tenant>" }
```

  `scopes` may be omitted (defaults empty); `sub` and `tenant` must be
  non-empty.

- **Failure semantics:** resolver error, timeout, malformed reply, or
  empty `sub`/`tenant` → the inbound is rejected `401` ("identity
  resolution failed"), an audit rejection is recorded, and **no message
  dispatch happens**.

Pinned by
`tests/google_chat.rs::upstream_identity_resolves_principal_via_resolver_tool`
(and the boot-refusal test in the same file).

## 6. Current limits (so you don't design against them)

- **Chat → tool routing is not yet manifest-configurable.** The chat
  adapters route via a built-in command parser: plain text dispatches
  the in-process `echo` tool with `{ "message": "<text>" }`, `/narrate`
  etc. route to the in-repo demo tools. Routing a chat turn to an
  arbitrary upstream agent tool (e.g. every WhatsApp message → your
  agent) needs an adapter-level default-tool setting that does not
  exist yet — file/track upstream before relying on it.
- **Identity strategies per adapter:** `google_chat` supports
  `sender_table`, `self_enrol`, and `upstream`; `telegram`, `whatsapp`,
  `discord`, and `signal` accept `sender_table` only; MS Teams uses
  `azure`.
- **Minted-token claims** carry `sub` only for the sender (§3): no
  per-sender `scopes`/`tenant`.
- **Proactive sends** are a different surface: `POST /v1/outbound`
  (#95) with a dedicated audience — and, once #100 lands, a dedicated
  issuer so the agent can sign its own outbound tokens (mirror-image
  static-upstream signing).
