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
| `upstream_fixture::FakeConsul` | `start(&[(service, host_port)])` → `.url()`. Resolves `tag:agent:<name>` to your stub. |
| `upstream_fixture::FakeVault` | `start_minting(token)` (OIDC swap, what the upstream router needs) or `start_kv_v2(expected_token, entries)` (manifest credential resolution). `.url()`. |
| `upstream_fixture::FakeAgent` | `start_echoing()`, `start_always_failing()`, `start_failing_then_recovering(n)`; `.host_port()`, `.hits()`, `.bearers_seen()`. |
| chat fakes | `chat_courier_fixture` (Telegram-shaped capture), `signald_fixture`, `rasterizer_fixture`. |

ADR-16 guarantee: any breaking change to these goes through a
one-release `#[deprecated]` cycle. Your CI won't break on a routine
refactor of Triton's internal tests.

## Pattern A — dev-token, end-to-end through your agent (the common case)

A real upstream dispatch needs **both** a `FakeConsul` (to resolve
your tool) and a `FakeVault` (the upstream router mints a per-call
OIDC token for your agent before dispatching). The binary refuses to
boot with `TRITON_CONSUL_URL` set but no Vault — they go together.
This compiles against the checked-out `triton-tests`
(`crates/triton-tests/tests/upstream.rs`):

```rust
use std::collections::HashMap;
use std::time::Duration;
use serde_json::json;
use triton_tests::{TritonProcess, upstream_fixture::{FakeConsul, FakeVault, FakeAgent}};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontend_calls_triton_calls_my_agent() {
    let agent  = FakeAgent::start_echoing().await;           // stands in for your Nomad job
    let consul = FakeConsul::start(&[("my-tool", agent.host_port())]).await;
    let vault  = FakeVault::start_minting("vault-minted-agent-token").await;

    let env = HashMap::from([
        ("TRITON_ENV".into(),            "nonprod".into()),
        ("TRITON_CONSUL_URL".into(),     consul.url()),
        ("TRITON_VAULT_URL".into(),      vault.url()),
        ("TRITON_VAULT_TOKEN".into(),    "triton-vault-token".into()),
        ("TRITON_VAULT_OIDC_ROLE".into(),"agent-oidc-swap".into()),
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
    assert_eq!(agent.bearers_seen()[0], "vault-minted-agent-token"); // NOT dev-token
}
```

> `doc/consumer-integration-tests.md` shows a slimmer snippet
> (`consul.base_url()`, only `TRITON_CONSUL_URL`, a
> `TRITON_MANIFEST_PATH`). Prefer what compiles against the
> checked-out crate: the methods are `consul.url()` /
> `vault.url()` / `agent.host_port()`, Consul implies Vault, and an
> empty manifest is the default (no `TRITON_MANIFEST_PATH` needed).
> The template in this skill is kept in sync with the live API.

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
