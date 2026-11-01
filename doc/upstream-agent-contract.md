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

Triton also registers its own in-process tools (`echo`, `narrate`,
`help`, and in a `dev-token` build the demo surfaces such as
`demo_panel`), and `tools/list` shows them next to the upstream's. A
public gateway in front of one agent sets `TRITON_BUILTIN_TOOLS=off`:
no in-process tool is registered, so MCP clients see only the agent's
tools (a chat `help` command then needs an upstream named `help`). Any
value other than `on`/`off` (or `true`/`false`, `1`/`0`) fails boot.
Test: `tests/mcp_protected_resource.rs`.

## 2. The dispatch request (Triton → agent)

```
POST http://<agent-host:port>/
Authorization: Bearer <token>
X-Triton-Tool: <tool-name>
X-Triton-Channel: <channel>
Content-Type: application/json

<tool args JSON object, plus Triton's reserved `_triton` key>
```

- **Path is always `/`** — agents own any routing inside themselves.
- **`X-Triton-Tool`** names the invoked tool, in both dispatch modes
  (parity pinned by `tests/static_upstream.rs`). It is informational:
  authorisation derives from the bearer, never from this header.
- **`X-Triton-Channel`** names the channel the turn will be rendered on
  (`whatsapp`, `msteams`, `googlechat`, `rest`, `mcp`, `a2a`, …). Also
  informational (§2b).
- **The body is the tool args**, plus one reserved key, `_triton`,
  that Triton owns (§2b). No envelope, and **no sender-identity fields
  are injected**: identity travels exclusively in the bearer (§3). For an HTTP-trio call (`POST /v1/tools/<name>`,
  MCP, A2A) the args are the client's request body passed through. For
  a chat inbound, the args are whatever the adapter's command parser
  built — plain text becomes `{ "message": "<text>" }` (see §6 for the
  current routing limits).
- **Timeout:** `TRITON_UPSTREAM_TIMEOUT_MS` (default 10 000 ms) per
  buffered call. Slow agents are reported as `upstream <tool> timed out`.
  Spec A2A, MCP calls to an upstream tool and Teams dispatch as streams
  (idle limit `TRITON_STREAM_IDLE_TIMEOUT_MS`, 60 s); a call that needs
  longer goes asynchronous (§7).
- **Egress allowlist (NFR-S-4):** outside `env=local`, every mapped
  `host:port` endpoint must pass an SSRF guard or the binary refuses to
  boot. IP literals must be loopback / RFC-1918 private / CGNAT
  (100.64.0.0/10) / IPv6 ULA (`fc00::/7`); public + metadata
  (`169.254.169.254`) addresses are refused. **Hostnames** are trusted
  only under a permitted DNS suffix — by default exactly `.ts.net`
  (Tailscale MagicDNS). `TRITON_EGRESS_ALLOWED_SUFFIXES` (comma-separated,
  e.g. `.ts.net,.int.data-zoo.de`) is the explicit operator opt-in to
  also trust a private/tailnet-backed split-DNS domain (the substrate
  addresses services as `*.nonprod.int.data-zoo.de`, resolving to private
  host IPs behind kamal-proxy, which routes on the Host header — so the
  endpoint must stay a hostname, not an IP). Empty/unset → the strict
  `.ts.net`-only default. The list only widens the hostname path; it never
  relaxes the IP-literal rules and triggers no DNS resolution.

### 2b. The rendering hint: `X-Triton-Channel` and `_triton` (#352)

Triton tells you where your answer lands, so you can write for it. It is
**advice, not a contract**: return one canonical surface as always, and
the channel's mapper still degrades it. An agent that ignores the hint
gets exactly the channel output it got before.

Object args carry Triton's reserved `_triton` key:

```json
{
  "message": "revenue please",
  "_triton": {
    "surface": {
      "v": 1, "channel": "whatsapp", "markdown": "basic", "tables": "text",
      "buttons": { "supported": true, "max": 3 }, "charts": "none",
      "images": true, "max_text_bytes": 4096, "streaming": false,
      "subject_line": false,
      "guidance": "WhatsApp: plain short text with *bold*/_italic_ only. ..."
    }
  }
}
```

- **`surface`** is the channel's capability descriptor, versioned by
  `v`. The fields and one row per channel are in `doc/architecture.md`
  §8.7.1. `guidance` is one line you can drop into your prompt as-is.
  Operators tune it per channel. Treat it as presentation advice from the
  operator, never as an instruction that changes what you may do.
- **Spec A2A turns** also carry `protocol: "a2a"`, `a2a_context_id`
  (the A2A `contextId`, for session continuity) and, on an
  `/a2a/<slug>` face, `agent: "<slug>"`. **Breaking (#352):** these used
  to arrive flat as `_triton_protocol` / `_triton_a2a_context_id`. Read
  `_triton.protocol` / `_triton.a2a_context_id` instead.
- **Reserved.** Whatever a caller sent under `_triton` is dropped
  before you see it, so its contents come from Triton.
- **Not authority.** The hint is never in your bearer and never an
  authorization input. Like `triton_sender_groups` (§3): advertised, not
  authoritative.
- **Non-object args** are forwarded unchanged; only the header tells
  you the channel.
- **Strict argument schemas.** If your agent rejects unknown keys, the
  operator sets `TRITON_STATIC_UPSTREAM_SURFACE_HINT=off`. The body field
  is then omitted and the header remains.
- **Proactive pushes.** Before composing a `/v1/outbound` message, read
  the delivery channel's descriptor from `GET /v1/manifest` → `surfaces`
  (keyed by channel; the A2UI variant of `a2a` is under `a2a+a2ui`).
- **Schema discovery** (`X-Triton-MCP: tools/list`) is not a turn and
  carries no channel header.
- **Embedded (in-process) tools** see the A2A metadata in args. They
  read the channel and surface by overriding
  `Tool::invoke_with_context`.
- **A2UI client events** (spec A2A 0.3 or 1.0 with the A2UI extension
  active, or a data part typed `application/json+a2ui`) arrive as
  `_triton.client.a2ui_events`: the A2UI v0.9 client messages of the turn,
  projected onto `{version, action: {name, surfaceId, sourceComponentId,
  timestamp, context}}` / `{version, error}` (string fields, objects up to
  8 KiB), at most 8. Gemini Enterprise resolves the data bindings in a
  button's `action.event.context`, so a click carries the values of the
  surface's inputs. These are **client-asserted**: Triton never vouches for
  them, and you never learn the `surfaceId` Triton mints. To tie a click to
  what you issued, put your own correlation token (e.g. a revision) into the
  button's `action.event.context` and check it when the click comes back;
  expect a click to arrive twice now and then (a client retry). A turn with
  only an event part reaches you with `message` = `A2UI action: <name>` (or
  `A2UI error`). Clicks on Triton's own cards are signed actions that
  Triton resolves: the signed message (a form's template filled from the
  click's context) becomes `message`, and a submit missing a required choice
  is answered without a turn. Such a click is still listed in
  `a2ui_events`; only producer surfaces (§4) rely on events.
- **Client files** (spec A2A 0.3 or 1.0) arrive as `_triton.client.files`,
  one entry per file: `{source, name?, media_type?, size, bytes}` with the
  content as standard base64, or `{source, name?, media_type?, error}`.
  `source` is `part` (a file part: 0.3 `{kind: "file", file: {bytes,
  mimeType, name}}`, 1.0 `{raw, mediaType, filename}`) or `file_upload` (the
  data URIs of an A2UI `FileUpload` action, `context.files[].inlineData`;
  the event stays in `a2ui_events` with each `inlineData` replaced by
  `file`, the index of its entry). `error` is `link` (a `uri`/`url` part:
  Triton never fetches links), `invalid` (not base64), `too_large` (over
  10 MiB, or over 25 MiB for the turn) or `too_many` (more than 10). A turn
  with only files reaches you with `message` = `Attached file: <name>`.
  Gemini Enterprise converts chat attachments before any agent sees them:
  Word and PowerPoint arrive as unnamed `text/plain` (markdown) parts,
  Excel as one named CSV per sheet, PDF/CSV/TXT/images unchanged. The
  original Office file only arrives through a `FileUpload` you render.
  Names and types are client-asserted; check the bytes yourself. An
  upstream agent must accept dispatch bodies up to about 40 MB.

### 2a. Optional: argument-schema discovery (`X-Triton-MCP: tools/list`)

Triton can't know your tool's argument schema from the static-upstream map
alone, so `GET /v1/tools` would otherwise report an empty `input_schema` for
an upstream agent — and a client (the Explorer) can't build an input form.
To fix that, answer a **`tools/list` introspection**: Triton POSTs `/` with
`X-Triton-MCP: tools/list` (an empty `{}` body) and expects:

```json
{ "tools": [ {
    "name": "<tool>",
    "description": "<what the tool does, for a model choosing it>",
    "annotations": { "readOnlyHint": true },
    "inputSchema": { "type": "object", … }
} ] }
```

Triton folds the returned `inputSchema` into `GET /v1/tools` (as
`input_schema`) and MCP `tools/list` for the matching tool name. A string
`description` and an object `annotations` (the MCP tool-annotation fields) are
passed through verbatim to both listings; when absent, the listings omit the
key. An MCP client's model chooses tools by `description`, so an upstream
exposed over MCP should send one. It is **best-effort and public**: discovery never fails the
listing, so you may answer it without verifying the bearer (return only the
schema, never data), and an agent that doesn't implement it simply keeps the
empty schema (clients fall back to a free-text composer). Optional but
recommended — it's a few lines: return your `Tool::input_schema()`.

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
| `triton_sender_scopes` | Absent by default. **With `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true` (#110/#114):** the resolved sender's scopes as a **JSON array** under this private claim — deliberately NOT the standard OAuth2 `scope`, since it's resolver-derived *authentication* data, not authorization Triton granted; a downstream must opt in to read it. Values are sanitised + capped, and intersected with `TRITON_STATIC_UPSTREAM_SCOPE_ALLOWLIST` when set. Omitted when empty |
| `triton_sender_groups` | Absent by default. **With `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true`:** the resolved sender's group memberships as a **JSON array** under this private claim — deliberately NOT `roles`, which a downstream like escurel derives admin from, so a resolver-supplied value can never escalate. Sanitised + capped like scopes, and intersected with `TRITON_STATIC_UPSTREAM_GROUP_ALLOWLIST` when set. Omitted when empty. Consume it by pointing your groups claim at `triton_sender_groups` |
| `idp` / `idp_tenant` / `email` | Absent by default. **With `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true`:** the identity provider that vouched for the caller — `idp` ∈ `google` (opaque Google access token, Gemini Enterprise), `entra` (Entra access token, or the Teams bot's `azure` identity), `oidc` (the deployment's own `TRITON_OIDC_ISSUER`); `idp_tenant` = the Google Workspace `hd` or the Entra `tid` (absent for a generic issuer); `email` = the verified email, lower-cased (Entra: `email`, else `upn`, else `preferred_username`; absent for Teams activities, which carry none). Validated like `sub` before signing |
| `iat` / `exp` | now / now + TTL, TTL clamped to **≤ 300 s** |

Notes for agent authors:

- **By default `sub` is the only per-sender claim.** The sender's
  resolved `scopes`/`tenant` are not carried unless the operator sets
  `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true` (#110/#114), which adds a
  `triton_sender_scopes` array (sanitised, capped, optionally allowlisted
  — NOT a standard OAuth `scope`, so don't treat it as granted authority)
  and sources `tenant` from the resolved sender. Off → key your own
  lookup off `sub`. The flag applies to the
  signed static-upstream path only; the Consul/Vault path carries no
  sender identity (it mints a Triton-workload token).
- **Diagnosing "my agent sees no groups".** Forwarding-off is silent from
  the caller's side — the call authenticates and a group-filtering agent
  simply returns nothing, which looks exactly like a broken filter in the
  agent. Triton says which it is, in its own log:
  - once at startup, `static-upstream principal forwarding decision`
    with `forward_principal`, `scope_allowlist_size`,
    `group_allowlist_size` (and the matching `*_enforced` flags, since
    "no allowlist" and "an empty allowlist" both read as size 0);
  - once per process at the moment memberships are actually dropped, a
    `warn` naming `dropped_groups` (the count only — never the values).
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
- Optional top-level `text` and `summary` (strings). `text` is the whole
  answer for a host that shows text only. `summary` is one sentence that
  says what the answer is; where Triton renders the surface as A2UI (Gemini
  Enterprise), the text part beside it is `summary`, so rows, choices and
  facts appear once, in the components. Without `summary` the text part is
  `text` (or the surface's prose).
- A card reads best in one anatomy: a `header` (title, one `context` line,
  a short `stamp`), one body component (table, key/value, form or
  `form_groups`), then one `primary` button and at most two others.
- **Producer-built A2UI v0.9** (Gemini Enterprise only): put native
  surfaces in `surface.a2ui`, a sibling of `components` that every other
  channel ignores, so keep the prose in a `text` component:

```json
{ "surface": {
    "components": [ { "kind": "text", "value": "Three close calls." } ],
    "a2ui": [ {
      "components": [ { "id": "root", "component": "MaterialCard", "children": ["b"] },
                      { "id": "b", "component": "MaterialButton", "label": "Apply",
                        "action": { "event": { "name": "apply",
                                    "context": { "rev": 7, "pick": { "path": "/pick" } } } } } ],
      "data_model": { "pick": "a" },
      "theme": { "agentDisplayName": "Coverage Matching" } } ] } }
```

  Each surface (at most 8) goes out verbatim as `createSurface` +
  `updateComponents` (+ `updateDataModel`) ahead of Triton's own card;
  `components` must contain `root`; `theme` may set `agentDisplayName` and
  `primaryColor` (`#rrggbb`). A click on a producer's button comes back as a
  client event (§2b); the producer handles it. Triton does not vet the
  components: passthrough is for upstreams the operator trusts like its own
  code. Producer surfaces only go to clients that negotiated A2UI; other
  channels see `components`. Measured in Gemini Enterprise (2026-10-31):
  Material cards, tables, chips, selects, checkboxes, progress bars,
  VegaChart and Canvas side panels render, and inputs and buttons inside a
  Canvas work like inline ones; `IFrameSrcdoc` does not render;
  `createSurface.sendDataModel` is not honoured.
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
- **Minted-token claims**: `sub` and `tenant` always; scopes, groups and
  the identity-provider claims only with
  `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true` (§3).
- **Proactive sends** are a different surface: `POST /v1/outbound`
  (#95) with a dedicated audience — and, once #100 lands, a dedicated
  issuer so the agent can sign its own outbound tokens (mirror-image
  static-upstream signing). **Authorization (#113):** the token MUST
  carry the `outbound:send` scope, and the recipient (`to`) MUST belong
  to the caller's `tenant` — for `sender_table` adapters Triton checks
  `to`'s tenant against the table, so an agent can only message its own
  tenant's users (cross-tenant → `403`). For `identity.kind: upstream`
  adapters there is no static table, so Triton trusts the caller's
  verified `tenant` (the agent owns its tenant's users) — a weaker,
  deliberate posture gated by the audience + scope.

## 7. Long-running calls: the asynchronous task protocol (ADR-20)

A call that takes longer than a caller waits (seconds to about eleven
minutes) is answered as a **task**. Triton offers streaming on every A2A,
MCP and Teams dispatch to an upstream (`Accept: text/event-stream`; the
REST trio when the client asks for SSE). An agent that wants to go
asynchronous answers `200` with `Content-Type: text/event-stream` and
these events (`event:` name + one-line JSON `data:`, blank-line separated):

| Event | `data` | Meaning |
|---|---|---|
| `task` | `{"task_id": "<id>", "check_after_seconds"?: n, "receipt"?: "<text>", "note"?: "<text>"}` | The call is now the agent's task `task_id` (≤ 200 chars, opaque to Triton). Sent first. `receipt` is the hand-over text for a caller that stops waiting ("filed, still running, the result will be in …"; mock 1k); on Teams it is the working card's title. `note` is one line under the working card's steps. |
| `progress` | `{"text": "<step line>", "step"?: "<id or n/m>", "percent"?: 0–100}` | A real step, shown as it arrives (mock 1j). Any number, any time. |
| `done` | the result — the canonical surface envelope (§4), as a buffered reply would be | Terminal success. |
| `error` | `{"message": "<text>"}` | Terminal failure; `message` is shown to the caller. Not retried. |

Rules:

- **Keep the connection alive.** Triton cuts a stream that is silent for
  `TRITON_STREAM_IDLE_TIMEOUT_MS` (default 60 s) or older than
  `TRITON_STREAM_MAX_DURATION_MS` (default 600 s). Send a `progress` event
  or an SSE comment line (`: ping`) at least every 30 s. A cut stream is
  not a failure once `task` was sent: Triton resumes (next rule).
- **Resume.** To re-attach to a task — after the stream broke, the
  agent closed it, or Triton restarted — Triton sends
  `POST /` with the usual `Authorization`, `X-Triton-Tool` and
  `X-Triton-Channel`, plus **`X-Triton-Task: <task_id>`** and
  `Accept: text/event-stream`, and the body `{"_triton": {…}}` (no tool
  args). The agent answers the same stream for that task **from its
  current state**: `task` again, the latest `progress`, then the rest
  until `done`/`error` — or at once `done`/`error` if it already ended.
  `404` (or an `error` event) means the task is unknown. A JSON `2xx` body
  instead of SSE is taken as the `done` result. A resume must never start
  the work again.
- **Identity on resume.** Only the task's OWNER drives a resume (the
  original caller inside the same Triton process; after a restart, the
  owner's own `tasks/get` / resubscribe). Ownership is the verified
  issuer, subject and tenant: a colleague in the same tenant does not see
  the task, and an audit operator reads it without resuming it. The bearer
  carries the same claims as the original dispatch, and the body the same
  `_triton` metadata. Still check that the resuming subject owns the task.
- **Idempotency.** Triton never re-sends a request it has a `task_id`
  for; it resumes. A stream that breaks *before* `task` fails the call (it
  cannot know whether the agent filed it).
- **Buffered agents are unaffected.** A JSON reply, or an SSE stream of
  `token`/`tool`/`done` without `task`, behaves as before.

What each caller sees:

| Surface | While running | When done |
|---|---|---|
| A2A 0.3 `message/stream` | `status-update` `{state: working, final: false, message: <progress text>, metadata: {step}}` | `artifact-update`, then `status-update completed, final: true` |
| A2A 1.0 `SendStreamingMessage` / `POST /v1/message:stream` | `statusUpdate` `{status: {state: TASK_STATE_WORKING, message: <progress text>}, metadata: {step, percent}}` | `artifactUpdate`, then `statusUpdate TASK_STATE_COMPLETED` |
| A2A blocking send (both versions) | after `TRITON_A2A_BLOCK_SECS` (60): the working task, its status message = the `receipt`, `metadata.checkAfterSeconds` | the answer, if it ends within the cap |
| A2A `tasks/get`, `GetTask`, `ListTasks`, `tasks/resubscribe`, `SubscribeToTask` | the stored task with the latest progress; a resubscribe streams on | completed task with the answer artifact |
| MCP `tools/call` | after `TRITON_MCP_ASYNC_WAIT_SECS` (20) past `task`: `content: [{text: <receipt>}]`, `structuredContent: {status: "filed", task_id: <the agent's task_id>, check_after_seconds, receipt?}`, `_meta.trace_id` = Triton's trace, `isError: false` — the client checks back with the agent's own status tool, by the agent's id | the normal tool result, if it ends within the wait |
| Teams (`TRITON_MSTEAMS_ASYNC`) | typing, then ONE Adaptive Card: the receipt as title with the elapsed time beside it, the real steps (done, current), the note; updated in place at most once a second and at least every 10 s, with a Universal Actions `refresh` that returns its current state. A card click turns the clicked card into this working card (no second card, no banner) | the same activity replaced by the answer (`updateActivity`) |

Tasks are stored by Triton (`TRITON_TASK_DB`, SQLite; memory when unset)
for `TRITON_TASK_TTL_SECS` (default 1 day). A task still running after
`TRITON_TASK_MAX_SECS` (default 3600) is failed as expired. Client streams
end after `TRITON_A2A_STREAM_MAX_SECS` (default 660) without a terminal
event; the client resubscribes or polls.

Tasks are bounded by `TRITON_TASK_MAX_SECS` from filing — the open, every
wait for the next event and the resume budget (resumes back off up to 8 s
and continue until the deadline). An oversized frame fails the task.
Only asynchronous MCP calls are stored, and A2A `ListTasks` lists A2A
tasks only.

Teams turns run through the same task runner and store (`triton-tasks`):
an asynchronous Teams task is stored with its delivery target (the verified
conversation reference and the card's activity id), resumed when its stream
breaks or is cut (`TRITON_STREAM_MAX_DURATION_MS` no longer caps it; the
deadline is `TRITON_TASK_MAX_SECS`), and resumed at boot after a restart,
answering into the same card. If the final `updateActivity` is refused,
the answer is posted as a new message.
The task DB is credential-bearing (realizations §7: 0600, never shared
between live processes). At boot, only Teams tasks are swept (they have a delivery target); an A2A
task left running by a stopped Triton is driven again when its owner reads
it, and MCP-filed tasks are never resumed by Triton (the client asks the
agent).

Pinned by `tests/async_tasks.rs` and `tests/msteams_async_task.rs` against
`triton_tests::async_agent_fixture::FakeAsyncAgent`.
