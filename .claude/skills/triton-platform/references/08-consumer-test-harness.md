# 08 — The consumer test harness (`triton-tests`)

Triton's discipline is **no mocks**: integration tests spawn the real
binary and drive it over real HTTP against real local fakes that
speak the actual wire protocol (Triton `CLAUDE.md` §1). That same
harness is the **supported consumer-facing surface** — you depend on
it from your own workspace to write `frontend → triton → app-agent`
tests in your CI. Full walkthrough: `doc/consumer-integration-tests.md`;
contract: FR-T-1..5; stability: ADR-16.

## Depend on it

The crate lives at `crates/triton-tests`. Depend via path (monorepo /
sibling checkout) or git (pinned commit). It is **not published to
crates.io** — pin a commit SHA for a frozen point.

```toml
[dev-dependencies]
triton-tests = { path = "../triton/crates/triton-tests" }
# or: { git = "https://…/triton", rev = "<sha>" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde_json = "1"
```

Your test crate should be a **separate Cargo workspace** so it isn't
pulled into Triton's. The `examples/consumer-smoke/Cargo.toml` shows
the trick: declare an empty `[workspace]` table to root it locally.

Fork `templates/consumer-integration-test/` rather than assembling
this by hand.

## The `pub` surface (governed by ADR-16)

From `crates/triton-tests/src/lib.rs` and its fixture modules:

| Item | What it gives you |
|---|---|
| `TritonProcess` | Spawns the real `triton` binary on free loopback ports; waits for `/healthz`; `Drop` kills it. `spawn()`, `spawn_with_env(...)`, `spawn_with_args(...)`. Exposes `rest_addr`, `mcp_addr`, `a2a_addr`, `metrics_addr`, `chat_webhook_addr`, and `rest_url(path)` / `mcp_url` / `a2a_url`. |
| `TestIssuer` | A real OIDC issuer (Ed25519 keypair + JWKS endpoint). `issuer_url()`, `sign_jwt(claims: Value)`, `unsigned_jwt(claims)` for the real-JWT path. |
| `upstream_fixture::FakeAgent` | A real axum server speaking the upstream wire shape. `start_echoing()`, `start_always_failing()`, `start_failing_then_recovering(n)`, `start_returning(json)`; `.host_port()` (feed it into `TRITON_STATIC_UPSTREAMS`), `.hits()`, `.bearers_seen()`, `.tools_seen()` (the `X-Triton-Tool` header), `.bodies_seen()`. |
| chat fakes | `chat_courier_fixture` (Telegram-shaped capture), `signald_fixture`, `rasterizer_fixture`. |

ADR-16 guarantee: any breaking change to these goes through a
one-release `#[deprecated]` cycle. Your CI won't break on a routine
refactor of Triton's internal tests.

## Pattern A — dev-token, end-to-end through your agent (the common case)

A real upstream dispatch needs only a `FakeAgent` and a
`TRITON_STATIC_UPSTREAMS` entry pointing your tool name at it. With no
signer configured, Triton sends the static `dev-token` bearer to your
agent (no JWT mint). The Consul + Vault fakes are **gone** — there is
no `FakeConsul`/`FakeVault` any more. This compiles against the
checked-out `triton-tests`
(`crates/triton-tests/tests/static_upstream.rs`):

```rust
use std::collections::HashMap;
use std::time::Duration;
use serde_json::json;
use triton_tests::{TritonProcess, upstream_fixture::FakeAgent};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontend_calls_triton_calls_my_agent() {
    let agent = FakeAgent::start_echoing().await;            // stands in for your deployed agent

    let env = HashMap::from([
        ("TRITON_ENV".into(), "nonprod".into()),
        // Resolve the tool name to the agent's host:port — no Consul, no Vault.
        ("TRITON_STATIC_UPSTREAMS".into(), format!("my-tool={}", agent.host_port())),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/my-tool"))
        .bearer_auth("dev-token")
        .json(&json!({ "city": "Berlin" }))
        .send().await.unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    // FakeAgent echoes args; Triton wraps the upstream result in `result`.
    assert_eq!(body["result"]["echoed"]["city"], "Berlin");
    // No signer configured → Triton sends the static dev-token bearer.
    assert_eq!(agent.bearers_seen()[0], "dev-token");
    // The dispatch carries the informational tool-name header.
    assert_eq!(agent.tools_seen()[0].as_deref(), Some("my-tool"));
}
```

> To exercise the **real RS256 path** instead of the dev-token bearer,
> add `TRITON_JWT_SIGNING_KEY` + `TRITON_SELF_ISSUER` +
> `TRITON_JWT_JWKS` to the env (all three together) and assert the
> bearer your agent saw is a JWT it can verify against Triton's
> `/.well-known/jwks.json` (→ `references/04`).

## Pattern B — real OIDC

`TestIssuer` signs arbitrary claims; there is no `mint_token` helper —
build the claims yourself (`crates/triton-tests/tests/oidc.rs`):

```rust
use triton_tests::{TritonProcess, TestIssuer};
use serde_json::json;

let issuer = TestIssuer::start().await;
let env = std::collections::HashMap::from([
    ("TRITON_OIDC_ISSUER".into(),   issuer.issuer_url()),
    ("TRITON_OIDC_AUDIENCE".into(), "agents-nonprod".into()),
]);
let triton = TritonProcess::spawn_with_env(std::time::Duration::from_secs(5), env).await;

let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
let token = issuer.sign_jwt(json!({
    "iss": issuer.issuer_url(),
    "sub": "alice",
    "aud": "agents-nonprod",
    "exp": now + 60,
}));
// …bearer_auth(&token)…  dev-token is now rejected (FR-T-1).
```

## What you don't get (by design)

- **No rate-limit override** — production defaults apply (NFR-P-3).
- **No `/metrics` scrape helper** — curl it yourself if needed.
- **No audit-bucket capture** — audit lines are on the child's
  stdout; `TritonProcess::stdout_snapshot()` returns them to assert
  on `trace_id` linkage / phase.
- **No embedded mode** — Triton is the binary; spawn-and-talk is the
  contract. There is no in-process library entry point.

## Prereq: the binary must exist

`TritonProcess` runs the compiled `triton` binary (prefers debug, via
`CARGO_BIN_EXE_triton` or by walking up to `target/debug/triton`). In
your downstream CI, ensure a `cargo build` (or `--release`) of Triton
has produced the binary before your tests run, or set
`CARGO_BIN_EXE_triton` to its path.
