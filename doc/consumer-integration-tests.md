# Triton — Consumer integration tests

Status: draft v0.2 (2026-05-25)
Companion to `requirements.md` (§5.8 FR-T), `architecture.md`
(§8.8), and `realizations.md` (§7).

This doc is for the developer who is *not* working on Triton
itself but on an app that uses it. You want an integration test
of the shape `frontend → triton → app-agent` running in your
app's own CI, with no Consul / Vault / OIDC issuer deployed.
Everything below is supported surface: see FR-T-1..5 for the
contract and ADR-16 for the stability guarantee.

## What you get

- A `pub` Rust test-harness in `crates/triton-tests` you can
  depend on via path or git.
- A `dev-token` mode on the binary that needs no OIDC issuer:
  the literal bearer `"dev-token"` maps to a fixed dev principal.
  Default for debug builds; compiled out of release builds, so
  the affordance cannot leak to production.
- Real fakes for every backing service Triton talks to: Consul,
  Vault (token swap and KV v2), OIDC issuer, upstream agent,
  chat-platform APIs. No `mock` doubles; the fakes speak the
  actual wire protocols.
- A canonical full-featured manifest fixture at
  `crates/triton-tests/fixtures/manifest-valid.yaml` plus
  twelve per-adapter variants.

## Minimal pattern (dev-token, no OIDC)

```rust
use std::collections::HashMap;
use std::time::Duration;
use triton_tests::{TritonProcess, upstream_fixture::{FakeConsul, FakeAgent}};

#[tokio::test]
async fn frontend_calls_triton_calls_my_agent() {
    // Your agent — the upstream Nomad job would do this in prod.
    let agent = FakeAgent::start("echo").await;

    // A fake Consul that resolves `my-tool` to your agent.
    let consul = FakeConsul::start(&[("my-tool", agent.host_port())]).await;

    // Spawn Triton with no Vault, no OIDC, empty manifest.
    let triton = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_CONSUL_URL".into(), consul.base_url()),
            ("TRITON_MANIFEST_PATH".into(), "tests/fixtures/empty.yaml".into()),
        ]),
    ).await.unwrap();

    // Drive it like a real frontend would.
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/tools/my-tool", triton.rest_url()))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({"msg": "hello"}))
        .send().await.unwrap();

    assert_eq!(resp.status(), 200);
    assert!(agent.requests().await.len() == 1);
}
```

Per FR-T-3, every fixture binds an ephemeral loopback port and
`Drop`-cleans, so `cargo test --jobs N` is safe.

## OIDC-mode pattern

If you want to exercise the real JWT-verification path (because
your app code mints tokens too, for example):

```rust
use triton_tests::{TritonProcess, TestIssuer};

let issuer = TestIssuer::start().await;
let triton = TritonProcess::spawn_with_env(
    Duration::from_secs(5),
    HashMap::from([
        ("TRITON_OIDC_ISSUER".into(), issuer.issuer_url()),
        // ...consul as above...
    ]),
).await.unwrap();

let token = issuer.mint_token("alice", &["tool:my-tool"]).await;
let resp = reqwest::Client::new()
    .post(format!("{}/v1/tools/my-tool", triton.rest_url()))
    .bearer_auth(&token)
    .json(&serde_json::json!({"msg": "hello"}))
    .send().await.unwrap();
```

When `TRITON_OIDC_ISSUER` is set, `dev-token` is rejected — same
behaviour as production. Switch back to the minimal pattern when
you don't need a real JWT.

## Chat-adapter pattern

For tests that drive an inbound webhook (Telegram, Discord, etc.)
through Triton and assert the outbound platform call:

1. Start a `FakeVault::start_kv_v2()` and write your adapter's
   credentials at the manifest's `vault://` paths.
2. Spawn the chat-platform fake from `chat_courier_fixture`
   (currently Telegram-shaped; Discord and others use the same
   capture surface).
3. Spawn Triton with a manifest that names your adapter; set the
   env-gated platform-API base var (e.g.
   `TRITON_TELEGRAM_API_BASE`) to the fake's URL. Per FR-T-5
   this is only honoured when `TRITON_ENV=local`.
4. POST a signed inbound to Triton's webhook port; assert on
   `FakeTelegramApi::captured_messages()`.

Reference implementation lives at
`crates/triton-tests/tests/discord.rs` and the parity tests
under `crates/triton-tests/tests/`.

## What you don't get

- **No rate-limit override.** Production defaults apply (NFR-P-3).
  If a test exceeds the per-adapter budget, the harness will
  back-pressure / drop the same way production does.
- **No metrics scrape from the harness.** `/metrics` is bound on
  the tailnet listener; the harness doesn't curl it. Add your
  own `reqwest::get` if you need to assert on a counter.
- **No audit-bucket capture.** Audit lines go to the spawned
  process's stdout. `TritonProcess::stdout_lines()` returns them
  as `Vec<serde_json::Value>` if your test needs to assert on
  the audit pair (`trace_id` linkage, phase discriminator, etc.).
- **No embedded-mode entry point.** Triton is the binary, not a
  library. The spawn-and-talk pattern is the contract.

## Stability

ADR-16 (architecture.md §9) governs the `pub` surface of
`triton-tests`. Breaking changes follow a one-release
deprecation cycle (`#[deprecated]` warning that compiles for one
release) before removal. Practically: if your CI builds today
against `triton-tests` at commit X, it will keep building
through at least one release tag past X without modification.

The crate is consumed via git or path dependency inside
DataZoo-internal workspaces. No crates.io publication is in
scope; if you need a frozen point-in-time pin, pin the commit
SHA in your `Cargo.toml`.

For breaking changes you actually want, file a follow-up: the
deprecation cycle is the contract, not a no-change pledge.
