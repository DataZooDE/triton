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
  Operators with B2B compliance requirements can add a second
  `whatsapp-cloud` adapter under the same manifest schema; the
  spec does not deliver it (deferred §7). Source: messenger
  paper §5 (WhatsApp); §7 deferred list.

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
