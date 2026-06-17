# Triton — Requirements Specification

Status: draft v0.2 (2026-05-23)
Scope: a production Rust implementation of the Triton multi-protocol
agent-ingress gateway, deployed as a Nomad job behind Fabio on the
Hetzner agent substrate v2. The same binary is the reference
implementation of the multi-adapter pattern other substrate
services adopt internally.

v0.2 absorbs the chat-channel extension specified by the follow-up
paper at `2026-05-23-triton-messengers/`. Six chat-channel adapters
(Telegram, WhatsApp Web, Signal, MS Teams, Discord, Google Chat)
join the original HTTP trio (MCP, A2A, REST). New requirements
carry the suffix `(v0.2)`; superseded requirements name the M-*
hypothesis from the follow-up paper that drove the change. The
hypothesis cross-reference is at §9.

Companion documents in this directory: `architecture.md` (arc42 HLD),
`realizations.md` (experiment-derived implementation traps).

## 1. Context

Triton is the public agent surface of the Hetzner agent substrate.
Clients (browser tooling, agent SDKs, MCP hosts, A2A peers, plain
REST callers) reach Triton through Fabio on `:443`. Triton verifies
the bearer token, dispatches each call to the appropriate upstream
*agent* (a separate Nomad job carrying domain-specific tooling), and
returns either a plain JSON result or an A2UI surface, in the wire
format the caller used.

The design has two prior experiments and one substrate-binding doc:

- `2026-05-15-triton/` — Python reference implementation, three
  adapters (MCP/A2A/REST), three demo tools (`compute_stats`,
  `narrate_stats`, `render_dashboard`), shared dispatcher, A2UI v0.8
  and v0.9 emission, dev-token auth.
- `2026-05-16-triton-rust/` — Rust parity port; same wire contract,
  hand-rolled MCP JSON-RPC over axum, `tokio::select!` over three
  servers, no `/preview` page.
- `2026-05-20-substrate-workload-constraints/` — pairs every
  workload-MUST (G-1..G-13) with the substrate-MUST it depends on
  (G-S1..G-S7) and enumerates cross-cutting invariants. **Each
  requirement below cites the G-* clause it implements.**

### 1.1 Naming

Substrate §3a calls out the "multi-protocol gateway pattern"; §3b
calls out the "deployed Triton instance / agent ingress". This spec
treats them as the same system: **Triton is the deployed gateway, and
its source is the canonical reference other substrate services copy.**
There is no separate "pattern" component to build.

## 2. Glossary

- **Adapter** — the per-protocol code that unwraps an inbound request
  into `(tool, args, principal)` and wraps the response back into the
  caller's wire format. One adapter per protocol; no business logic.
- **Dispatcher** — the central `invoke()` path. Argument validation,
  timing, structured audit emission. Every adapter funnels through it.
- **Principal** — `{sub, scopes, tenant, raw_token}`, the result of
  OIDC verification. Carried through dispatcher into upstream
  router.
- **Upstream agent** — a separate Nomad job registered in Consul
  under `tag:agent:<name>`. Implements one or more tools.
- **A2UI** — agent UI envelope format. Triton emits both v0.8 and
  v0.9; clients select via content negotiation.
- **Substrate** — the Hetzner agent substrate v2 (Hetzner Cloud +
  GCS backplane + Nomad/Consul/Vault + Tailscale + Fabio + Packer
  golden image). The deployment target.
- **G-N / G-S-N** — requirement identifiers from
  `2026-05-20-substrate-workload-constraints/README.md`. **Workload**
  clauses (G-N) are obligations of this binary; **substrate** clauses
  (G-S-N) are obligations of the substrate that this binary depends
  on but does not implement.

## 3. Goals / Non-goals

### 3.1 Goals

1. Three HTTP protocol adapters (MCP, A2A, REST) on three TCP
   listeners in a single process, emitting semantically identical
   A2UI envelopes across all three (G-1). v0.2 adds six chat-channel
   adapters (Telegram, WhatsApp Web, Signal, MS Teams, Discord,
   Google Chat) under the same dispatcher and audit pipeline.
2. OIDC bearer-token authentication against the substrate identity
   issuer for the HTTP trio; principal forwarded to upstream agents
   as a fresh Vault-minted short-lived OIDC token (G-2, G-10b). v0.2
   chat-channel adapters select one of four identity-resolution
   strategies per manifest (FR-I, ADR-14).
3. Consul-driven upstream agent discovery; no static endpoint config
   (G-9).
4. Per-tool timeout + circuit-breaker isolating slow agents (G-11).
5. One audit line per inbound call and per upstream dispatch, both
   carrying the same `trace_id` (G-3, G-13). v0.2 chat-channel
   invocations emit a `Dispatch` record + a `Post` record under the
   same `trace_id`; signature rejections emit a `Rejected` record.
6. SIGTERM in-flight drain to support Nomad blue/green canary
   promotion without dropped requests (G-4).
7. Stateless across restarts (G-8). Single Rust static binary baked
   into the Packer golden image (G-6, G-S4).
8. **(v0.2)** Declarative YAML manifest (`adapter.yaml`) as the
   single source of truth for adapter wiring, tool registration,
   identity strategies, surface mapping rules, and rate-limit
   budgets. Boot-time validation closed-checks every kind /
   signature / identity / `degrade` key against the documented set;
   no hardcoded tool or adapter registry (ADR-13, M-MANIFEST-1).
9. **(v0.2)** Surface mapper at L6′ projects the A2UI envelope onto
   a platform-neutral `PlatformMessage` per the adapter's `degrade`
   rule table. Pure function; per-platform caps enforced at the
   mapper edge; dashboard components delegated to an injected
   `Rasterizer` (ADR-12, M-MAP-1, M-RICHNESS-1, M-RASTER-1,
   M-PARITY-MULTI-1).

### 3.2 Non-goals

- Browser-facing `/preview` page or bundled Lit runtime
  (`/static/runtime.html`). Dev-only artefacts inherited from the
  Python prototype that do not ship to the substrate.
- Persistence. The process holds no user data across restarts; A2A
  task storage is in-memory only, as in the experiments.
- Native renderers (Flutter, React) — clients pick their renderer.
- TLS termination — Fabio terminates TLS; Triton speaks HTTP/1.1
  cleartext over Tailscale or loopback.
- LLM provider integration in Triton itself. Narration and any other
  LLM work belongs to upstream agents.
- Multi-region HA. Single substrate region per `SPEC.md §4`.

## 4. Stakeholders

| Role | Concern |
|---|---|
| Substrate operator | Triton boots clean on a fresh allocation; SIGTERM drain works; audit lines parse; `/version` matches the deployed image. |
| Agent author | Register a Nomad job with `tag:agent:<name>`, expose a tool schema, get traffic. No coupling to Triton's release cycle. |
| App author | Spin up Triton in the app's own CI with a single `cargo add` (path/git dep) and ≤ 30 LOC of test wiring; exercise `frontend → triton → app-agent` against a real Triton process without standing up Consul, Vault, or an OIDC issuer. |
| Client author (MCP host, A2A peer, REST caller) | One stable URL on `:443`; wire-format symmetry; A2UI version negotiation. |
| Security reviewer | OIDC verification at the boundary; no static credentials; principal-scoped Vault tokens for upstream; no exfiltration path beyond audit + agents. |

## 5. Functional requirements

Requirements use **MUST / SHOULD / MAY** (RFC 2119). Each requirement
cites the G-* clause it implements. The coding agent has discretion
on implementation details not pinned below.

### 5.1 Adapter ring (FR-A)

- **FR-A-1** Triton MUST expose three concurrent HTTP TCP listeners
  on configurable ports, defaulting to MCP on `:8001`, A2A on
  `:8002`, REST on `:8003`. All HTTP/1.1, no UDP/QUIC. *(G-1;
  matches both experiments.)*
- **FR-A-1.v0.2** Triton MUST additionally expose one inbound
  listener per chat-channel adapter declared in `adapter.yaml`,
  selected from three closed-set realisations: `webhook` (HTTPS
  endpoint, used by Telegram, MS Teams, Discord Interactions,
  Google Chat), `socket` (persistent WebSocket or UNIX domain
  socket, used by WhatsApp Web, Discord Gateway, signal-cli daemon),
  `long_poll` (worker that polls the platform's `getUpdates`
  endpoint, supported by Telegram as an alternative to webhook).
  *(M-INBOUND-1; ADR-11.)*
- **FR-A-2** Each adapter MUST be the **only** code that knows its
  wire format. Adapters unwrap requests into `(tool_name, args_json,
  principal, trace_id, requested_a2ui_version)` and wrap responses
  back. v0.2 chat-channel adapters split this into an inbound
  listener (unwrap) and an outbound courier (wrap); the dispatcher
  sits between them. No business logic, no upstream calls, no
  audit emission inside the adapter. *(G-1, audit-symmetry
  invariant from Python experiment; M-INBOUND-1.)*
- **FR-A-3** The REST adapter MUST honour `Accept:
  application/json+a2ui` (default) and `Accept: application/json+a2ui;
  version=0.9`. The A2A adapter MUST honour
  `Message.metadata.a2ui_version: "v0.9"`. The MCP adapter MUST emit
  the version associated with the negotiated MCP App.
- **FR-A-4** For every tool that returns an A2UI surface, the
  envelope MUST be **semantically identical** across all three
  adapters when the same input + principal + version are supplied.
  Parity is asserted by comparing parsed dicts, not raw bytes (JSON
  key order is not stable across serde paths).
- **FR-A-5** REST MUST expose `GET /v1/tools` listing tool names,
  input JSON schemas, and a `returns_a2ui` flag, mirroring the
  Python reference at `2026-05-15-triton/triton/adapters/rest.py`.
- **FR-A-6** MCP MUST implement `initialize`, `tools/list`,
  `tools/call`, and `resources/read` over Streamable HTTP
  (JSON-RPC 2.0 over HTTP, plain JSON responses; SSE not required).
  The runtime resource (`ui://triton/runtime.html`) MAY be a stub —
  the substrate deployment does not serve the bundled Lit runtime.
- **FR-A-7** A2A MUST implement `POST /message:send` accepting
  `Message{parts: [Part{data: {tool, args}}]}` and returning a
  response `Message` whose part carries the result.
  `InMemoryTaskStore` is sufficient (the protocol requires *a* store;
  no user data persists).
- **FR-A-8 (v0.2)** The chat-channel outbound courier MUST emit one
  `PlatformMessage` per dispatched invocation, encoded into
  platform-native bytes per the adapter's `degrade` rule table
  (manifest section `adapter.<name>.degrade`). The courier MUST
  hold the platform credential (Vault-referenced per NFR-S-5) and
  MUST NOT forward the inbound principal raw to the platform.
  *(M-OUTBOUND-1; ADR-11.)*
- **FR-A-9 (v0.2)** The surface mapper MUST be a pure function of
  `(envelope, adapter.<name>.degrade, SurfaceLimits)`; the
  `PlatformMessage` produced for the same envelope MUST be
  byte-equal across two runs against the same adapter. *(M-MAP-1.)*
- **FR-A-10 (v0.2)** The surface mapper MUST enforce documented
  per-platform surface caps at its edge (Discord 25-item select
  cap, Telegram 8-buttons-per-row, WhatsApp Web 4000-char text
  chunk, MS Teams Adaptive Card layout caps). On cap excess, the
  mapper MUST chunk text into multiple `Fragment::Text`, paginate
  button rows into multiple `ButtonSet` fragments (label only on
  the first page), and reject oversize selection sets with
  `UnsupportedSurface`. *(M-RICHNESS-1.)*
- **FR-A-11 (v0.2)** `dashboard` components MUST be delegated to an
  out-of-process `Rasterizer` (an upstream tool named
  `render_a2ui_to_png` or a peer Nomad sidecar) for text-first
  adapters (Signal, WhatsApp Web, Telegram, Discord, Google Chat);
  the mapper emits a `Fragment::Media` carrying the rendered PNG
  plus a caption derived from the dashboard's narration child. For
  MS Teams, the same envelope projects onto an Adaptive Card
  `ColumnSet` layout natively and skips the rasteriser. The mapper
  MUST reject a `dashboard` component with `UnsupportedSurface`
  if no rasteriser is configured for a text-first adapter.
  *(M-RASTER-1.)*
- **FR-A-12 (v0.2)** Every interactive option emitted by the surface
  mapper (button, selection item) MUST carry a payload that is an
  HMAC-SHA256 signed token of layout
  `b64url(JSON({tool, args})) || "." || b64url(HMAC)`, signed under
  the adapter's `CorrelationKey` (per-adapter 32-byte key, manifest-
  declared via Vault reference). The inbound listener MUST verify
  this token in constant time on every follow-up event (Telegram
  `callback_data`, Discord components v2 `custom_id`, MS Teams
  `Action.Submit.data`, numbered-prompt reply text for Signal /
  WhatsApp Web / Google Chat); rejection MUST surface as a
  documented error and MUST NOT reach the dispatcher. *(M-CORRELATION-1.)*
- **FR-A-13 (v0.2)** For any tool invocation whose envelope carries
  only stable, non-rasterised component types (text, narration,
  button-row, selection-row), the `PlatformMessage` produced by
  any two chat-channel adapters MUST be equivalent under the
  pairwise application of the two adapters' `degrade`-rule
  inverses. *(M-PARITY-MULTI-1; the chat-channel analogue of
  FR-A-4.)*

### 5.2 Dispatcher (FR-D)

- **FR-D-1** There MUST be a single `ToolRegistry::invoke` path
  through which every tool call from every adapter passes. *(G-1
  parity, G-3 audit symmetry.)*
- **FR-D-2** The dispatcher MUST validate `args_json` against the
  tool's declared schema (serde-derived structs) before invoking the
  handler; validation failures surface as `ValidationError`.
- **FR-D-3** The dispatcher MUST support both sync and async
  handlers; the Rust port uses async throughout, so this collapses to
  awaiting an `async fn`.
- **FR-D-4** The dispatcher MUST surface four typed error variants:
  `Auth`, `Validation`, `Tool`, `Provider`. Adapters MUST translate
  each into a protocol-appropriate response (REST: HTTP status; A2A:
  `Message.metadata.error`; MCP: JSON-RPC error code).
- **FR-D-5** The dispatcher MUST measure per-invocation latency in
  milliseconds (u64) and pass it to the audit emitter (FR-AU-1).

### 5.3 Identity (FR-I)

- **FR-I-1** Triton MUST verify the inbound bearer token against the
  substrate OIDC issuer (G-S1) before calling the dispatcher. The
  experiments' fixed dev-token verifier is replaced; the `Principal`
  type stays. *(G-2.)*
- **FR-I-2** The JWKS for the substrate issuer MUST be cached
  per-`kid` and refreshed on cache miss, rate-limited to at most one
  fetch per N seconds per `kid` (default 30 s) to prevent JWKS-poll
  DoS.
- **FR-I-3** Permitted JWT algorithms: RS256, RS384, RS512, ES256,
  ES384, EdDSA. `none` and symmetric algorithms MUST be rejected.
- **FR-I-4** On verification success, Triton MUST construct
  `Principal{sub, scopes, tenant, raw_token, trace_id}` and pass it
  through dispatcher and into the upstream router.
- **FR-I-5** A dev-token fallback MAY exist but MUST be gated by a
  build-time `cfg` so production builds reject any non-OIDC token at
  compile time. *(Cross-cutting §2 invariant 3: no static
  credentials.)*
- **FR-I-6 (v0.2)** Every chat-channel adapter MUST yield a
  `Principal{sub, scopes, tenant, raw_token, trace_id}` that is
  structurally identical to the OIDC-derived principal of the HTTP
  trio. The field set, field types, and JSON serialisation MUST be
  byte-equal across the two paths. *(M-IDENT-1.)*
- **FR-I-7 (v0.2)** Each chat-channel adapter MUST select an
  identity-resolution strategy from the closed set
  `{sender_table, azure, self_enrol, upstream}`, declared in
  `adapter.<name>.identity.kind`. The strategy is consulted after
  the platform signature check and produces the chat-channel
  `Principal`:
  - `sender_table` — operator-enumerated platform-id-to-`Principal`
    table (Vault-referenced).
  - `azure` — Microsoft Entra ID principal derived from the Bot
    Framework activity's signed claims (MS Teams).
  - `self_enrol` — pairing flow for unknown senders; first contact
    returns a `Principal` with the literal scope `"pairing"` only;
    operator confirmation via a side channel enrols the sender in
    the per-adapter `fallback_table` and subsequent inbound events
    yield a fully-scoped `Principal` with the same `subject`
    (M-ENROL-1).
  - `upstream` — delegated to a resolver tool reached through the
    upstream router.
- **FR-I-8 (v0.2)** Each chat-channel adapter MUST verify the
  platform's inbound signature scheme *before* parsing the payload,
  selected from the closed set
  `{secret_token, hmac256, bot_framework_jwt, ed25519, google_oidc_jwt}`,
  declared in `adapter.<name>.inbound.signature`. Verification
  failures MUST emit an `AuditPhase::Rejected` record and MUST NOT
  reach the dispatcher. Coverage:
  - `secret_token` — Telegram `X-Telegram-Bot-Api-Secret-Token`
    header equality (constant-time).
  - `hmac256` — HMAC-SHA256 over the body under an app secret
    (the WhatsApp Cloud API adapter, `kind: whatsapp_cloud`, #94;
    not used by the canonical Baileys-style `whatsapp_web` adapter).
  - `bot_framework_jwt` — Bot Framework JWT validated against the
    published OpenID metadata (MS Teams).
  - `ed25519` — Ed25519 signature over `(timestamp || body)` under
    the application public key (Discord Interactions).
  - `google_oidc_jwt` — Google-issued OIDC service-account JWT
    validated against Google's published OIDC metadata with the
    `aud` claim checked against either the deployment's webhook
    URL or its Cloud project number (Google Chat).
  Platforms whose transport is a local socket (WhatsApp Web
  session, signal-cli daemon) authenticate at the session-locality
  boundary instead and are exempt from per-message signature
  verification at the adapter layer. *(M-SIG-1.)*
- **FR-I-9 (v0.2)** The Signal adapter MUST refuse to construct
  (and therefore refuse to start) if its configured signal-cli
  bridge endpoint does not resolve to a loopback address (IPv4
  `127.0.0.0/8`, IPv6 `::1`, or a `unix://` socket on the local
  filesystem). The refusal MUST be documented in stdout logs and
  MUST prevent `/healthz` from returning 200. *(M-LOCALITY-1; C-11.)*

### 5.4 Upstream router (FR-U)

- **FR-U-1** Triton MUST discover upstream agents by querying Consul
  for services tagged `agent:<tool_name>`. There MUST be no static
  list of agent endpoints in config or code. *(G-9.)*
- **FR-U-2** For each inbound tool call, the router MUST resolve the
  upstream, mint a fresh short-lived OIDC token via Vault role
  `agent-oidc-swap` scoped to the agent (G-S7), and dispatch over
  HTTP/1.1 on the tailnet. The router MUST NOT forward the inbound
  raw token. *(G-10b; lethal-trifecta cut §2-7.)*
- **FR-U-3** Each tool MUST have a configurable upstream timeout
  budget and a per-tool circuit-breaker with three states (closed,
  open, half-open). The breaker MUST open after N consecutive
  timeouts (configurable, default 5) and probe with a single
  half-open request after a cooldown (configurable, default 30 s).
  *(G-11.)*
- **FR-U-4** When a tool's circuit is open, calls to that tool MUST
  return a `Tool` error with reason `circuit_open`; other tools MUST
  remain available.
- **FR-U-5** The router MUST wrap raw upstream JSON results into an
  A2UI envelope when the inbound caller requested A2UI; pre-shaped
  A2UI returned by an upstream MUST be passed through unchanged.
  *(G-12.)*

### 5.5 Audit (FR-AU)

- **FR-AU-1** Triton MUST emit one JSON line to stdout per inbound
  call (at the dispatcher) AND one per outbound dispatch. Both
  lines MUST share the same `trace_id`. *(G-3, G-13.)*
  - For the HTTP trio, the outbound line is the upstream-router
    record.
  - **(v0.2)** For chat-channel adapters, the outbound line is the
    courier record emitted at platform-API response with a
    `phase: "post"` discriminator and a `status` field drawn from
    the closed set `{posted, retry, dropped}`. The dispatcher
    record carries `phase: "dispatch"`. Inbound signature
    rejections produce a third record kind, `phase: "rejected"`,
    emitted before the dispatcher is reached. *(M-ASYNC-1; ADR-15.)*
- **FR-AU-2** Each line MUST contain at least: `who` (Principal.sub),
  `what` (tool name), `when` (RFC 3339 UTC), `env` (env label),
  `result` (`ok`|`error:<class>`), `protocol`
  (`mcp`|`a2a`|`rest`|`upstream`), `tool`, `subject`, `tenant`,
  `latency_ms`, `status`, `trace_id`. Matches the experiments'
  schema extended with the substrate fields `{who, what, when, env,
  result}` per `SPEC.md §11`.
- **FR-AU-3** Tokens, JWKS private material, and Vault-minted upstream
  tokens MUST NEVER appear in audit lines or any other log. Only
  `sub`, `tenant`, and a token-hash prefix when needed for
  correlation.
- **FR-AU-4** Triton MUST NOT ship audit lines anywhere itself. The
  substrate audit-collector (G-S3) tails stdout. *(Do not introduce
  Loki/Vector/OTel-exporter dependencies.)*

### 5.6 Observability (FR-O)

- **FR-O-1** Triton MUST expose `GET /healthz` returning
  `{"status":"ok"}` once the three listeners are bound. *(Substrate
  health-probe contract.)*
- **FR-O-2** Triton MUST expose `GET /version` returning the binary
  SHA and golden-image SHA. *(G-6.)*
- **FR-O-3** Triton MUST expose a Prometheus/OTel metrics endpoint
  (e.g. `GET /metrics`) bound to the tailnet only, scraped by
  `tag:ops`. Public ingress through Fabio MUST NOT expose this
  endpoint. *(G-5, G-7.)*

### 5.7 Lifecycle (FR-L)

- **FR-L-1** Triton MUST process startup with no local state and
  MUST come up cleanly on a fresh Nomad allocation. *(G-8, §2
  invariant 4 — cattle nodes.)*
- **FR-L-2** On SIGTERM, Triton MUST stop accepting new connections
  on all three listeners, allow in-flight requests to complete or hit
  a per-request deadline (configurable, default 30 s), flush stdout,
  and exit 0. *(G-4.)*
- **FR-L-3** On SIGINT, behave identically to SIGTERM (developer
  convenience matching the Rust port's
  `tokio::signal::ctrl_c()` handler).
- **FR-L-4 (v0.2)** At cold start, Triton MUST load `adapter.yaml`
  and closed-check every kind discriminator (`adapter.<name>.kind`,
  `inbound.kind`, `outbound.kind`, `identity.kind`,
  `inbound.signature`) and every `degrade` rule key against the
  documented sets. Boot MUST refuse on any unknown value. *(M-MANIFEST-1.)*
- **FR-L-5 (v0.2)** At cold start, Triton MUST verify that for every
  tool whose `surface_components` declaration is non-empty, every
  chat-channel adapter's `degrade` table contains a rule for each
  declared component type. Boot MUST refuse on any missing rule.
  *(M-COVERAGE-1.)*
- **FR-L-6 (v0.2)** Every credential field in `adapter.yaml` (bot
  tokens, webhook secrets, service-account references, Azure
  identity references, identity tables, correlation keys) MUST be
  either a `vault://<path>#<field>` reference (resolved at boot
  against Vault) or a literal value admitted only in dev mode with
  a runtime warning. Production builds MUST refuse to start on
  any literal credential. *(M-SECRETS-1; NFR-S-5.)*

### 5.8 Consumer test harness (FR-T)

These requirements pin the developer-experience surface a
third-party app author depends on when writing
`frontend → triton → app-agent` integration tests in the app's own
CI. The capabilities themselves are not new; this section names
them as a supported contract. See `consumer-integration-tests.md`
for the worked walkthrough.

- **FR-T-1** Triton MUST support a "minimum-viable boot" mode in
  which no Consul, no Vault, and no OIDC issuer are reachable, the
  manifest is empty, and the binary accepts the literal bearer
  `"dev-token"` (mapped to `Principal{sub: "dev-user", scopes:
  ["dev"], tenant: "dev"}`). This mode MUST be on by default in
  debug builds via the `dev-token` Cargo feature on `triton-bin`
  and MUST be compiled out of release builds (re-affirms ADR-10).
  When `TRITON_OIDC_ISSUER` is configured, dev-token MUST be
  rejected.
- **FR-T-2** The Rust test-harness crate (currently
  `crates/triton-tests`) MUST expose its fixtures —
  `TritonProcess`, `TestIssuer`, `FakeConsul`, `FakeVault`,
  `FakeAgent`, and the chat-platform fakes — as `pub` items
  consumable from external Rust workspaces via path or git
  dependency. The public surface follows a deprecation cycle, not
  free refactoring (see ADR-16).
- **FR-T-3** Each backing-service fixture MUST be self-contained:
  binds an ephemeral loopback port, returns its base URL, requires
  no shared filesystem state, and releases its port on `Drop`.
  Two consumer tests running in parallel under
  `cargo test --jobs N` MUST be able to spin independent fixtures
  without contention.
- **FR-T-4** A consumer integration test MUST be able to register
  a stub upstream agent by passing `(service_name, host:port)`
  tuples to `FakeConsul::start`; Triton's upstream router MUST
  dispatch to the stub without any additional configuration
  beyond pointing `TRITON_CONSUL_URL` at the fake.
- **FR-T-5** The env-gated platform-API redirection vars
  (`TRITON_TELEGRAM_API_BASE` and per-adapter equivalents added by
  future PRs) MUST permit redirection to a fake endpoint when
  `TRITON_ENV=local` and MUST be refused outside `local`, so the
  same fake-platform fixtures the test harness ships can drive
  consumer tests without widening NFR-S-4's egress allowlist in
  production.

## 6. Non-functional requirements

### 6.1 Security (NFR-S)

- **NFR-S-1** No static cloud credentials in the binary, the image,
  or Nomad job env. JWKS verifies inbound tokens; Vault mints
  upstream tokens; no other secret material is in scope. *(§2
  invariant 3.)*
- **NFR-S-2** Public surface only via Fabio on `:443`. The three
  adapter ports MUST NOT be `urlprefix-`-tagged in Consul; Fabio sees
  one stable host (`agents.<env>.<domain>`) routed at Triton. *(G-7,
  G-S6.)*
- **NFR-S-3** Lethal-trifecta cut: Triton processes
  attacker-controllable input AND has a network path to upstream
  agents, so it MUST NOT hold prod credentials beyond the
  per-request lifetime. Vault-minted upstream tokens MUST have TTL ≤
  5 minutes. *(§2 invariant 7.)*
- **NFR-S-4** Air-gap stance: no outbound network from the binary
  except to (a) Consul DNS, (b) Vault on the tailnet, (c) the
  substrate OIDC issuer for JWKS, (d) discovered upstream agents on
  the tailnet, (e) stdout. No DNS over public internet, no telemetry
  exporters dialling out. *(§2 invariants 2, 6.)*
  - **(v0.2)** The chat-channel adapters add platform-API egress
    paths on a per-adapter basis: `api.telegram.org`,
    `discord.com/api`, `graph.facebook.com`, `chat.googleapis.com`,
    `smba.trafficmanager.net` (Bot Connector) and
    `graph.microsoft.com`, plus the published OIDC metadata
    endpoints for Bot Framework JWT and Google OIDC validation
    (`login.botframework.com`, `accounts.google.com`). These paths
    are documented in the manifest and are the only public-internet
    egress permitted by the substrate ACL.
  - **Static-upstream hostname allowlist:** the `TRITON_STATIC_UPSTREAMS`
    SSRF guard trusts hostname endpoints only under an explicit DNS
    suffix. The strict default is `.ts.net` (Tailscale MagicDNS); an
    operator may widen it with `TRITON_EGRESS_ALLOWED_SUFFIXES`
    (comma-separated, e.g. `.ts.net,.int.data-zoo.de`) to admit a
    trusted private split-DNS domain — the substrate addresses every
    service as `*.nonprod.int.data-zoo.de`, which resolves to private
    host IPs behind kamal-proxy. This is an explicit operator opt-in: it
    only widens the hostname path, never relaxes the IP-literal rules
    (loopback / RFC-1918 / CGNAT / ULA only; public + metadata refused),
    and performs no DNS resolution (purely name-suffix based, no TOCTOU).
- **NFR-S-5 (v0.2)** All chat-channel adapter credentials (bot
  tokens, webhook secrets, service-account JSON, Azure identity
  references, identity tables, correlation keys) MUST be Vault
  references in production; literal values are admitted in dev mode
  only with a runtime warning. The boot-time manifest validator
  rejects literal credentials in production mode. *(M-SECRETS-1.)*
- **NFR-S-6 (v0.2)** The Signal adapter's bridge endpoint MUST
  resolve to a loopback address; signal-cli runs as a host-local
  sidecar and terminates Signal end-to-end encryption inside the
  trust boundary of the Triton allocation. Non-loopback endpoints
  are rejected at adapter construction time (FR-I-9). *(M-LOCALITY-1;
  C-11.)*

### 6.2 Performance (NFR-P)

- **NFR-P-1** Per-request auth overhead (cached JWKS hit) SHOULD be
  < 1 ms on a modern x86 core.
- **NFR-P-2** Upstream dispatch SHOULD add < 5 ms overhead beyond
  the agent's own latency in the steady state (no token mint, no
  Consul lookup) — both Vault tokens and Consul resolutions cached
  with sensible TTL.
- **NFR-P-3 (v0.2)** Each chat-channel adapter MUST enforce the
  per-adapter `rate_limit` budget (`messages_per_sec`, `burst`)
  declared in the manifest. The budget is enforced at the courier;
  excess outbound traffic is queued under the burst budget and
  rejected (or back-pressured to the dispatcher per platform
  semantics) past it. Per-platform defaults: Telegram 25/s,
  WhatsApp Web 6/s, Signal 5/s, MS Teams 8/s, Discord 50/s,
  Google Chat 60/s.
- **NFR-P-4 (v0.2)** Persistent-socket adapters (WhatsApp Web,
  Discord Gateway) MUST recover from socket loss within a bounded
  budget: ≤ 30 s for a clean disconnect (transport-level reset
  without re-pairing), ≤ 5 min for the WhatsApp Web QR re-pairing
  path. The dispatcher MUST resume receiving inbound events after
  recovery without operator intervention. *(M-LIFECYCLE-1.)*

### 6.3 Operability (NFR-O)

- **NFR-O-1** Configuration via CLI flags + `TRITON_*` environment
  variables (Nomad-template friendly), mirroring the experiments'
  `triton/settings.py` and `src/settings.rs`. **(v0.2)** A YAML
  manifest (`adapter.yaml`) is in scope for v0.2 and supersedes the
  v0.1 "no config files" stance for the chat-channel surface area
  (ADR-13). The manifest declares adapters, tools, identity
  strategies, surface mapping rules, and rate-limit budgets;
  Vault references are mandatory in production for every credential
  field (NFR-S-5). CLI flags and env vars continue to govern
  process-wide settings (ports, host, dev token).
- **NFR-O-2** Single static Rust binary; the golden image baseline
  is Rust-only (G-S4). No Python runtime, no Node runtime in the
  Triton allocation.
- **NFR-O-3** Resource budget: the Triton allocation is light (no
  model resident, no per-request fan-out beyond one upstream). The
  default Nomad client class (`CX22`-equivalent) is sufficient; do
  not request `kb-class`. *(Contrast KB-S6.)*

### 6.4 Portability (NFR-PT)

- **NFR-PT-1** Build target: `linux/x86_64` (substrate Hetzner
  Cloud nodes). `linux/aarch64` SHOULD also build for local dev on
  Apple Silicon; not required to ship.
- **NFR-PT-2** All third-party crates MUST be statically linked into
  the binary. The only dynamic dependency permitted is libc.

## 7. Out of scope (deferred)

- gRPC adapter (deferred; the experiments use plain HTTP for all
  three HTTP protocols).
- mTLS between Triton and upstream agents (Tailscale provides the
  transport security; per-call OIDC tokens provide the identity).
- DPoP, token binding, MTLS-bound tokens (RFC 9449/8473).
- Multi-tenant policy beyond the OIDC `tenant` claim (Triton is
  identity-aware but not policy-rich; agents enforce per-tool policy
  if needed).
- HA / multi-region. Single-region scope per `SPEC.md §4`.
- Native renderers, A2UI builders for versions beyond v0.8 and v0.9.
- ~~**WhatsApp Business Cloud API** (v0.2).~~ **Delivered (#94).**
  The canonical dev/nonprod WhatsApp adapter remains the persistent
  WhatsApp Web socket (Baileys-style, `kind: whatsapp_web`).
  Operators with B2B compliance requirements add a second
  `kind: whatsapp_cloud` adapter (Meta Graph / EU aggregator) with
  `hmac256` inbound, the `/v18.0/{id}/messages` courier, message
  templates (utility/marketing/authentication) for sends outside the
  24-hour service window, and interactive buttons/lists. Proactive
  sends ride the agent-initiated outbound API (#95). Inbound
  `interactive`-reply routing is deferred.
- **Card v2 surface for Google Chat** (v0.2). Google Chat's
  protocol-level Card v2 admits a richer surface; the v0.2 adapter
  exposes only text + media + threads + reactions, mirroring
  openclaw's integration. The surface mapper's `degrade.buttons`
  rule for Google Chat is `numbered_prompts`; the Card v2 leg is
  reserved for a future manifest extension.
- **Group-chat A2UI** (v0.2). Whose `Principal` does a shared
  button tap belong to? The semantics are not in the existing
  surface model; v0.2 accommodates one user at a time.
- **Voice / video** (v0.2). Several chat platforms expose voice and
  video; A2UI does not, and the present spec does not extend the
  envelope.

## 8. Acceptance criteria

Sourced from substrate `README.md` §7. Each acceptance test MUST pass
on `nonprod` before promotion.

- **ACC-1** Three-adapter parity. For the same input + principal +
  A2UI version, parsed-dict comparison of the response envelopes
  across MCP, A2A, REST is identical. *(G-1, FR-A-4.)*
- **ACC-2** SIGTERM drain. `kill -TERM <pid>` during a live in-flight
  call yields no 5xx, no dropped connection, and a clean Nomad alloc
  stop in the event stream. *(G-4, FR-L-2.)*
- **ACC-3** Consul-driven dispatch. Stop one of two stub agents
  registered as `agent:stub-a` / `agent:stub-b`; the matching tool
  surfaces a clean error; the other tool continues to serve. *(G-9.)*
- **ACC-4** Circuit-breaker opens. A slow agent triggers the
  per-tool breaker; subsequent calls return `circuit_open` within
  one round-trip; other tools unaffected. *(G-11, FR-U-3.)*
- **ACC-5** Linked audit. One inbound call surfaces two audit lines
  in the GCS audit bucket within 60 s, sharing one `trace_id`.
  *(G-13, FR-AU-1.)*
- **ACC-6** Tailscale ACL denial. From a `tag:ops` host, a direct
  request to an agent allocation bypassing Triton is denied at the
  ACL layer. *(G-S5.)*
- **ACC-7** OIDC rejection. A call with a missing or invalid bearer
  token is rejected at the identity boundary; the dispatcher MUST
  NOT be invoked. *(G-2, FR-I-1.)*
- **ACC-8** Fresh-allocation cold start. On a freshly scheduled
  Nomad allocation, Triton binds the three listeners, passes
  `/healthz`, and serves traffic without manual intervention. *(G-8,
  FR-L-1.)*
- **ACC-9 (v0.2)** Paper-time manifest validation. Running
  `python3 2026-05-23-triton-messengers/tools/verify_manifest.py
  2026-05-23-triton-messengers/manifest-example.yaml` exits 0 and
  reports `3 of 3 PASS` for M-MANIFEST-1, M-COVERAGE-1, M-SECRETS-1.
  *(FR-L-4, FR-L-5, FR-L-6.)*
- **ACC-10 (v0.2)** Impl-time hypothesis suite. Running
  `python3 2026-05-23-triton-messengers-impl/tools/verify_impl.py`
  reports `≥ 14 of 16 PASS, 0 FAIL`. The two residual deferred
  hypotheses (M-LIFECYCLE-1, M-ENROL-1) are tracked in the
  messenger paper's verification plan and land with the
  WhatsApp Web / Discord Gateway / Google Chat adapter
  implementations.
- **ACC-11 (v0.2)** Cross-channel parity. The Telegram-and-Discord
  parity test
  (`crates/triton-chat-tests/tests/test_chat_adapters_parity.rs`)
  drives the same canonical envelope through both adapters and
  asserts byte-equal text + HMAC-suffixed correlation tokens
  under the manifest's `degrade` rules. *(M-MAP-1,
  M-PARITY-MULTI-1, FR-A-13.)*
- **ACC-12 (v0.2)** Signal locality refusal. Booting the Signal
  adapter with a non-loopback bridge endpoint refuses at
  construction time with a documented error; `/healthz` never
  returns 200; alloc fails to start. *(M-LOCALITY-1, FR-I-9.)*
- **ACC-13** Consumer-harness smoke. A Rust crate outside this
  workspace, declaring `triton-tests` as a path or git dependency,
  boots Triton with no Consul, no Vault, no OIDC issuer, and an
  empty manifest; posts a `dev-token` bearer call to
  `GET /v1/tools`; and receives HTTP 200 with an empty tool list.
  *(FR-T-1, FR-T-2.)*

## 9. Hypothesis cross-reference (v0.2)

The messenger follow-up paper at `2026-05-23-triton-messengers/`
catalogues sixteen M-* empirical claims. Every claim is verified
against (or extends) a requirement in this spec. The table below
is the canonical mapping; the messenger paper's
`verification-plan.md` carries the per-fixture sketches.

| M-* hypothesis    | Requirement(s) verified                                | Phase / status      |
|-------------------|--------------------------------------------------------|---------------------|
| M-ASYNC-1         | FR-AU-1 (two-record audit, shared `trace_id`)          | IMPL — PASS         |
| M-INBOUND-1       | FR-A-1.v0.2, FR-A-2 (three inbound shapes)             | IMPL — PASS         |
| M-OUTBOUND-1      | FR-A-8, FR-AU-1 (courier status discriminator)         | IMPL — PASS         |
| M-IDENT-1         | FR-I-1, FR-I-6 (Principal shape preservation)          | IMPL — PASS         |
| M-SIG-1           | FR-I-8 (closed-set signature schemes)                  | IMPL — PASS         |
| M-MAP-1           | FR-A-9, FR-A-13 (mapper purity, parity)                | IMPL — PASS         |
| M-RICHNESS-1      | FR-A-10 (SurfaceLimits at mapper edge)                 | IMPL — PASS         |
| M-RASTER-1        | FR-A-11 (Rasterizer for dashboard components)          | IMPL — PASS         |
| M-CORRELATION-1   | FR-A-12 (HMAC token round-trip)                        | IMPL — PASS         |
| M-MANIFEST-1      | FR-L-4 (closed-set boot validation)                    | PAPER — PASS        |
| M-COVERAGE-1      | FR-L-5 (degrade rule coverage)                         | PAPER — PASS        |
| M-SECRETS-1       | FR-L-6, NFR-S-5 (Vault credentials)                    | PAPER — PASS        |
| M-LOCALITY-1      | FR-I-9, NFR-S-6, C-11 (Signal loopback refusal)        | IMPL — PASS         |
| M-LIFECYCLE-1     | NFR-P-4 (socket recovery bound)                        | IMPL — deferred     |
| M-PARITY-MULTI-1  | FR-A-13 (pairwise cross-adapter parity)                | IMPL — PASS         |
| M-ENROL-1         | FR-I-7 (`self_enrol` strategy)                         | IMPL — deferred     |

The current aggregate is **14 of 16 PASS, 0 FAIL, 2 deferred**;
the two deferred items land with the WhatsApp Web / Discord Gateway
workers (M-LIFECYCLE-1) and the Google Chat adapter (M-ENROL-1)
respectively.
