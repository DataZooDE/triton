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
  content as standard padded base64, or `{source, name?, media_type?,
  error}`. `source` is `part` (a file part: 0.3 `{kind: "file", file:
  {bytes, mimeType, name}}`, 1.0 `{raw, mediaType, filename}`) or
  `file_upload` (the data URIs of an A2UI `FileUpload` action,
  `context.files[].inlineData`; the event stays in `a2ui_events` with each
  `inlineData` replaced by `file`, the index of its entry). `error` is
  `link` (a `uri`/`url` part: Triton never fetches links), `invalid` (not
  standard base64, or no content) or `too_large` (over 10 MiB, or over the
  turn's 25 MiB). Files are admitted in the order their parts arrive; at
  most 10 entries, errors included, and the rest are counted in one last
  entry `{error: "too_many", count}` (an upload past the limit has no `file`
  index; an upload event whose context is still over 8 KiB keeps only
  `files: [{fileId, file}]`). Uploads in events past the 8-event cap are not
  seen. A turn with only files reaches you with `message` = `Attached
  file: <name>` (or `Attached files: <names>`). Files go with the first
  dispatch only and are never stored with an A2A task: a resumed task (a
  broken stream, or a gateway restart) runs without them.
  Gemini Enterprise converts chat attachments before any agent sees them:
  Word and PowerPoint arrive as unnamed `text/plain` (markdown) parts,
  Excel as one named CSV per sheet, PDF/CSV/TXT/images unchanged. The
  original Office file only arrives through a `FileUpload` you render.
  Names and types are client-asserted; check the bytes yourself. A2A
  request bodies may be up to 40 MiB (read only after authentication), so
  an upstream agent must accept dispatch bodies of about that size.

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

**Your own MCP App.** A tool whose results your own MCP App renders declares
it in the descriptor's `_meta` (MCP Apps 2026-01-26, Resource Discovery):

```json
{ "name": "render_report", "_meta": {
    "ui": { "resourceUri": "ui://<authority>/views" },
    "openai/outputTemplate": "ui://<authority>/views" } }
{ "name": "report_refresh", "_meta": {
    "ui": { "resourceUri": "ui://<authority>/views", "visibility": ["app"] },
    "openai/visibility": "private" } }
```

A descriptor with a `ui` object keeps it, and its `openai/*` keys, verbatim
on MCP `tools/list`, and Triton writes no answer-card widget into it; any
other `_meta` key of yours is dropped there. A tool without its own `ui`
keeps the widget. The host loads `ui://<authority>/...` through Triton's
`resources/read` proxy from the upstream that owns the authority (§8:
`read_resource`); `resources/list` names each such URI once, with the MCP
Apps mime type, for a host that reviews or prefetches resources at connection
time. Visibility is the host's business: an app-only tool stays
listed, the host keeps it from its model, and Triton dispatches it like any
other tool. The data your app needs rides the result `_meta` under a key the
operator lists per tool (§4b); an embedding host lists it with
`EmbedOpts::upstream_meta`.

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
- Optional top-level `model_context` (string): text for the **calling
  model only**, typically the identifiers of the rows the answer shows, so
  the model can name one in its next call (`get_item {id}`) while neither
  the person nor the card ever shows them. A host that renders the answer
  card gives its model only the text content (MCP Apps: `structuredContent`
  is not added to model context), and that text is then the card text, the
  row names; without `model_context` such a model cannot chain an id-based
  tool. On MCP, Triton takes it out of the result and, for a card host,
  appends it to the text content after a blank line; it never reaches the
  card or the MCP typed result. Another MCP client keeps your `text` as it
  is (a host may show it to the person) and reads your rows from the typed
  result. The other faces (REST, A2A) pass the field through in the result
  unchanged. At most 8 KiB are kept (cut at a character, marked `…`); a
  non-string value is dropped.
  It is not access control: write nothing the calling principal must not
  read.
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
  `components` must contain `root`; `data_model` (any JSON) becomes the
  `updateDataModel` value; `theme` may set `agentDisplayName` and
  `primaryColor` (`#rrggbb`). Without the producer's name a surface carries
  the deployment's (`TRITON_A2UI_AGENT_DISPLAY_NAME`, default "DataZoo
  Agent"). `catalogId` names the catalog the components
  are written for: `https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json`
  (basic) or the GE composite id
  (`https://www.gstatic.com/vertexaisearch/a2ui/v0_9/gemini_enterprise_composite_catalog.json`,
  the default when absent). Triton validates every component against the
  pinned v0.9 schema of that catalog when the answer arrives; a surface that
  fails (unknown component, missing required property, unknown catalog,
  no `root`, beyond the cap) is **dropped**, the rest of the answer still
  renders, and Triton logs a `warn` (`a2ui passthrough surface dropped`, with
  `tool`, `surface_index`, `surfaces`, `reason`, `schema_path`,
  `component_index`, `component_id`; never a value; an id that is not a
  plain identifier of at most 64 chars is logged as `<unprintable>`; every
  surface past the cap is one line) and counts
  `triton_a2ui_passthrough_dropped_total{reason}`. The check is per
  component: Triton does not verify that `children`/`child` ids resolve or
  that ids are unique, so a dangling reference still renders blank. Gemini
  Enterprise gets surfaces of either catalog (under its composite id, a
  superset of basic). MCP Apps hosts (Claude, ChatGPT) render the
  **basic-catalog** surfaces in Triton's answer-card widget, beside Triton's
  own card (the widget bundles the official renderer, which ships the basic
  catalog only; GE-composite surfaces stay Gemini-only). On MCP they ride the
  result `_meta`, never the model-visible `content`/`structuredContent`. A
  click on a producer's own event there does nothing, except an in-card event
  (`triton.in_card:<name>`, §4a); avoid `Icon`
  (it needs a font the host frame's CSP does not load). Event names starting
  `triton.` are reserved for Triton's own card: a surface whose control fires
  one is dropped (`reason="reserved_event"`), except the in-card marker
  `triton.in_card:<name>`. On MCP a producer's basic
  surfaces (and their `data_model`) go into the result `_meta` only for a
  client that declared MCP Apps (`io.modelcontextprotocol/ui` with
  `text/html;profile=mcp-app`, as Claude and ChatGPT do); every other client
  gets neither them nor Triton's card. This keeps data away from clients that
  cannot show it; it is not access control: any caller of the tool can
  declare MCP Apps, so nothing the calling principal must not see belongs in
  a surface. (An upstream's `_meta.tool_trace`, a debug sidecar, goes to every
  MCP client, card host or not.) The widget's
  producer surfaces are capped at 64 KiB in all (trailing ones dropped). A click on a producer's button comes back as a
  client event (§2b); the producer handles it. Schema validity is not a
  security review: passthrough is still for upstreams the operator trusts like its own
  code. On A2A, producer surfaces only go to clients that negotiated A2UI; other
  channels see `components`. Measured in Gemini Enterprise (2026-10-31):
  Material cards, tables, chips, selects, checkboxes, progress bars,
  VegaChart and Canvas side panels render, and inputs and buttons inside a
  Canvas work like inline ones; `IFrameSrcdoc` does not render;
  `createSurface.sendDataModel` is not honoured.
- Triton owns the dispatch/post audit pair (ADR-6). Agents emit their
  own logs but must not assume their response body is audited.

### 4a. In-card actions: re-run a read-only tool in place (#369)

On an MCP Apps host (Claude, ChatGPT) a click is normally a new user turn
(`ui/message`; Claude stages it behind a confirmation banner). For a control
that asks for **the same answer with other arguments** (a page, a filter
chip, a refresh, a read-only search) the card can instead call your tool
again and replace itself, without a turn and without the model. The rule
"a click is a complete new request, the agent never asks back" still holds:
each in-card call is a fresh, stateless `(tool, args, principal)` dispatch
that you answer completely or refuse.

**Mark the control.** On Triton's card (conversation components):

```json
{ "kind": "button", "label": "Next", "primary": true,
  "action": { "type": "rerun", "args": { "page": 2 },
              "fallback": "Show page 2 of the planned agents" } }
{ "kind": "chips", "chips": [
  { "label": "High risk", "message": "Show only the high-risk planned agents",
    "rerun": { "filter": "high", "page": 1 } } ] }
```

`args` are laid over the arguments of the call that produced the card
(top-level keys; `_triton` is never carried), so you name only what changes.
`fallback` (a chip's `message`) is a complete request: every other channel
(A2A, Gemini Enterprise, Teams, chat, text) and every host that cannot call
in place sends it as a new turn, exactly like `send_message`, and so does the
widget whenever the in-card call fails. A rerun without a sendable fallback is
no control. On a producer's own basic-catalog surface (§4), name the event
`triton.in_card:<name>`; it arrives as one entry of
`_triton.client.a2ui_events` (§2b) on a re-run of the call that produced the
surface, with the event's resolved `context`. Triton signs each in-card
event name of that answer together with the surface that fires it into the
result's token, so a client can neither invent another name nor fire one
surface's event on another; the `context` stays client-asserted.
An in-card event must not have a side effect, whatever its name. A producer
event has no fallback turn: if the host cannot call in place, or the call is
refused, the click does nothing (the widget never sends a producer's text as
the user's). Form submits are not in-card yet: a form stays `send_message`.

**What you receive.** An in-card dispatch is a normal call of your tool on
the `mcp` channel with `_triton.client.in_card: true`. It is buffered (no
`text/event-stream` offered), so it cannot become an asynchronous task (§7):
answer within the normal timeout. Answer it like any call: the result's card (and producer surfaces) replace the old ones in
place, and the widget sends the one-line card text to the model with
`ui/update-model-context`. Put the card's state into the header so the user
and the model can tell why it changed (`"context": "Page 2 of 3 · Filter:
high"`). Drill-downs ("Show agent X") change the topic: keep them
`send_message`. Anything that writes, files, drafts or proposes stays
`send_message` too.

**Conditions (all enforced by Triton, any failure is a refusal):**

- The operator declares the tool in-card capable in `adapter.yaml`
  (`tools.<name>.in_card: true`), **and** your `tools/list` (§2a) annotates
  it `readOnlyHint: true`. Replaying a read is harmless; a write must stay a
  visible turn the user confirms. Only the `tools/list` of the endpoint
  `TRITON_STATIC_UPSTREAMS` routes the tool to counts; another endpoint that
  advertises the same name neither describes nor gates it. A declared tool
  that is not annotated read-only (or whose `tools/list` did not answer) gets
  no in-card controls, only their fallbacks, and Triton logs a warning when
  the answer changes to "no"; a token for it is refused (`forbidden`) before
  it reaches you. Triton asks only when an answer has a card to sign. A yes
  is cached for 60 s, a no for 5 s, per tool.
- Each control carries a token signed with `TRITON_A2UI_ACTION_KEY` over
  `(tool, args)` (and, for producer events, the allowed `(surface, event)`
  pairs),
  bound to the tenant, the principal's `sub` and the issuer that verified it,
  valid for at most 24 h. A client cannot change the tool or the arguments, and another
  user cannot use a leaked card. There is no nonce store: a replay by the
  same principal within the expiry re-reads what that principal may read.
- Authorization is the dispatcher's, as for any call (revocation, scope
  restrictions), with your bearer minted for the same principal.
- A per-principal rate limit (10 in a burst, 2/s sustained), keyed like the
  token: tenant, issuer and `sub`.
- One audit line per call, with `in_card: {ingress: "mcp-app-event", kind,
  event, source_component_id, token, context_keys, context_bytes}`: key
  names and sizes of the context, never values.

### 4b. Result `_meta`: the chart summary and MCP pass-through (#375)

**Chart summary (`_meta.chart`, v1).** A tool that renders a chart (a
`render_report` upstream) describes its primary chart at the top-level
`_meta.chart` of its result. The description does not depend on any renderer.
Triton maps it to each channel's native chart, so the upstream does not have
to emit Vega-Lite for Triton to read back:

```json
"_meta": { "chart": {
  "kind": "bar", "title": "Revenue by region",
  "x": "region", "y": "revenue", "series": "year",
  "x_title": "Region", "y_title": "Revenue", "unit": "EUR",
  "orientation": "vertical", "stacked": false,
  "rows": [ { "region": "North", "revenue": 120.5, "year": "2025" } ] } }
```

| key | required | meaning |
|---|---|---|
| `kind` | yes | `bar` \| `line` \| `area` \| `pie` \| `donut` \| `scatter` |
| `x` | yes | row field: the category (bar/pie/donut) or the x position (line/area/scatter) |
| `y` | yes | row field: the numeric value |
| `series` | no | row field: one series per distinct value, in first-seen order; not for pie/donut |
| `rows` | yes | 1 to 200 objects, with at most 32 KiB of charted values (`x`, `y`, `series`) in total; other keys of a row are ignored and not passed on. The `x` and `series` values are strings, numbers or booleans (scatter: `x` is a number). `y` is a finite number. Nulls are not allowed: drop missing points |
| `title`, `x_title`, `y_title` | no | strings |
| `unit` | no | unit of `y`, shown as `y_title (unit)` |
| `orientation` | no | `vertical` (default) or `horizontal`; bar only |
| `stacked` | no | bar with `series` only |

Triton reads the summary from the result's own `_meta`, from
`structuredContent._meta`, or from the same two places of a `content[].text`
that holds the result as JSON (up to 1 MiB). A `_meta.chart` deeper inside the
result, for example in data the tool relays, is not the summary.

Triton ignores unknown keys. If a summary breaks any rule, Triton rejects all
of it and logs a `warn` (`ignoring the upstream's chart summary`). It then
reads the deprecated `_meta.vega_specs` (the first object in the array) and,
failing that, uses the PNG image. A valid summary always takes precedence
over `vega_specs`. `vega_specs` is read for one more release; after that,
only the summary and the image are used.

| channel | mapping |
|---|---|
| Teams | `bar` → `Chart.VerticalBar`; with `series` → `Chart.VerticalBar.Grouped` (`stacked` passed on); horizontal → `Chart.HorizontalBar`; horizontal stacked series → `Chart.HorizontalBar.Stacked`; horizontal unstacked series → `Chart.VerticalBar.Grouped` (Teams has no grouped horizontal bar). `line`, `area` → `Chart.Line`. `pie` → `Chart.Pie`, `donut` → `Chart.Donut`. `scatter` → no native chart, the PNG image. Every chart element carries the PNG as its `fallback`. |
| A2A (Gemini Enterprise) | Triton derives a Vega-Lite v5 spec from the summary for the native `VegaChart` |
| Google Chat, others | the PNG image (unchanged) |

**Upstream `_meta` on MCP: an allow-list.** An MCP `tools/call` result
carries the upstream's top-level `_meta` keys that the allow-list names, next
to Triton's own keys. Every other key is dropped:

| key | shape | who gets it |
|---|---|---|
| `ui` | object | every client: your MCP App's resource link (#143); the host decides whether to render it |
| `tool_trace` | array | every client: an opt-in debug sidecar |
| `chart` | object | card hosts |
| `png_base64` | string | card hosts |
| `svgs` | array | card hosts |
| `vega_specs` | array | card hosts (deprecated, see above) |

A card host is a client that renders the answer card: it declared the MCP
Apps extension and the deployment serves the widget. Chart data goes to card
hosts only, like the card itself; a client that cannot show it does not get
it. A key with the wrong shape is dropped.

The operator adds keys for one tool, or gives a listed key another audience,
in `adapter.yaml`:

```yaml
tools:
  render_report:
    meta:
      openai/outputTemplate: card_host   # an added key, any JSON value
      png_base64: all                    # a listed key, widened
      vega_specs: "off"                  # a listed key, dropped
```

The rules:

- **Protected keys.** An upstream never sets `trace_id`, any `triton/` key
  (`triton/a2ui`, `triton/in_card`, later ones), or a key whose prefix
  contains the label `modelcontextprotocol` or `mcp`
  (`io.modelcontextprotocol/related-task`; the MCP specification reserves
  these). They are not on the list, and a manifest that lists one does not
  boot.
- **Size budget.** Passed keys may use 4 MiB of serialized JSON in total,
  taken in list order (the table above, then the operator's keys). A key
  that would exceed the budget is dropped and logged (`warn`). The keys
  before it still pass, and the call still succeeds.
- Only a top-level `_meta` object is read. Any other `_meta` passes nothing.
- An embedding host has no `adapter.yaml`: it lists the same per-tool keys
  and audiences with `triton_embed::EmbedOpts::upstream_meta`.
- REST returns the dispatch result unchanged, and the chat and A2A adapters
  read the dispatch result, so none of these rules apply there.

`_meta` is not shown to the model. `ui` and `tool_trace` reach every MCP
client, card host or not: as with surfaces, nothing the caller must not see
belongs there.

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

## 8. In-process upstream (a host that embeds Triton)

A host that embeds Triton (`triton-embed`) can serve its agents without
HTTP between Triton and the agent: it implements
`triton_core::UpstreamDispatch` and hands it to the dispatcher with
`Dispatcher::with_upstream`. Everything above the dispatcher is the
binary's: `triton_embed::router_parts(dispatcher, &opts)` mounts the same
REST/MCP/A2A routes, and with `EmbedOpts::ingress(IngressConfig::from_env()?)`
the same ingress — Entra multi-tenant with required scopes, opaque Google
access tokens, `/mcp/<slug>` with its RFC 9728 metadata, spec A2A from
`TRITON_A2A_*`, the task store and timing from `TRITON_TASK_*` /
`TRITON_MCP_*` (including `TRITON_MCP_CARD_NOTE`, and the server's
`serverInfo` name and title and `instructions` from `TRITON_MCP_SERVER_NAME`, `TRITON_MCP_SERVER_TITLE` and
`TRITON_MCP_INSTRUCTIONS`), the default A2UI `agentDisplayName` from
`TRITON_A2UI_AGENT_DISPLAY_NAME`, and `TRITON_DEV_TOKEN`
and `TRITON_MCP_APP_WIDGET`. The
variables, defaults and refusals are the ones in the table at the top of
`crates/triton-embed/src/ingress.rs`; the binary flattens the same clap
group. An explicit `EmbedOpts` option (`spec_a2a`, `dev_token`, `widget`)
wins over the ingress; a Google verifier configured in both places is
refused. `router_parts` also returns the task `Runner` and the identity
provider, so a host can wire a Teams adapter into the same store with
`triton_embed::chat::msteams` (feature `chat`; `MsTeamsConfig::from_env()`
is the binary's `TRITON_MSTEAMS_*` group, with its NFR-S-4 refusals) and
build the channel registry with `triton_embed::surfaces::registry`.

The wire sections above map onto the trait as follows. An in-process
upstream that keeps to this mapping is indistinguishable from a static
upstream to every caller:

| Wire (§) | In-process |
|---|---|
| `POST /` with `X-Triton-Tool` (§2) | `invoke(tool, args, principal, ctx)`; `invoke_streaming` for the A2A, MCP and Teams faces |
| `X-Triton-Channel`, `_triton.surface` (§2b) | `ctx.channel`; call `triton_core::surface_hint(args, ctx)` to put `_triton.surface` into the args as the static upstream does, or read `ctx.surface` directly |
| The bearer and its claims (§3) | `principal` — the verified identity, never a token to re-verify |
| A JSON result or `done` (§4) | `Ok(value)` / `StreamEvent::Done(value)`, the same canonical surface envelope |
| An agent fault: non-2xx, timeout, undecodable body | `Err(TritonError::Tool(..))` — mapped on every face as a static upstream's agent fault |
| A tool the agent does not serve | `Err(TritonError::Validation("unknown tool: <name>"))` — a caller fault, as the static map answers |
| `X-Triton-MCP: tools/list` (§2a) | `agent_tools()`; `list_agents()` names the tools |
| `task`, `progress`, `done`, `error` events (§7) | `StreamEvent::Task({task_id, check_after_seconds?, receipt?, note?})`, `StreamEvent::Progress({text, step?, percent?})`, `StreamEvent::Done`, `StreamEvent::error(TritonError::Tool(message))` |
| A stream that closes without a terminal event (§7) | a stream that ends after `Task` without `Done`/`Error`: the runner resumes it |
| Resume with `X-Triton-Task` (§7) | `resume_streaming(tool, task_id, args, principal, ctx)`, answering from the task's current state; an unknown id is an `Err` |
| `resources/read`, `updateModelContext` of a `ui://` resource | `read_resource`, `update_model_context` |

The rules of §7 hold unchanged: a resume must never start the work again,
only the task's owner drives one, and a stream that breaks before `Task`
fails the call. In-process there is no egress, so the NFR-S-4 static-map
allowlist and the minted upstream JWT do not apply; the principal the
trait receives is the one Triton verified.

Pinned by `tests/embed_parity.rs`: the `triton` binary dispatching to HTTP
fakes through `TRITON_STATIC_UPSTREAMS` and an embedded host dispatching to
an in-process upstream, both configured from the same `TRITON_*` names,
answer MCP `tools/list` (with the answer-card widget `_meta`),
`resources/list`, a widget-carrying `tools/call`, both A2A card versions,
the RFC 9728 metadata and the 401/403 challenges of `/mcp/<slug>`, the
error mapping of an agent fault and an unknown tool, and a broken task
stream (resumed by the upstream task id, never refiled) identically.
`tests/embed_ingress.rs` and `tests/embed_msteams.rs` pin the embedded
ingress and the embedded Teams adapter.
