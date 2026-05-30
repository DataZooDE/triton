# Triton — working principles for AI collaboration

Read this first. The spec lives in `doc/` (`requirements.md`,
`architecture.md`, `realizations.md`). This file is the contract for
*how we build it*, not *what we build*.

## 1. Red/Green TDD — no shortcuts

- Every change starts with a **failing test**. Write the test first;
  watch it fail for the right reason; then make it pass.
- A task is **only "done" when an integration test (NO mocks) actually
  passes against a running process.** Unit tests are useful but they do
  not finish a task on their own.
- "No mocks" means: real HTTP clients over real TCP sockets to a real
  spawned binary; real local Consul/Vault test servers (`hashicorp/consul`
  / `hashicorp/vault dev` or tiny in-repo HTTP fakes that speak the actual
  wire protocol) — not Rust trait doubles substituted in-process. The
  spawned-binary integration tests live in `crates/triton-tests/`.
- If a test cannot be written as an integration test (rare — e.g. pure
  formatting helpers like `py_float`), it's a unit test inside the
  owning crate's `tests/` directory, and the task isn't "done" until at
  least one integration test exercises the same code path end-to-end.

## 2. Incremental PRs

- One conceptual change per PR. Each PR ends with green CI and a
  passing integration test that demonstrates the increment works.
- Branch from `main` (`feat/<slug>`, `fix/<slug>`); push with
  `git push -u origin <branch>`; open a draft PR with `gh pr create
  --draft`. Mark ready-for-review only when the PR's integration test
  is green locally.
- Never amend a commit that has already been pushed. Always create new
  commits on top.

## 3. 12-factor alignment

This is a substrate-deployed workload. Read
`.claude/skills/substrate-platform/SKILL.md` (and its `references/`)
before touching deployment or runtime surface.

| Factor | How Triton honours it |
|---|---|
| I — Codebase | One repo, many deploys (nonprod, prod via Nomad blue/green). |
| II — Dependencies | `Cargo.toml` explicit; static link everything except libc (NFR-PT-2). |
| III — Config | `TRITON_*` env vars + CLI flags; precedence `CLI > env > compile-time defaults`. Adapter wiring (v0.2) reads `adapter.yaml` resolved against Vault. |
| IV — Backing services | Consul, Vault, OIDC issuer, upstream agents — all reached over the tailnet by name; no hardcoded endpoints. |
| V — Build/release/run | `cargo build --release` → single static binary baked into the Packer golden image. Release = image SHA; run = `nomad job run`. |
| VI — Processes | Stateless across restarts (G-8). No on-disk state. |
| VII — Port binding | The binary binds its own ports; Fabio sits in front, not in the binary. |
| VIII — Concurrency | One tokio runtime; horizontal scale by Nomad `count`. |
| IX — Disposability | Fast startup, graceful SIGTERM drain (G-4, FR-L-2). |
| X — Dev/prod parity | Dev token gated behind `cfg(feature = "dev-token")`; production builds reject any non-OIDC bearer at compile time (ADR-10, FR-I-5). |
| XI — Logs | JSON lines to stdout, two kinds (`log` and `audit`); substrate ships them (G-S3). Never link a log shipper. |
| XII — Admin processes | None inside the binary. Operator tasks live in the substrate repo. |

## 4. SOLID, clean code

- **Adapters stay 100–200 LOC.** Anything larger is a smell that
  business logic is leaking out of the dispatcher (realizations.md §1).
- **Dispatcher is the single audit pivot** (ADR-6). Adapters wrap/unwrap;
  they do not emit audit lines and they do not call upstreams.
- **Single Responsibility per crate.** `triton-core` owns the dispatcher,
  principal, audit emitter, and error types. `triton-adapters-http`
  owns the HTTP trio. `triton-bin` owns process lifecycle and wiring.
  Later v0.2 work adds `triton-chat-surface` and one crate per chat
  adapter.
- **Open/Closed for protocol versions.** New A2UI versions = new builder
  file. Never `if version == "0.9"` in the dispatcher or tools (ADR-4).
- **Dependency inversion at substrate seams.** Consul, Vault, OIDC
  clients hide behind traits owned by `triton-core` so integration tests
  can boot real local servers; production wires real clients. The trait
  is for substitutability of the *backend*, not for unit-test mocking.
- **No premature abstraction.** Three concrete adapters today; if/when
  a fourth HTTP protocol appears, extract a trait then, not now.

## 5. Asking vs assuming

- If you hit a real ambiguity in the spec — a clause that contradicts
  another, an undefined behaviour, a deployment detail the substrate
  skill doesn't cover — **stop and ask**. Don't guess.
- Before asking, spend up to a minute on read-only investigation
  (grep, doc re-read, substrate-skill reference) so the question is
  specific. "FR-X-N says A but FR-Y-M implies B in case Z; which wins?"
  beats "what should I do?"
- When you discover a non-obvious gotcha while solving a problem,
  write a **future-note** for yourself: append a short bullet to
  `doc/realizations.md` §7 (create the section if missing) capturing
  *what bit you, why, and how to avoid it next time*. This is how the
  experiments' `realizations.md` was built; we keep that habit going.

## 6. Hard prohibitions

- No `:latest` images, no static credentials in code/HCL/env, no
  Loki/Vector/OTel exporter dependency, no on-disk state.
- No mocks in integration tests.
- No code committed to `main` directly. Always via PR.
- No skipping the failing-test step — write the red test first.

## 7. End-of-task ritual

When you believe a task is done:

1. `cargo fmt --check && cargo clippy --all-targets -- -D warnings`
2. `cargo test --workspace` — all green.
3. The increment's **integration test** runs as part of `cargo test`
   AND has been verified by running the binary by hand at least once.
4. Commit (Co-Authored-By trailer), push, open PR, paste the
   integration-test output in the PR body.
5. Mark the TaskCreate task `completed`. Move to the next task.

## 8. Browser verification of the Explorer (rodney)

The `apps/explorer` SPA (Flutter Web, **CanvasKit** renderer) is
checked in a real browser with **rodney**
(https://github.com/simonw/rodney — a Go CLI driving persistent
headless Chrome). The harness lives at
`deploy/local-e2e/explorer-rodney.sh`: it builds + serves the SPA,
boots a local Triton (dev-token), walks every page taking screenshots
(`deploy/local-e2e/.rodney-out/`, gitignored), and exercises the A2UI
round-trip. Run it after explorer changes:

```bash
deploy/local-e2e/explorer-rodney.sh          # headless, builds web
deploy/local-e2e/explorer-rodney.sh --show --no-build   # visible, reuse build
```

Gotchas (CanvasKit paints into a `<canvas>`):

- rodney's CSS `click`/`text`/`exists` **cannot see Flutter widgets**.
  Drive and assert through the DOM **semantics tree** instead.
- Enable it first by clicking the injected
  `flt-semantics-placeholder` ("Enable accessibility").
- Tappable widgets are `flt-semantics[role=button]` / `[flt-tappable]`
  — click them by `textContent` (nav labels read "Playground Tab 2 of
  8", etc.). Dropdown-menu items render in an overlay that is *not*
  semantics-tappable; avoid driving those.
- Assert **rendered content** via `rodney ax-tree | grep`, NOT a
  `flt-semantics` querySelector: Flutter only keeps the semantics DOM
  fresh for newly-rendered subtrees while an a11y client is actively
  reading, and `ax-tree` (a CDP fetch) is that client. Poll — content
  appears a beat after paint and after `/v1/*` calls resolve.
- Seed auth before the app boots past the login gate: set
  `localStorage['flutter.triton.bearer']` and `['flutter.triton.baseUrl']`
  (shared_preferences web = JSON-encoded values under `flutter.`-keys),
  then reload.
