# Triton — Realizations from the experiments

Status: draft v0.1 (2026-05-22)
Companion to `requirements.md` and `architecture.md` in this
directory.

This file collects concrete lessons from the two Triton experiments
(`2026-05-15-triton/` Python, `2026-05-16-triton-rust/` Rust) and
the substrate constraints
(`2026-05-20-substrate-workload-constraints/`). Each item names a
trap the production implementation can avoid by knowing about it.
The coding agent should read this once before writing the relevant
module.

No prose padding. Each bullet is: **what to do / not do** — why —
source.

---

## 1. Python prototype lessons (`2026-05-15-triton/`)

- **Parity tests compare parsed dicts, not raw bytes.** JSON key
  order is not stable across serde paths; comparing bytes will fail
  intermittently and waste a day chasing ghosts.
  Source: `2026-05-16-triton-rust/README.md:131-138`.

- **A2UI version negotiation lives ONLY at the envelope layer.** The
  dispatcher and the domain tools must never `if version == "0.9"`.
  Add a new builder file per new version; route to it from the
  content-negotiation helper. Python experiment isolates v0.8 and
  v0.9 into `builder.py` and `builder_v09.py` with no shared base —
  copy that shape.
  Source: `2026-05-15-triton/triton/ui/{builder,builder_v09}.py`,
  `architecture.md` ADR-4.

- **Capability signalling is asymmetric across protocols.** Each
  adapter needs its own translation table for "this tool returns an
  A2UI surface":
  - MCP: `AppConfig.resource_uri` in tool metadata.
  - A2A: `AgentCard.default_output_modes` + `Part.media_type =
    application/json+a2ui`.
  - REST: caller-driven via `Accept: application/json+a2ui[;
    version=0.9]`.

  Do not try to unify these in the dispatcher. The asymmetry is
  inherent to the wire formats.
  Source: `2026-05-15-triton/triton/adapters/{mcp,a2a,rest}.py`,
  Triton paper §Discussion.

- **Adapters MUST stay 100–200 LOC.** If business logic creeps in,
  audit symmetry breaks: two adapters log differently for the same
  call. The Python `dispatcher.py:70-101` is the single audit
  emitter; replicate the structure in Rust.
  Source: `2026-05-15-triton/triton/dispatcher.py`, Triton paper
  §Architecture.

- **A2A protocol compliance requires *a* task store, but no user
  data persists.** The Python prototype uses `InMemoryTaskStore` at
  `triton/adapters/a2a.py:210`. Production must do the same; do not
  introduce Redis to satisfy A2A.
  Source: substrate G-8.

- **Stateless re-renders are non-negotiable.** UI state lives on the
  client. Every interaction is a fresh `(tool, args, principal)`
  invocation. This is what makes horizontal scale-out trivial and
  blue/green canary safe. Do not be tempted to add server-side
  session state "just for one tool".
  Source: Triton paper §Request lifecycle (H-EVENT-1).

- **`/preview` and the Lit runtime are dev-only.** Do not port them.
  They are inherited Python ergonomics; the substrate deployment
  doesn't serve them. The MCP `ui://triton/runtime.html` resource is
  a stub in the Rust port and should remain so.
  Source: `2026-05-16-triton-rust/README.md:23-27`.

---

## 2. Rust port lessons (`2026-05-16-triton-rust/`)

- **String formatting fidelity matters.** A naive `format!("{}", x)`
  loses the `.0` suffix on whole numbers (`1.0` becomes `"1"`); the
  Python prototype renders it as `"1.0"`. If you ever compare
  serialised A2UI across the two reference implementations, you'll
  hit this. Implement a `py_float()` helper and test edge cases
  (`0.0`, `1.0`, `-0.0`, NaN, Inf) explicitly.
  Source: `2026-05-16-triton-rust/src/llm/mock.rs` (`py_float`
  helper).

- **Banker's rounding (round-half-to-even) is NOT `f64::round`.**
  Rust rounds half away from zero; Python's `round(x, n)` rounds
  half to even. If you implement any numeric output meant to match
  Python reference, write the half-to-even logic explicitly and
  test it on edge cases (e.g. `0.5 → 0`, `1.5 → 2`, `2.5 → 2`).
  Source: `2026-05-16-triton-rust/src/llm/mock.rs:34-48`.

- **Hand-rolled MCP JSON-RPC beats `rmcp`/`tonic` for thin layers.**
  The Rust port runs MCP over axum + serde_json in ~200 LOC. Pulling
  in `rmcp` or `tonic` adds significant API surface, build time, and
  framework lock-in for a thin layer. Stay hand-rolled.
  Source: `2026-05-16-triton-rust/src/adapters/mcp.rs`, `Cargo.toml`
  (no `rmcp`/`tonic` dependency).

- **Wrap settings in `Arc<Settings>` from the start.** Axum `State`
  requires `Clone`; making `Settings` itself `Clone` is wasteful for
  what is effectively immutable config. `Arc<Settings>` is the
  idiomatic pattern; retrofitting it later means touching every
  handler signature.
  Source: `2026-05-16-triton-rust/src/adapters/rest.rs:20-30`.

- **`tokio::select!` over three `axum::serve` futures + `tokio::signal::ctrl_c()`
  is the canonical shutdown pattern.** Don't reach for
  `tokio_graceful_shutdown` or actor frameworks. The four-arm select
  (three servers + signal) at `src/cli.rs:49-79` is the whole
  shutdown story.
  Source: `2026-05-16-triton-rust/src/cli.rs:49-79`.

- **`Arc<Mutex<HashSet<String>>>` is enough for MCP session IDs.**
  The Rust port uses exactly this for MCP session tracking. No actor
  framework, no `dashmap`, no `RwLock`. Sessions are write-mostly and
  short-lived; the mutex contention is irrelevant at substrate scale.
  Source: `2026-05-16-triton-rust/src/adapters/mcp.rs:34`.

- **Async traits need `#[async_trait]`** (until the toolchain pinned
  by the golden image stabilises trait async fn). `LlmProvider` in
  the Rust port uses it; mirror that for any new trait with async
  methods.
  Source: `2026-05-16-triton-rust/src/llm/provider.rs:4-7`.

- **Inject `a2ui_version` into the args JSON once, in the adapter,
  not per-tool.** Rust's `if let Some(obj) = args.as_object_mut()`
  pattern is awkward; doing it once at the adapter boundary keeps
  the dispatcher and tools clean.
  Source: `2026-05-16-triton-rust/src/adapters/{rest,a2a}.rs`.

- **MCP responses are plain JSON, not SSE.** The Python prototype's
  test harness tolerates either (`_parse_sse_or_json`), but the Rust
  port emits plain JSON. Stay with plain JSON; SSE is unnecessary
  complexity for the current MCP semantics.
  Source: `2026-05-16-triton-rust/README.md:145-146`.

- **Latency is `u64` milliseconds.** Don't use `i32` or a `Duration`
  in the audit struct; `u64` ms is what the substrate audit-collector
  expects and matches the Python field type.
  Source: `2026-05-16-triton-rust/src/dispatcher.rs:74`.

---

## 3. Substrate-derived realizations (`2026-05-20-substrate-workload-constraints/`)

- **SIGTERM drain must actually drain.** Nomad blue/green canary
  promotion (`SPEC.md §3.7`) sends SIGTERM and waits for the alloc
  to stop. If the binary exits before in-flight requests complete,
  the canary drops traffic. The Python prototype relies on uvicorn
  defaults (insufficient); the Rust port traps ctrl_c but doesn't
  drain (incomplete). Production must use axum's graceful-shutdown
  signal across all three servers and only exit once in-flight
  futures have completed or hit a per-request deadline.
  Source: substrate G-4, Rust port `src/cli.rs:76-78`.

- **Fabio routing table is the Consul catalog. Do NOT ship a
  central router config.** Services declare routes via
  `urlprefix-<host>/<path>` tags in Consul. Triton's REST surface
  carries `urlprefix-agents.<env>.<domain>/`; that's the entire
  ingress configuration. Agent allocations carry `tag:agent:<name>`
  but NO `urlprefix-` tags — they stay invisible to Fabio.
  Source: substrate G-7, G-S2, G-S6, Δ-2.

- **Audit lines go to stdout as JSON; the substrate ships them.** Do
  not introduce a Loki/Vector/OTel-exporter dependency in Triton.
  The substrate audit-collector (G-S3, Δ-3) is a periodic Nomad job
  that tails alloc stdout and writes to the GCS audit bucket. Triton
  is producer-only; shipping is not its concern.
  Source: substrate G-3, G-S3, §2 invariant 6.

- **Public surface only on `:443` via Fabio.** A2A and any future
  gRPC adapter are tailnet-only — they get no `urlprefix-` tag and
  are reached as `triton-a2a.service.consul` over the tailnet.
  Source: substrate G-7, NFR-S-2.

- **No static credentials shipped to production builds.** The
  experiments default to `TRITON_DEV_TOKEN = "dev-token"`. If this
  fallback is left in production, it's a shipped secret. Gate the
  dev-token path on a build-time `cfg` so a release build refuses
  any non-OIDC bearer at compile time.
  Source: substrate §2 invariant 3, requirement FR-I-5.

- **Lethal-trifecta cut constrains the upstream router.** Triton
  processes attacker-controllable input AND has a network path to
  upstream agents — so it must not also hold credentials it could
  exfiltrate to a compromised agent. The router MUST mint a fresh
  short-lived OIDC token via Vault role `agent-oidc-swap` per call,
  scoped to that agent. NEVER forward the inbound bearer.
  Source: substrate §2 invariant 7, G-10b, G-S7.

- **Cattle node behaviour: start clean, no local state.** A fresh
  Nomad allocation on a fresh node must come up and serve traffic
  without operator intervention. Do not assume any on-disk state
  (no `~/.triton`, no `/var/lib/triton`). All state is in-process or
  reachable over the network (Consul, Vault, OIDC).
  Source: substrate §2 invariant 4, G-8, FR-L-1.

- **Audit schema is `{who, what, when, env, result}` PLUS Triton
  fields.** The experiments emit `{protocol, tool, subject, tenant,
  latency_ms, status}` (`triton/dispatcher.py:91-101`). Production
  must emit a superset including the substrate schema. The collector
  parses by field name; adding fields is safe, omitting required
  ones breaks ingestion.
  Source: substrate G-3, `SPEC.md §11`, §13.8.

- **Two linked audit lines per call.** Inbound (at the dispatcher)
  + upstream (at the router), both carrying the same `trace_id`.
  The audit bucket carries the full causal chain. Forgetting the
  upstream line breaks the chain and silently degrades
  observability.
  Source: substrate G-13, FR-AU-1.

- **`/metrics` is tailnet-only.** Scraped from `tag:ops`. Do NOT
  expose it through Fabio. Bind it on a separate listener (a fourth
  port, or a separate axum router on the Tailscale interface).
  Source: substrate G-5, G-7.

- **Triton's allocation is light; don't request `kb-class`.** Unlike
  KB (model resident + per-tenant HNSW + DuckDB page cache),
  Triton holds no model, no per-tenant state, no caches beyond JWKS
  and the Consul/Vault token cache. Default Nomad client class
  suffices.
  Source: substrate §3a vs KB-S6, NFR-O-3.

- **Tailscale ACL is one-way: `tag:cli` → `tag:agents`.** Only
  Triton may reach agents; nothing else does. `tag:ops` does NOT
  reach `tag:agents` — operators reach Triton instead. This is
  asserted as an acceptance test (ACC-6).
  Source: substrate G-S5, Δ-2b.

- **Vault-minted upstream tokens MUST have TTL ≤ 5 minutes.** The
  lethal-trifecta cut depends on the token being short-lived enough
  that a compromised agent cannot exfiltrate it for later use.
  Source: NFR-S-3, substrate §2 invariant 7.

---

## 4. Cross-cutting traps

- **Do not retrofit an OpenTelemetry exporter "for tracing".** The
  audit lines + `trace_id` ARE the trace. Adding OTel introduces an
  outbound network path (or a self-hosted collector) that the
  substrate explicitly rules out in v1. If distributed tracing is
  ever needed, extend the audit schema, not the dependency list.
  Source: substrate §2 invariant 6.

- **Do not introduce a config file format.** CLI flags + env vars
  are the entire configuration story (mirrors experiments). A YAML
  config file would mean a hot-reload story, a schema, and a
  validator — all out of scope for v1.
  Source: NFR-O-1.

- **Do not add a "v3" adapter on the same listener.** Three TCP
  listeners is the design (ADR-1). Multiplexing protocols on one
  port complicates ACL boundaries and rate-limit policies, and
  breaks the protocol-isolation invariant the experiments validated.

- **Do not assume one Triton instance globally.** Nomad may schedule
  more than one allocation for HA; the alloc is stateless, so
  multiple instances behind Fabio are safe. But each instance
  caches JWKS and Vault tokens independently — design caches with
  per-instance scope, no inter-instance coordination.

- **Read both experiments before writing the corresponding Rust
  module.** The Python prototype shows the reference behaviour; the
  Rust port shows the idiomatic implementation. The substrate
  constraints fill in the production-grade requirements neither
  experiment had to satisfy. All three are needed for any non-trivial
  module.

---

## 5. Messenger-extension realizations (v0.2)

Lessons from the messenger follow-up at
`2026-05-23-triton-messengers/` and its impl workspace at
`2026-05-23-triton-messengers-impl/`. Each item is a trap the v0.2
production deployment can avoid by knowing about it.

- **A surface mapper, not a wider envelope.** Tempting to grow the
  A2UI envelope with platform-specific knobs (a Telegram inline
  keyboard variant, a Discord components-v2 variant, an Adaptive
  Card variant). Don't. The mapper at L6′ is one component; each
  adapter's projection rules live in its own `degrade` table in
  `adapter.yaml`. The envelope stays platform-neutral; the courier
  encodes platform-native bytes. This factoring is what makes the
  parity test (M-PARITY-MULTI-1) tractable — both adapters
  produce the same `PlatformMessage`; only the courier diverges.
  Source: messenger paper §8 (Surface mapping); ADR-12;
  `2026-05-23-triton-messengers-impl/crates/triton-chat-surface/src/mapper.rs`.

- **Two-record audit preserves trace_id symmetry across async
  paths.** The HTTP trio's dispatcher + upstream-router pair was
  the original design; chat-channel adapters add an outbound
  courier that emits the second record. The audit query
  `WHERE trace_id = X` returns the complete exchange — the
  dispatcher record (phase=dispatch, status=accepted) and the
  courier record (phase=post, status ∈ {posted, retry, dropped}).
  Forgetting the courier record breaks the chain and silently
  degrades observability. Source: messenger paper §4, §7; ADR-15;
  M-ASYNC-1.

- **YAML over TOML for the declarative manifest.** The v0.2 manifest
  is YAML. TOML's section-header style (`[adapter.discord.inbound.gateway]`)
  fights the nested adapter shape; YAML's nested-mapping style reads
  more naturally. YAML also aligns with the surrounding ecosystem
  (Kubernetes manifests, GitHub Actions workflows, Ansible
  playbooks), so operators are already comfortable. The Rust
  loader uses `serde_yaml_neo`; the Python paper-time verifier
  uses PyYAML. Source: messenger paper §6 (Declarative adapter
  manifests); ADR-13; `2026-05-23-triton-messengers/manifest-example.yaml`.

- **Enforce surface caps at the mapper edge, not at the courier.**
  Discord's 25-item select cap, Telegram's 8-buttons-per-row
  pragmatic cap, WhatsApp Web's 4000-char text chunk — all of
  these belong in the mapper as `SurfaceLimits` consts; the
  courier sees a `PlatformMessage` that's already cap-compliant.
  This is the cheapest place to enforce: the mapper rejects (for
  selects), paginates (for buttons), or chunks (for text) before
  the courier holds a platform credential. Source: messenger paper
  §8; M-RICHNESS-1;
  `2026-05-23-triton-messengers-impl/crates/triton-chat-surface/src/mapper.rs`
  (`SurfaceLimits::{TELEGRAM, DISCORD, WHATSAPP_WEB}`).

- **Dashboard rasterisation is out-of-process, not embedded.** Do
  not link a headless browser into the Triton binary. The
  `Rasterizer` trait is consumed by the mapper as an injected
  dependency; in production it's an upstream tool named
  `render_a2ui_to_png` (preferred — inherits identity + audit
  symmetry through the upstream router) or a peer Nomad sidecar.
  This keeps the binary small, the dependency graph clean, and
  rasterisation horizontally scalable independent of the gateway.
  Source: messenger paper §8 (Dashboard components); M-RASTER-1;
  FR-A-11.

- **HMAC-signed correlation tokens, not server-side conversation
  state.** Every interactive option emitted by the mapper carries
  a token that encodes the follow-up `(tool, args)` pair, signed
  under the adapter's `CorrelationKey`. The inbound listener
  verifies the HMAC on the follow-up event and recovers the pair.
  The platform never sees the tool name; the dispatcher receives
  a verified triple. The alternative — server-side conversation
  state keyed by message id — re-introduces session affinity and
  breaks the stateless re-render property (ADR-5). Source:
  messenger paper §7 (Identity, threading, async reply);
  M-CORRELATION-1; `crates/triton-chat-surface/src/correlation.rs`.

- **Loopback refusal for signal-cli is non-negotiable.** The
  signal-cli bridge terminates Signal end-to-end encryption inside
  the trust boundary of the Triton allocation. Accepting an
  external bridge (`tcp://10.0.0.5:7583`) is equivalent to
  accepting plaintext from a third party. The refusal lands at
  adapter construction (`SignalAdapter::new`), before any socket
  I/O; the gateway exits with a documented error and `/healthz`
  never returns 200. Source: messenger paper §5 (Signal); C-11,
  FR-I-9, NFR-S-6; M-LOCALITY-1;
  `crates/chat-adapter-signal/src/lib.rs::validate_loopback`.

- **WhatsApp Web is the canonical WhatsApp adapter; Cloud API is
  a manifest extension.** The Baileys-style WhatsApp Web socket is
  community-maintained and breaks under platform changes
  periodically; pin the protocol-fragment version per adapter,
  treat live-test failure as a flag for protocol re-validation.
  Operators with B2B compliance requirements add a second
  `kind: whatsapp_cloud` adapter under the same manifest schema —
  delivered in #94 (templates + interactive buttons/lists), distinct
  from the `whatsapp_web` socket kind. Source: messenger paper §5
  (WhatsApp).

- **Closed-set boot validation catches misconfiguration before
  socket I/O.** Three Phase-A checks gate the gateway boot:
  M-MANIFEST-1 (no foreign top-level / kind / `degrade` keys),
  M-COVERAGE-1 (every tool's `surface_components` covered by every
  chat-channel adapter's `degrade` table), M-SECRETS-1 (every
  credential field is a `vault://` reference). All three are
  implemented in `2026-05-23-triton-messengers/tools/verify_manifest.py`
  and are mirrored by FR-L-4, FR-L-5, FR-L-6 in this spec. Source:
  M-MANIFEST-1, M-COVERAGE-1, M-SECRETS-1; ACC-9.

---

## 6. Cross-cutting traps (v0.2 update)

- **Do not retain the v0.1 "no config files" stance for v0.2.**
  NFR-O-1 explicitly admits `adapter.yaml` for the chat-channel
  surface area; the v0.1 cross-cutting trap "do not introduce a
  config file format" applied to the HTTP-trio binary in scope
  for v0.1 only. The manifest exists; it has a closed-set
  validator; it has a YAML schema sketch in `requirements.md` §9
  and a worked example at
  `2026-05-23-triton-messengers/manifest-example.yaml`.

- **Do not collapse the inbound listener and the outbound courier
  into one component for v0.2 adapters.** Chat platforms separate
  the inbound event from the outbound platform call; the courier
  holds the platform credential and is the only component that
  does. The dispatcher sits between the two halves and remains the
  single audit pivot. Collapsing them re-introduces ambient
  credentials in inbound code paths and breaks the
  lethal-trifecta cut. Source: ADR-11; messenger paper §4.

- **Do not put the `degrade` rule table on the tool.** It's a
  per-adapter table, not a per-tool table. A tool that emits a
  button-row component does so in a platform-neutral way; the
  *adapter* decides whether buttons project to inline keyboards
  (Telegram), components v2 (Discord), Adaptive Card actions
  (Teams), or numbered prompts (Signal / WhatsApp Web / Google
  Chat). Putting the projection rule on the tool would break the
  cross-channel parity property (M-MAP-1). Source: messenger
  paper §6; ADR-12.

---

## 7. Production-implementation gotchas

Items discovered while building the production Rust port; each is
a trap the next developer should not have to step in.

- **MCP-Apps `_meta.ui.*` rides on the tool *result*, not the dispatch
  envelope — and it must be lifted, not just preserved.** (#143 A) An
  upstream renderer returns its `ui://` resource link under
  `result._meta.ui.resourceUri`. Triton's MCP `tools/call` already kept the
  whole result intact inside `structuredContent.result`, so the data was
  technically there — but a host reads the resource link from the
  *response* `_meta`, where it wasn't. Lift `result._meta.ui` onto the
  `tools/call` response `_meta` (next to `trace_id`), and copy the whole
  `ui` object so unknown `ui.*` siblings a newer host understands aren't
  silently dropped. Capture it *before* any A2UI wrap rewrites the result.

- **A `ui://<authority>/…` resource is routed by reusing the tool
  registry, so the authority must be registered as its own key.** (#143 B)
  `resources/read ui://peacock/r1` resolves `peacock` through the same
  `TRITON_STATIC_UPSTREAMS` map as tool dispatch — there is no separate
  owner→endpoint table. So an operator whose `peacock` upstream owns the
  tool `render_report` must register *both* keys at the same endpoint:
  `render_report=host:port,peacock=host:port`. The breaker for proxied
  `resources/read` / `updateModelContext` is keyed on the *authority*
  (`peacock`), a different slot from the tool-name breaker — intentional,
  but don't expect a tripped `render_report` breaker to also stop reads.

- **`callServerTool` needs no Triton code; `updateModelContext` must not be
  inspected.** (#143 C) An in-iframe `callServerTool` reaches Triton as a
  plain `tools/call` (the host translates it), so the stateless re-render
  contract is just the existing dispatch path — resist adding a bespoke
  method. `updateModelContext`, by contrast, is relayed to the owning
  upstream *verbatim*: route by the `uri` authority only, POST the `record`
  as the body untouched. The moment you `serde` the record into a typed
  struct you risk dropping a field a newer renderer added — keep it a raw
  `Value` passthrough.

- **A returned `_meta.ui.resourceUri` is a confused-deputy surface, but a
  bounded one.** (#143 review) Upstream A's tool result can name
  `ui://B/...`; when the host then calls `resources/read`, Triton routes it
  to upstream B with the caller's principal. This does *not* cross a
  privilege boundary in today's model — every upstream is operator-pinned
  in `TRITON_STATIC_UPSTREAMS`, and any authenticated caller can already
  call any registered tool directly — so it's accepted, not fixed. The
  cheap defence we *do* apply: only reflect `_meta.ui` when it's a JSON
  object (refuse a scalar/array blob), so a hostile upstream can't bloat
  the response through that key. If a per-caller tool ACL ever lands, the
  reflected authority must be re-bound to the tool's owning upstream.

- **A delegated renderer needs the sidecar's size cap re-applied by hand.**
  (#143 review) The sidecar `Client::render` caps the response body at
  `MAX_RESPONSE_BYTES` (2 MiB). The delegated path buffers the upstream
  JSON via `resp.json()` (no cap — a pre-existing trait of *every* upstream
  call) and then base64-decodes `png_base64`. `UpstreamRasterizer` re-adds
  the cap on both the encoded string and the decoded bytes; without it a
  broken/hostile renderer forces two large allocations. If you add more
  delegated-binary tools, port this guard — `resp.json()` won't do it.

- **Delegating rasterisation to an upstream means the PNG crosses a JSON
  boundary, so it's base64.** (#143 D) The sidecar returns `image/png`
  bytes directly; an upstream `render_a2ui_to_png` tool returns a JSON tool
  result, so the bytes ride as `png_base64`. The `DashboardRasterizer`
  trait hides which path is in use, but its `render` had to grow a
  `&Principal` arg the sidecar ignores — the upstream path needs it to mint
  the per-call token. When delegating, skip the sidecar-URL egress check:
  the upstream endpoint is already SSRF-guarded at boot by the
  static-upstreams allowlist, and re-checking a URL that isn't used would
  reject a perfectly good config.

- **"Flaky" CI that loses the runner mid-`cargo test --workspace` is an
  OOM during the test *link*, not a flaky test.** Symptom: the Rust job
  is marked `failure` with the cargo steps at `conclusion: null` (runner
  lost, not a non-zero exit), it dies ~1–3 min in with **no test output**
  and an empty failure log, and a re-run usually passes. Root cause: the
  workspace built ~58 *separate* `triton-tests` integration binaries, each
  statically linking the full dep tree (reqwest+rustls, axum, tokio,
  jsonwebtoken, every chat crate, the rasterizer…) with cargo's default
  `debug = 2` (full DWARF) → **80–180 MB binaries**; linking several in
  parallel (`-j4` on the 16 GB public runner) spikes RAM past the ceiling
  and the OOM killer reaps the runner agent. It's intermittent because it
  depends on whether two heavy links overlap at peak. It even hit a
  *frontend-only* PR, which is the tell: the cost is in the workspace
  compile/link, independent of what changed. Three fixes, applied
  together:
  1. **Slim dev/test debuginfo** — `[profile.dev] debug =
     "line-tables-only"` + `split-debuginfo = "unpacked"`. Keeps
     `file:line` in panic backtraces (what we use) while cutting binary
     size **~70%** (triton 180→53 MB; a test binary 90→23 MB) and link
     memory with it.
  2. **Bound CI build memory** — `CARGO_INCREMENTAL=0` (useless on an
     ephemeral runner, bloats `target`+RAM) and `CARGO_BUILD_JOBS=2` (cap
     concurrent links) in the job `env`.
  3. **One integration binary, not 58** — `triton-tests` sets
     `autotests = false` and aggregates every `tests/*.rs` into a single
     `it` binary (`tests/it/main.rs` `#[path]`-`mod`s each file). The
     workspace test build now does ONE fat link instead of 58. Safe
     because the files have no in-process `std::env::set_var` (would race
     when run in one process) and no crate-root-only inner attributes;
     verified by the full suite (255 tests) passing in the single binary.
  To add an integration test: drop a `tests/<name>.rs` and add a `mod`
  line to `tests/it/main.rs` (it won't be picked up otherwise —
  `autotests = false`).

- **Streaming (SSE) breaks "audit at call end" — fix it with a
  drop-aware finalizer, not by auditing at first byte.** When a tool
  result streams (issue #132), the outcome (clean done / mid-stream
  error / client disconnect / upstream truncation) is only known when
  the stream *closes*, which may be long after the HTTP 200 headers
  flushed. To keep ADR-6's "exactly one dispatch audit line", wrap the
  event stream in a combinator (`triton_core::stream::Finalized`) that
  holds the finalizer in an `Option` and fires it from whichever of
  {terminal `Done`/`Error` item, inner-`None`, `Drop`} happens first —
  so it can never double-fire or zero-fire. Client disconnect is the
  `Drop`-still-armed case; axum drops the response body's stream when
  the peer goes away. The finalizer **must be the outermost** layer
  (after any A2UI wrapping) or a disconnect drops an inner layer first
  and never reaches it.
- **A `Drop` finalizer cannot `.await`** — so anything it touches must
  be sync. The per-tool circuit breaker map was `RwLock<HashMap<_,
  Mutex<Breaker>>>`; the streaming path pre-resolves the slot to an
  `Arc<Mutex<Breaker>>` *before* building the stream, so the finalizer
  only takes a `std::sync::Mutex` (no async lookup) when it fires.
- **`Accept: text/event-stream` is offered to the agent, but a plain
  agent answers with JSON.** `StaticUpstream::invoke_streaming` always
  sends the SSE `Accept`, but a non-streaming agent ignores it and
  replies `application/json`. Branch on the *response* content-type:
  decode SSE only when the agent actually streamed, otherwise buffer
  the JSON body into a single terminal `done`. Forgetting this makes a
  buffered upstream return an empty SSE body (zero frames → truncated).
- **Pre-first-byte vs mid-stream errors split the error contract.** An
  error known before the upstream's 200 (open breaker, connect fail,
  non-2xx, unknown tool) returns `Err` from `invoke_streaming` →
  ordinary HTTP error response + inline audit. An error *after* 200
  rides as a terminal `event: error` frame (status already sent) and
  audits at termination. `invoke_streaming` returning
  `Result<Stream, _>` is what makes the boundary expressible.
- **Dart (Explorer): don't `.transform(utf8.decoder)` a Dio stream
  body.** Dio's `ResponseType.stream` body is `Stream<Uint8List>`;
  `utf8.decoder` is a `StreamTransformer<List<int>, String>`, and the
  runtime rejects the variance (`Utf8Decoder is not a subtype of
  StreamTransformer<Uint8List, String>`). Buffer the raw bytes, find the
  ASCII frame boundaries (`\n\n` / `\r\n\r\n`) in the byte buffer, and
  `utf8.decode` each complete block — which also avoids splitting a
  multi-byte char across chunk boundaries. See
  `apps/explorer/lib/api/sse.dart`.

- **`cargo test -p triton-tests --test <name>` does not necessarily
  rebuild `target/debug/triton`.** The integration-test crate doesn't
  declare `triton-bin` as a Rust dependency (it discovers the binary
  at runtime via path lookup), so cargo's dirty-tracking can leave a
  stale binary in place when only `triton-core` or `triton-bin`
  source changed. Symptom: a red test hits `404` (or worse, an old
  bug appears to come back). Mitigation: run `cargo build` (or
  `cargo test --workspace`) before iterating on a single integration
  test. (A `build.rs` in `triton-tests` that runs
  `cargo build --bin triton` recursively hits a target-lock deadlock
  inside cargo — not worth doing.) Discovered in PR 4.

  **Self-heal (issue #132 follow-up).** This trap masqueraded as a
  *flaky* test: a `triton` binary built between two features silently
  omits the newer one, so an assertion fails for the wrong reason (the
  symptom that bit `forward_principal` — a minted token missing its
  `groups` claim because the spawned binary predated #131). The harness
  now guards against it in `triton_binary_path` →
  `ensure_fresh_binary`: a cheap mtime pre-check (binary newer than every
  production `*.rs` under `crates/`, excluding `triton-tests` itself →
  fresh, zero overhead — the `--workspace` case), and on *suspected*
  staleness it runs `cargo build -p triton-bin` once per test process.
  Crucially it defers the actual decision to cargo's **content-hash**
  fingerprint, so a bare `touch`/`git checkout` that bumps mtime without
  changing bytes is a fast no-op rather than a false failure. The
  build.rs deadlock above doesn't apply — this runs at test *runtime*,
  after cargo has released the compile lock.

- **Integration-test harness MUST prefer `target/debug/triton` over
  `target/release/triton`.** `cargo test` rebuilds the debug binary;
  the release binary is whatever was last produced by
  `cargo build --release` (often weeks/months out of date from a
  manual smoke test). A harness that picks release first will run
  silently stale code while the developer thinks they're testing the
  latest changes — the resulting failure modes look like cosmic-ray
  bugs ("but the binary clearly has X!"). Fix in
  `crates/triton-tests/src/lib.rs::triton_binary_path` is to swap the
  order: try debug first, fall back to release. Discovered in PR 4
  after ~45 minutes of confused debugging.

- **`jsonwebtoken::Validation::algorithms` MUST be a single-element
  list matching the JWT's `header.alg`.** A multi-family allowlist
  (e.g. `[RS256, RS384, RS512, ES256, ES384, EdDSA]`) causes
  `decode` to return `InvalidAlgorithm` for EdDSA tokens even though
  EdDSA is in the list — confirmed against jsonwebtoken 9.3.1. The
  algorithm allowlist (FR-I-3) MUST be enforced as a separate
  `ALLOWED_ALGS.contains(&header.alg)` check BEFORE constructing
  `Validation`; then `Validation::new(header.alg)` keeps its default
  one-element `algorithms` and `decode` succeeds. Do not widen
  `validation.algorithms` for "defence in depth" — the up-front
  allowlist check is the defence. Discovered in PR 8 after ~30
  minutes of confused debugging.

- **Ephemeral-port probe + spawn races under heavy `cargo test`
  parallelism.** `free_tcp_port()` opens a listener on `127.0.0.1:0`,
  reads `local_addr()`, then closes the socket — so the port is
  released back to the kernel before the child gets a chance to
  bind. With N test threads × 3 ports each, two harnesses can pick
  the same port; whichever child binds second exits with
  `AddrInUse`. Symptom: a previously-green test fails sporadically
  with `Error: Os { code: 98, kind: AddrInUse, ... }` on stderr and
  `triton not ready within 5s` on the assertion side. Mitigation in
  `TritonProcess::spawn_with_args`: detect early-exit via
  `try_wait` during the readiness loop and retry up to 5 times with
  fresh ports + brief backoff. A proper fix would have the binary
  bind `127.0.0.1:0` and report the bound ports on stdout — deferred
  until we have a use case (PR 9 upstream router stubs may need it).
  Discovered in PR 5.

- **For chat webhooks, never put the body parse (axum's `Json<T>`,
  `Form<T>`, etc.) ahead of the signature check.** Axum extractors
  run in argument order and short-circuit on extraction failure with
  axum's own status — so a request with a malformed body and a wrong
  signature is rejected by the *extractor* before the handler ever
  runs, which means no `phase: rejected` audit line is emitted and
  the attacker has confirmed that the route exists without ever
  touching our auth code. For inbound chat adapters (FR-I-8 / FR-AU-1),
  take the body as `axum::body::Bytes`, verify the signature against
  raw bytes first, then `serde_json::from_slice` and audit any parse
  failure as `Validation` so it lands on the same audit pivot.
  Codex flagged this in PR 13 review; closed by switching the
  Telegram handler from `Json(update)` to `body: Bytes` + manual
  parse.

- **Constant-time header equality must run over a fixed scratch
  buffer, not over `min(presented, configured)`.** The naive
  `if a.len() != b.len() { return false }` early-out and the
  `a[..n].ct_eq(&b[..n])` "compare-common-prefix-then-AND-with-length"
  variant both create a secret-length oracle: an attacker controlling
  the header can sweep `presented.len()` and see handler latency
  grow until it plateaus at `configured.len()`. For FR-I-8 closure,
  copy both sides into zero-padded fixed-size buffers (size = the
  documented platform max — Telegram says 1..=256 for its
  secret_token header) and ct_eq over the full buffer. Enforce the
  max at boot so the configured side never gets truncated.
  Discovered in PR 13 from Codex's second blocker.

- **An adapter that needs Vault before the Vault resolver exists must
  warn-and-skip, not exit non-zero.** PR 13 wires the Telegram
  webhook against literal manifest secrets; the production
  `manifest-valid.yaml` fixture (and any realistic substrate
  manifest) carries `vault://...` refs. If the boot wiring treats
  "credential is a Vault ref" as a fatal `BuildError`, the binary
  refuses to boot the moment a single Vault-ref adapter appears,
  blocking every test that uses the canonical fixture even though
  the *other* adapters in the manifest are perfectly serviceable.
  Encode the carve-out in the adapter's `BuildError` enum
  (`VaultUnsupported(&'static str)` distinct from
  `MissingCredential` / `Unsupported`) so main.rs can match on it
  explicitly: log a warning naming the field, skip wiring that
  adapter, continue. PR 14 will resolve the Vault ref and lift the
  carve-out. Discovered in PR 13 — the failing test was the existing
  `binary_boots_with_valid_manifest`, which I had to keep green
  while the new wiring was added. **Status: lifted in PR 16** —
  `BuildError::VaultUnsupported` was removed when the resolver
  landed; any unresolved Vault ref now exits the binary non-zero,
  closing Codex's "fail-closed" concern.

- **When a piece of substrate config gets a second consumer,
  partial-wiring rules need re-examining.** PR 9 wired Vault for
  the upstream router's OIDC swap and treated
  `TRITON_VAULT_URL` + `_TOKEN` as inseparable from
  `TRITON_CONSUL_URL` — all three or none. PR 16 added a second
  consumer (the secret resolver) that needs only Vault. Without
  loosening the rule, every chat-only deploy would have had to set
  a `TRITON_CONSUL_URL` it never uses. Specifically: keep
  `consul without vault` and `vault_url without vault_token`
  fatal; allow `vault_url + vault_token` alone (resolver-only
  mode, no upstream router). Document the matrix in a comment
  next to the match so the next consumer doesn't reset it.
  Discovered in PR 16 when `binary_refuses_boot_when_vault_unreachable`
  passed on the no-consul path but `webhook_authenticates_with_vault_resolved_secret`
  failed because the binary refused to boot for "missing consul".

- **A chat-channel post-back failure MUST NOT fail the inbound
  ack.** The inbound webhook contract with Telegram is "did you
  receive the message and decide what to do with it" — that's
  already true the moment the dispatcher returns. Whether the
  reply made it back to the user is a separate, independent
  question, and a failure there ("api.telegram.org returned 500"
  or "DNS dead") is *not* something the platform should retry the
  inbound for. If the courier failure bubbled into the inbound
  status, Telegram would replay the same update for ~24 h and we'd
  dispatch the same tool again on every retry — double-charged
  side effects, more audit lines than there were messages. The
  right shape: dispatcher → 200 OK to the webhook → courier fires
  → `phase: post` audits the outcome (success OR failure). The
  inbound ack and the post-back live on separate axes. Codex
  flagged this in PR 13's concern about retry storms; PR 18
  locks it in with the `post_failure_audits_provider_error_does_not_fail_inbound_ack`
  test.

- **The dispatcher stays the single audit pivot even for
  side-effect-only phases.** When PR 18 added the outbound
  courier, the temptation was to let the adapter emit its own
  `phase: post` line — the dispatcher isn't involved in the HTTP
  POST. Don't. ADR-6's contract is "all audit lines are
  constructed by the dispatcher" so the schema can never drift
  between phases. New phase → new dispatcher method
  (`record_post`), adapter passes outcome + latency + principal,
  dispatcher does the construction + emit. Cheap; keeps the
  schema invariant intact.

- **`reqwest::Error::Display` includes the request URL.** This
  is a latent FR-AU-3 violation any time a secret lives in the
  URL path — and the Telegram Bot API puts the bot token there
  (`/bot{token}/sendMessage`). A naive
  `tracing::warn!(error = %reqwest_error)` will print the URL,
  including the token, on every transport failure. Always wrap
  the error in a local enum whose `Display` only carries
  redacted text, and run a final `s.replace(secret, "<redacted>")`
  belt-and-braces before the error reaches the audit/log
  pipeline. Codex flagged this in PR 18 review; lock-in test is
  `bot_token_never_leaks_into_courier_failure_logs`.

- **2xx HTTP doesn't mean "success" for Bot-API-style RPCs.**
  Telegram's Bot API (and Discord's, and Slack's) returns
  `200 OK` with a body envelope `{ok: bool, ...}`. A response
  saying `{ok: false, error_code: 429, parameters: {retry_after: N}}`
  arrives as HTTP 200 and would be classified as "posted" by any
  courier that only checks `status.is_success()`. Always parse
  the application envelope and require the `ok` field. Codex
  PR 18 blocker 2; locked in by `bot_api_200_with_ok_false_*` tests.

- **Configurable external endpoints need an env-gated allowlist.**
  `TRITON_TELEGRAM_API_BASE` exists for tests to point the
  courier at a `FakeTelegramApi`. In a non-`local` environment
  the same env var would be a free SSRF/exfil channel: every
  tool reply and the bot token's URL path get POSTed to wherever
  the operator (or attacker who controls the env) names. The
  fix is one comparison: outside `local`, refuse any
  api_base ≠ `https://api.telegram.org`. NFR-S-4 v0.2 spells out
  the egress allowlist; mirror it as a boot guard. Codex PR 18
  blocker 3.

- **FR-AU-1 v0.2 closed sets need their own audit field.** The
  spec says chat post audits carry a `status` from
  `{posted, retry, dropped}`, but the existing schema's `status`
  is `u16` HTTP. Don't overload — add a sibling `status_label:
  Option<&str>` field with `skip_serializing_if = "Option::is_none"`
  so non-chat-post phases stay the same on the wire, and chat
  post phases get the closed-set discriminator. The map is
  `posted` for `{ok: true}`, `retry` for transport/decode
  failures + `error_code == 429` + `error_code >= 500` +
  explicit `retry_after`, `dropped` for the rest. Cemented in
  PR 18 from Codex's nit.

- **HTML parse_mode means every user-controlled string must be
  HTML-escaped, full stop.** Telegram's `sendMessage` with
  `parse_mode: "HTML"` will 400 the whole request on any stray
  `<`, `>`, or `&` — and worse, a tool that emitted something
  like `<a href=...>` would inject markup the operator never
  authored. The surface mapper escapes EVERY text fragment
  (Text *and* Narration's interior) before wrapping with `<i>`.
  Order matters: `&` first, then `<` and `>`, or you'd
  double-escape entities you just produced. Locked in by
  `html_special_chars_in_tool_output_are_escaped`. The same
  rule applies to Discord's Markdown parse_mode (PR shipping
  Discord must do the equivalent escape; copy this approach).

- **Surface mapper components that can't ship yet must be
  audibly deferred, not silently dropped.** Button components
  need HMAC correlation tokens (Telegram's `callback_data` is
  ≤64 bytes; arbitrary `(tool, args)` won't fit). PR 19 doesn't
  ship them, but it counts them and emits a `tracing::warn` line
  with `deferred_buttons = N` per dispatch so the operator sees
  how often the gap matters in practice. The test
  `unsupported_components_are_logged_not_silently_dropped`
  asserts the warning text. Same pattern for every later
  component (selection, form, dashboard) until its mapping ships.
  Wording note: this is *logged*, not *audited* (no audit-record
  field carries the deferral count). If we ever need the count in
  the audit pivot, add it to `AuditRecord` rather than overloading
  the log channel.

- **Cap enforcement belongs at the mapper, not at the courier.**
  Codex PR 19 review caught this: PR 19 let oversized text and
  empty surfaces reach Telegram, then leaned on the courier's
  HTTP-400 path to classify them as `dropped`. That's a worse
  shape — it wastes an API call per violation, attributes the
  failure to the wrong layer, and breaks the L6′ contract
  (architecture.md §8.7: `Surface mapper rejects with
  UnsupportedSurface at the mapper edge before the courier sees
  the envelope`). PR 20 moves both checks to the mapper:
  - Empty surface → `RenderError::EmptyAfterRender`; caller
    records `phase: post, status_label: dropped` and skips the
    courier call entirely.
  - Text > 4096 bytes → truncate at a UTF-8 boundary, append a
    visible sentinel, set `truncated: true` on `RenderedMessage`
    so the caller can `tracing::warn`. Truncation is preferable
    to outright rejection here because the upstream tool might
    have produced something useful in the first 4 KB.
  Locked in by `empty_surface_is_dropped_at_mapper_edge_no_courier_call`
  (which also asserts the FakeTelegramApi captures zero requests)
  and the unit tests on `enforce_text_cap`.

- **UTF-8 boundary discipline when truncating user-supplied text.**
  Telegram's 4096-char cap is bytes-of-UTF-8 in practice. A naive
  `text.truncate(4096)` panics on a multi-byte char boundary; a
  naive `&text[..4096]` panics with `byte index 4096 is not a
  char boundary`. The right shape is a walk-back loop: try
  `end = budget`, decrement until `text.is_char_boundary(end)`.
  Test `truncation_preserves_utf8_boundaries` uses a 4-byte
  codepoint (`𝄞`) to verify the cut never lands mid-sequence.

- **Truncating rendered HTML can break it; truncate raw text
  instead.** Codex PR 20 review caught this: PR 20's first pass
  truncated the *rendered* string (`<i>...</i>` with `&lt;`
  entities already inlined), so a cut could land mid-tag or
  mid-entity and produce HTML Telegram rejects. The replacement
  approach:
  1. Render each component to its own complete HTML chunk.
  2. Truncation cuts only between chunks (always on the `\n\n`
     separator), so no chunk is ever split mid-tag or mid-entity.
  3. If even the first chunk exceeds the cap, truncate its
     *raw* text before HTML-escape and re-render. Use per-char
     escape-cost accounting (`&` → 5 bytes, `<`/`>` → 4 bytes,
     others → UTF-8 byte length) so a string of `&` chars
     doesn't blow up unpredictably when escaped.
  Tests `truncation_never_splits_html_entities`,
  `truncation_keeps_italic_tags_balanced`, and
  `truncation_drops_tail_components_when_head_fits` lock in
  every branch.

- **`text.strip_prefix('/').and_then(split_once)` silently drops
  no-arg commands.** PR 19's `/narrate` without a space fell
  through to echo because `split_once(' ')` returned None. Use
  `.unwrap_or((rest, ""))` so the entire remainder is treated as
  the command and the args default to empty. Then a match on the
  command routes to the right tool (with empty subject for the
  narrate case) instead of leaking to a different tool entirely.

- **Telegram's 64-byte `callback_data` is the binding constraint
  for HMAC correlation tokens.** Architecture.md §8.7 specifies
  `b64url(JSON({tool, args})) || "." || b64url(HMAC-SHA256(body, key))`
  but doesn't pick a truncation length. Full SHA-256 = 32 bytes →
  43 b64url chars, more than half the 64 budget; minimal tool+args
  (`{"tool":"narrate","args":{"subject":"alice"}}` = 43 bytes raw
  → 58 b64url chars) already overflows. PR 21 closes both by
  (a) using short keys `{"t","a"}` so the dispatcher decoder
  re-expands the names, and (b) truncating the HMAC to 8 bytes
  (64-bit). 64-bit auth tags under per-adapter rate limits
  (default ≤50 msg/s) still need ≥ 2^32 forge attempts on average
  — 1000+ years. Same security territory as Stripe webhook
  signatures. If a tool+args still won't fit, the encoder returns
  `EncodeError::OversizedToken` and the mapper defers the button
  via `deferred_buttons` (instead of ever shipping an oversized
  callback_data to Telegram and 400-ing mid-traffic).

- **Constant-time HMAC verify even when the presented token has
  the wrong length.** Decoder splits on `.`, base64-decodes both
  halves, then HMAC checks. Early-return on shape errors (no
  dot, bad base64) is fine — those leak nothing about the secret.
  For the HMAC compare itself: always compute the full HMAC over
  the body and ct_eq even when the presented MAC has the wrong
  length (pad to expected). Mirrors PR 13's secret-token
  approach. Gotcha when writing tests: flipping just the LAST
  char of an 11-char b64url HMAC is NOT a reliable corruption —
  base64-no-pad ignores the unused bits of the trailing char so
  the decoded byte may not change. Decode → flip → re-encode, or
  flip a non-trailing char.

- **The principal comes from the sender, never from the
  correlation token.** Architecture.md §8.7 says "the platform
  never sees the tool name or args directly; the dispatcher
  receives a verified `(tool, args, principal)` triple." PR 21
  is intentional about WHERE the principal comes from:
  `callback_query.from.id` → `sender_table` lookup, same shape
  as the inbound message path. The token only carries
  `(tool, args)`, never `sub` or `tenant`. A hostile platform
  actor who replays a stolen token cannot impersonate a different
  user because the sender_table lookup is independent of the
  token's contents. Locked in by
  `forged_callback_token_is_rejected_with_phase_rejected` and the
  always-from-sender principal construction in
  `handle_callback_query`.

- **Discord's synchronous interaction-response model collapses
  outbound into inbound.** Unlike Telegram (webhook → ack → POST
  back to api.telegram.org for the reply), Discord's webhook
  expects the channel-message body inline in the HTTP response:
  `{type: 4, data: {content, components}}`. The audit shape stays
  the same — `phase: dispatch` for the tool invocation, `phase:
  post` for the response body we returned — but `record_post`
  fires synchronously with `status_label: "posted"` once the body
  is built (no separate API call to wait on). Future-note for the
  next adapter: check the platform's reply semantics before
  copying Telegram's outbound courier pattern. Discord, Slack
  slash commands, and Discord modal-submits all use the
  inline-response model; Telegram/Signal/WhatsApp use the
  callback-API model.

- **Ed25519 signature verification needs a timestamp-skew guard
  alongside the constant-time signature check.** Discord docs say
  every interaction carries `X-Signature-Timestamp`; verifying
  the signature alone admits replays of stale requests captured
  by a network attacker. PR 22 adds a 5-minute skew window
  (`MAX_TIMESTAMP_SKEW_SECS = 300`), the same number Discord's
  own webhook docs recommend. The skew computation must use
  `saturating_sub` both directions so a clock-drifted future
  timestamp doesn't underflow into "fine" silently.

- **A canonical `manifest-valid.yaml` fixture means every newly
  wired adapter MUST get realistic test data in that fixture's
  KV table.** PR 22's Discord wiring broke
  `binary_boots_with_valid_manifest` because the fixture's
  `public_key: "discord-public-key"` had been a free-form string
  placeholder ever since PR 16 — when only Telegram actually
  parsed its credentials, Discord's stayed unread. Lesson: when
  shipping a new adapter, audit the canonical-fixture KV data
  against the adapter's parse rules and update placeholder
  values that would now fail. The RFC 8032 Test 1 Ed25519
  public key is a useful canonical value here because it's
  traceable to a public spec.

- **The numbered_prompts state machine lives on the adapter,
  not behind the dispatcher.** PR 32 first sketched putting the
  per-chat form state on the dispatcher (so every chat adapter
  could share it). That broke ADR-6 in subtle ways: the
  dispatcher is the single audit pivot, but form-prompt
  sendMessages are *adapter* events with *no underlying tool
  dispatch* (the user is mid-form; nothing has run yet). Forcing
  those into the dispatcher's audit shape meant inventing a
  synthetic tool name and a synthetic dispatch phase — both
  worse than the v0.2 status quo of `phase: post` lines that
  the adapter constructs via `record_post(tool_name="form_prompt", …)`.
  Keep state on the adapter; let the audit shape stay honest;
  `record_post` already accepts arbitrary `tool_name` strings.

- **`(chat_id, sender_sub)` is the only form-state key shape
  that survives sender_table reloads.** Keying just by `chat_id`
  loses isolation when two senders share a chat. Keying just by
  platform `from.id` loses isolation when the sender_table
  reload swaps the sub → tenant resolution mid-form (you'd
  apply the OLD tenant's policy to the NEW sender's submission).
  Keying by `(chat_id, sub)` makes the lookup follow the
  verified identity, and re-deriving the principal from the
  sender_table at submit time means the latest manifest's
  authorisation always wins. PR 32 discovered this when writing
  the integration test that runs two senders in two chats
  simultaneously.

- **In-memory adapter state needs a per-tenant cap and an LRU
  eviction story, not just a global limit.** G-8 forbids on-disk
  state, so a noisy tenant with infinite in-flight forms is the
  default OOM surface. Cap per-tenant (defaults to 100 in PR 32),
  evict oldest on overflow, audit the eviction as
  `phase: rejected, result: error:validation` so the operator
  notices the noisy tenant. Global-only caps starve well-behaved
  tenants when one tenant misbehaves; per-tenant caps + LRU
  give every tenant a steady-state quota independent of
  neighbours.

- **The harness `pub` surface is the consumer-test contract,
  not an internal artefact.** When you rename
  `FakeVault::start_kv_v2` or change `TritonProcess::spawn_with_env`'s
  signature, you're silently breaking every downstream app's CI
  that depends on `triton-tests`. The Rust compiler will not tell
  you — the downstream crates live outside this workspace, and
  workspace CI never compiles them. Run the ADR-16 deprecation
  cycle (one release of `#[deprecated]` before removal) for any
  breaking change to the `pub` items listed in FR-T-2. Discovery
  aid for later: a `cargo public-api` snapshot in CI would catch
  accidental breakage at PR time; not wired up yet, follow-up
  when the surface starts moving fast enough to need it.

- **NFR-S-4 egress checks belong next to the adapter that needs
  the egress, not at process boot.** PR 36's rasterizer URL has
  the same shape as `TRITON_TELEGRAM_API_BASE` — outside `local`
  env it MUST point at a tailnet hostname. The first version of
  the check ran in `main` before any adapter was wired, which
  broke every existing integration test using `TRITON_ENV=nonprod`
  to exercise OIDC — those tests don't enable chat adapters and
  shouldn't care about a rasterizer URL they'll never call.
  Lesson: gate every egress allowlist check on "is this network
  dependency actually being used?" The check now lives inside
  the `AdapterKind::Telegram` arm where we KNOW the rasterizer
  will be reached. Same pattern as Telegram's own api_base check.

- **`tiny-skia` 0.11's PNG feature is `png-format`, not `png`.**
  When pinning the version in `[workspace.dependencies]`, look at
  `cargo info tiny-skia` (or src/Cargo.toml on docs.rs) before
  picking feature names — the docs page on crates.io can lag a
  major release. Wrong feature name = silent fallback to no-PNG,
  which surfaces as a runtime "PNG encode failed" only after the
  first render attempt. Discovered while wiring the rasterizer
  in PR 36.

- **`r#"..."#` raw-string delimiters collide with `#` characters
  inside the string (e.g. hex color `#ff0000`).** Use `r##"..."##`
  when the string body contains any `"#` sequence — Rust's parser
  treats the first `"#` it sees as the close of the raw string.
  The error message ("expected `;`") is unhelpful; the fix is the
  delimiter count, not the content. PR 36's rasterizer test SVG
  bit me here. Lint aid for later: a `clippy::needless_raw_strings`
  lint exists but doesn't catch this — `cargo expand`-style
  inspection is the manual fallback.

- **Discord interaction responses can carry attachments, but only
  via `multipart/form-data`.** Plain JSON channel-message responses
  can't embed inline image bytes; the documented path is a
  multipart body with a `payload_json` part referencing
  `attachments: [{id: 0, filename: ...}]` + one `files[N]` part per
  attachment. The Content-Type on the HTTP response changes from
  `application/json` to `multipart/form-data; boundary=...`. PR 38
  hand-rolls the multipart body in `triton-chat-discord::lib`
  (RFC 2046 framing) because `reqwest::multipart::Form` is for
  outgoing client requests, not axum responses. A future
  refactor could share the framing with `multer` or similar; for
  now the inline build is small enough to live next to the call
  site.

- **`reqwest::Response::bytes().await` reads the entire body with
  no cap.** A misbehaving (or attacker-controlled) rasterizer can
  flood the adapter's memory before the chat-platform courier ever
  runs. PR 38 added a `MAX_RESPONSE_BYTES = 2 MiB` cap and switched
  to a chunk-by-chunk read so the stream aborts the moment the cap
  is exceeded. The cap surfaces as `RasterizerError::Server("...too
  large...")`, mapped by the adapter to the same `rasterizer_failed`
  audit path as a 5xx — operators get one consistent fail mode.

- **Per-component caps need per-field caps too.** PR 36's
  `DashboardRequest::validate` capped title (256 B) and tile count
  (32) but NOT each tile's `label`/`value`/`trend` strings. A tool
  emitting 32 tiles with 4 KB strings each would blow the SVG body
  to ~400 KB even though the headline caps look fine. PR 38 added
  `MAX_TILE_FIELD_BYTES = 128` per string with per-tile error
  messages naming the offending index + field. Lesson: when adding
  a structural cap, also cap every variable-length payload INSIDE
  the structure — otherwise the cap is a fence with one missing
  rail.

- **Two known gaps deferred from PR 38 codex review.** Both need
  bigger design discussions than the PR scope allowed:
  - **Blocking-render cancellation.** The rasterizer's
    `spawn_blocking` raster step doesn't cooperate with the HTTP
    timeout — `tokio::time::timeout` cancels the AWAIT, not the
    `spawn_blocking` task itself, which keeps running until the
    raster naturally returns. A pathological SVG that takes 10s to
    render still occupies a blocking worker for 10s. Real fix:
    move the renderer to a separate OS process with a hard kill
    timer. Out of scope for v0.2.
  - **Client admission control.** The chat adapter calls the
    rasterizer with no in-flight limit, so a burst of dashboards
    can saturate it. A `Semaphore::new(N)` around `Client::render`
    would gate concurrency, but `N` needs design — too low and a
    slow Telegram POST blocks unrelated Discord dashboards; too
    high and the rasterizer's own `spawn_blocking` pool saturates
    first. Will land alongside a fleet-level capacity story.

- **Vault token revocation mid-lease self-heals via invalidate +
  retry-once.** The `VaultToken` (`triton-secrets/src/vault_token.rs`)
  refreshes *proactively* at half the lease, so normal expiry never
  bites. For revocation *before* `refresh_at` (operator
  `vault token revoke`, policy change, Vault restart losing the
  lease), both consumers — the KV resolver (`triton-secrets`) and the
  per-call OIDC mint (`triton-upstream`) — treat a 401/403 as the one
  retryable case: they call `VaultToken::invalidate()` (clears the
  cache), then retry once, which forces a fresh login. A second
  401/403 is terminal. Gotcha for the next person: `invalidate()` is
  *not* single-flight, so a burst of simultaneous 401s can each clear
  a freshly-relogged-in token and trigger a few redundant logins —
  acceptable under a rare revocation event, but don't assume exactly
  one re-login. Covered by `vault_workload_identity.rs::
  dispatch_recovers_when_vault_token_is_revoked` (FakeVault 403s the
  first OIDC swap, then succeeds; asserts login_hits == 2).

- **A consumer agent built with `adk-rust` must REPLACE its A2A server,
  not add one.** `examples/adk-hello-agent` runs a real adk-rust
  `LlmAgent` but Triton is its only front door, so it depends on the
  adk-rust *library* crates (`agents`/`models`/`anthropic`/`runner`/
  `sessions`) and explicitly NOT `server` — adk-rust's own `POST /awp/a2a`
  is exactly the interface Triton supersedes. Gotchas met building it:
  (1) it's a **standalone Cargo workspace** (own `[workspace]`) so
  adk-rust's ~25-crate tree never enters Triton's build; (2) adk-rust
  0.9's `RunnerConfig` is `#[non_exhaustive]` — construct the runner via
  `Runner::builder()`; the struct-literal form the docs show won't
  compile; (3) put the LLM behind a tiny `Brain` trait with a
  deterministic `StaticBrain` default so the no-mock spawned-binary e2e
  stays hermetic — "no mocks" governs the wire/backends under test (real
  HTTP, real Consul/Vault fakes), not the model provider, which is
  neither Triton's concern nor on the wire.

- **The Explorer can't list an upstream agent's tool from `/v1/tools`.**
  That endpoint returns only the in-process `ToolRegistry::descriptors()`
  (`build_registry()` in `triton-bin`); upstream tools are an
  invoke-by-name *fallback* in the dispatcher and never appear there, and
  the manifest's `tools:` block doesn't add descriptors either. So the
  reengineered Console grew a **custom-tool-name** entry to target a tool
  that isn't listed (e.g. the `hello` agent). For the local browser demo
  the harness reaches the agent by spawning the existing
  `upstream_fixture::FakeConsul`/`FakeVault` as standalone bins
  (`crates/triton-tests/src/bin/fake-{consul,vault}`) and pointing
  Triton's upstream router at them — no Triton code change needed.

- **`Router::nest("/mcp", r)` where `r` has a root `/` route answers
  `/mcp` but 404s `/mcp/`.** axum 0.8 nest semantics: the inner `/`
  route matches the prefix exactly (`/mcp`), not the trailing-slash
  variant. This bit the single-port embed host (issue #75): the
  Explorer's MCP client unconditionally POSTed `"$baseUrl/"`, harmless
  when `baseUrl` is a bare origin (path normalises to `/`, MCP on its
  own port) but `/mcp/` -> 404 once `/v1/runtime` advertised
  `mcp_base=/mcp` and the base carried that mount path. Fix is
  client-side: the MCP endpoint *is* `baseUrl` -- only append `/` when
  the base has no path. **Invisible to curl smoke-tests** (you naturally
  curl `/mcp`, which works); it only surfaces when the real SPA drives
  the wire. Caught by a Chrome-DevTools-MCP click-through of the embedded
  Explorer where REST+A2A were 200 and MCP alone was 404 -- concrete
  payoff for CLAUDE.md section 8's "verify in a real browser". To make
  the host tolerant of trailing slashes from other MCP clients, mount the
  (Clone, Arc-backed) `McpState` router at both `/mcp` and `/mcp/`, or add
  a `NormalizePathLayer`; but the contract is that clients POST the
  advertised endpoint verbatim.

- **The agent-initiated outbound surface needs its OWN OIDC audience,
  not the HTTP trio's.** `POST /v1/outbound` (#95) must reject a bearer
  minted for the trio audience and vice-versa, so per-surface
  authorisation actually means something. The `OidcVerifier` validates a
  single `aud`, so build a SECOND verifier/`IdentityProvider` from
  `TRITON_OUTBOUND_AUDIENCE` and mount the outbound router with it —
  don't try to teach one verifier two audiences (it would accept either
  token on either surface). Fail closed: if OIDC is on but the outbound
  audience is unset, leave `/v1/outbound` unmounted.

- **WhatsApp's 24-hour service window is per-recipient runtime state, so
  it lives in memory only (G-8).** A free-form proactive send to a
  recipient who hasn't messaged in 24 h is rejected by Meta; the adapter
  decides free-form-vs-template from an in-memory `HashMap<wa_id,
  Instant>` stamped on every inbound. A cold start treats everyone as
  window-closed until they message in again — the safe default, and the
  reason proactive sends should prefer templates. Watch for: introducing
  the window rule (#94) tightened the #95 free-form happy-path test,
  which now has to open the window with an inbound first.

- **The Cloud-API adapter was hiding inside `kind: whatsapp_web`.** Until
  #94, the Baileys socket bridge and the Cloud-API webhook adapter both
  answered to `AdapterKind::WhatsappWeb`, disambiguated only by
  `inbound.kind`. Splitting `whatsapp_cloud` out is mostly mechanical, but
  note the two `match adapter.kind` loops in `triton-bin` (socket vs
  webhook) and the manifest `phone_number_id` rule all keyed off the old
  kind — grep for every `WhatsappWeb` before assuming the split is done.

- **A Google OIDC ID token proves *Google signed it*, not *who asked*.**
  #141 widened the google_chat verifier to accept `iss =
  accounts.google.com` (the modern console flavor). But that issuer is
  shared by *every* Google-minted ID token, and the `aud` is the webhook's
  **public** App URL — anyone with a Google service account can mint a
  token for it via IAM `generateIdToken`. Since the sender identity is then
  read from the *unsigned* request body, that's a clean impersonation
  bypass. The Chat-specific discriminator Google documents is the `email`
  claim = `chat@system.gserviceaccount.com`; the verifier must require it
  whenever `iss` is the generic OIDC issuer (the legacy `iss =
  chat@system…` flavor is self-proving and needs no extra check). Rule of
  thumb: when an inbound credential's issuer/audience aren't both unique to
  *this* caller, you need a per-actor claim too — signature + audience
  alone is not authentication.

- **A test-local `locate_triton_binary` skips the harness's freshness
  guard.** `TritonProcess::spawn` locates the binary via
  `triton_binary_path()` which runs `ensure_fresh_binary` (rebuilds
  `triton-bin` when any production source is newer). Tests that spawn the
  binary *directly* for boot-refusal assertions (google_chat's
  `locate_triton_binary`) get no such guard: under
  `cargo test -p triton-tests …` the T1b broken-SA-key boot test ran a
  pre-change `target/debug/triton` that happily booted — the test read as
  "fail-closed not implemented" when the implementation was fine. Also:
  never assert boot-refusal with a bare `.output()` (a regression that
  boots = a test that hangs forever); poll `try_wait` with a deadline and
  kill + panic on timeout.

- **A negotiated A2UI envelope has no `surface` field, so the channel
  preview can't just re-post the bubble.** `POST /v1/surface/render` runs
  the chat mappers, which call `extract_surface` (needs
  `{ "surface": … }`). But a turn the Explorer is *showing* holds the
  negotiated `{version, stream}` — `v09::build` reshapes onto `stream` and
  drops `surface`. Re-invoking the tool to recover it would run a whole new
  LLM turn (and could yield a *different* surface — wrong for a "preview
  THIS answer" affordance). Fix: `envelope_to_surface` (the v0.9 inverse of
  `build`) in `triton-core::a2ui`, and `surface_render` normalises its
  input — accept `{surface}` directly OR reverse a negotiated envelope. A
  round-trip unit test (`build` then reverse == identity, every variant)
  pins it against drift when a new `Component` variant lands.

- **Don't trust a memorized crypto test vector — fetch the source.** (#191,
  `triton-chat-twilio`) Writing the `X-Twilio-Signature` unit test from
  recollection alone produced a plausible-looking but wrong vector (wrong
  host in the URL, a longer-than-real `CallSid`, and consequently a
  fabricated expected signature) — it failed, and failed in a way that
  looked like an algorithm bug rather than a bad fixture. `WebFetch`ing
  Twilio's actual docs turned up the real values in one shot. Lesson:
  when a red test's *first* failure is against a "well-known documented
  vector" you typed from memory, verify the vector against a primary
  source before spending time debugging the implementation — the fixture
  is at least as likely to be wrong as the code.

- **`SendGrid`'s courier and Twilio's inbound HMAC use different auth
  shapes for the same account — don't assume one secret ⇒ one credential
  field.** (#191) SendGrid's outbound API takes a Bearer API key; Twilio's
  Messaging API (planned for the WhatsApp/RCS couriers) takes HTTP Basic
  `AccountSid:AuthToken` on outbound but the bare Auth Token as an HMAC-SHA1
  key on inbound — three different credential *shapes* riding on what an
  operator thinks of as "my Twilio secret". `SignatureScheme::TwilioSignature`
  only wires the inbound `secret` field in `triton-manifest`; the outbound
  courier (PR-T2) will need its own `account_sid` + `token` fields on
  `outbound.credentials`, resolved separately even though `token` and
  `secret` may be configured to the same underlying value operationally.

- **Twilio's WhatsApp channel cannot build interactive messages at
  send-time — WhatsApp Cloud's `#94` model doesn't port.** (#191, PR-T3)
  Planning assumed Twilio-WhatsApp buttons/lists would mirror WhatsApp
  Cloud's `build_interactive_body` (render `Component::Button`/
  `Selection` into an ad-hoc JSON payload per send, with a fresh signed
  correlation token as the button `id` each time). Checking Twilio's
  actual `Messages` resource docs first (before writing any code) showed
  the only levers for rich content are `ContentSid` (a Twilio-assigned id
  for an operator-pre-approved **Content Template**, authored via
  Console/Content API ahead of time) and `ContentVariables` (fills the
  template's `{{n}}` placeholders — text only, not button structure).
  There is no path to send a NEW button set Twilio hasn't already seen.
  So PR-T3 reuses the *existing* `category`/`variables` proactive-send
  mechanism (#94's `OutboundRequest` fields, unchanged) to resolve
  `ContentSid`, and dynamic `Button`/`Selection` rendering stays deferred
  (counted, not built) — not a missing feature, a different platform
  shape. Anyone extending this to real per-message interactivity needs a
  template *catalogue* design (map each distinct button-set shape the
  agent might emit to a pre-authored ContentSid), which is out of scope
  until a concrete need shows up.

- **Codex review of the Twilio work (#191) found 4 real issues; one flag
  turned out to match existing precedent.** Ran a security/correctness
  review pass over PR-T1/T2/T3 before continuing to PR-T4. Confirmed and
  fixed: (1) `outbound.token` was required by the generic `outbound.kind:
  rest_api` closed-set check but the adapter never actually read it —
  it silently reused `inbound.secret` for BOTH the inbound HMAC key and
  the outbound HTTP Basic password, so a manifest could set them to
  different values with no error and the wrong one would win silently.
  Fixed by resolving `outbound.token` into its own field and using it for
  Basic auth. (2) `outbound.from` was required at adapter-build time but
  not checked by `Manifest::validate()`, so a manifest missing it passed
  validation and only failed later at boot — added the same
  kind-specific check `account_sid` already had. (3) The `public_url`
  M-SECRETS-1 exemption matched on field NAME only, not adapter kind —
  since inbound credentials are a flattened open map, ANY adapter could
  smuggle a literal secret past the production check by naming a field
  `inbound.public_url`. Scoped the exemption to `AdapterKind::
  TwilioWhatsapp`. One flagged item did NOT need fixing: Codex noted
  in-webhook rate-limiting happens after signature verification, so an
  attacker could force unlimited cheap parse+HMAC work before being
  throttled — checking WhatsApp Cloud's `verify_hmac256` confirmed this
  is the established codebase pattern everywhere (verify first, THEN
  rate-limit), not a Twilio-specific regression; changing it would be a
  cross-cutting architecture change out of scope for this work. Lesson:
  an AI review's findings still need independent verification against
  the actual code and existing precedent before applying — some are
  real bugs, some are consistent-with-everything-else non-issues.

- **"Decode the inbound interactive reply" doesn't mean the same thing
  on every platform — check whether a correlation token is even
  possible before assuming the Telegram/Discord pattern transfers.**
  (#191, PR-T4) The original plan expected Twilio-WhatsApp's inbound
  button-tap handling to mirror Telegram's `callback_query` decode (a
  signed `(tool, args)` token as the button `id`). But PR-T3 already
  established Twilio buttons live inside operator pre-authored Content
  Templates — Triton never builds the button structure per-message, so
  it never emits a correlation token as the payload in the first place.
  Twilio's actual inbound shape (`ButtonPayload` + `ButtonText` on an
  otherwise-ordinary inbound message, confirmed via the Messaging
  webhook-parameters docs) meant the real fix was much smaller: prefer
  `ButtonPayload` over `Body` as the routing text, dispatch through the
  existing pipeline. Two-line diff instead of porting a signature-decode
  path. Worth the research pass before writing code.

- **`unreachable!()` guarded by a boolean computed in a SEPARATE match on
  the same value is not actually unreachable if the two matches diverge
  even slightly.** (#191, PR-T5) Refactoring the WhatsApp-only manifest
  checks to also cover `twilio_rcs` introduced `let is_twilio_adapter =
  matches!(adapter.kind, A | B); let label = match adapter.kind { A =>
  .., B => .., _ => unreachable!() };` — looks safe (the `_` arm "can't"
  fire because `is_twilio_adapter` already filtered), but `label` is
  computed unconditionally for EVERY adapter kind on EVERY validate()
  call, not just when `is_twilio_adapter` is true. Every non-Twilio
  adapter (Telegram, Discord, WhatsApp Cloud, everything) hit the
  `unreachable!()` and the whole binary panicked at boot. 171 of the 321
  workspace tests failed instantly. Caught only because the end-of-task
  ritual runs the FULL `cargo test --workspace`, not just the
  newly-added tests — a scoped `cargo test twilio_rcs` run right before
  this would have stayed green while everything else was on fire. Fix:
  make the label an `Option`, not a value computed via a match that
  assumes a guard from elsewhere. Lesson restated because it bears
  repeating: never skip the full-suite run, no matter how contained a
  change looks.

- **`Dispatcher::record_post`'s `status_detail` is `Option<&'static str>`
  — a closed diagnostic-label set, not a place for dynamic data.**
  (#191, PR-T6) First attempt at the delivery-receipt handler tried to
  put the Twilio `MessageSid`/`ErrorCode` (both runtime `String`s) into
  `status_detail` for correlation. Wouldn't compile: `PostResult<'a>`
  types `status_detail` as `Option<&'static str>` specifically so every
  call site passes a literal like `"rasterizer_call"`, not
  caller-computed text. The actually-idiomatic fix, once noticed: the
  audit record's `trace_id` field is `&'a str` (not `'static`) — so a
  context-free event with no real principal (this codebase is
  stateless, G-8; nothing survives from the original send to correlate
  by) can carry a meaningful dynamic id there instead of a fresh
  `uuid::Uuid::new_v4()`. Setting the synthetic `Principal.trace_id` to
  the Twilio `MessageSid` gives free structured correlation with zero
  new mechanism; the truly dynamic diagnostic detail (ErrorCode) rides
  on a plain `tracing::warn!` line, matching how every other courier's
  error path already logs before calling `record_post` with a static
  label.
