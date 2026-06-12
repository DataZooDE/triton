# Triton — Consumer integration tests

Status: draft v0.2 (2026-05-25)
Companion to `requirements.md` (§5.8 FR-T), `architecture.md`
(§8.8), and `realizations.md` (§7).

This doc is for the developer who is *not* working on Triton
itself but on an app that uses it. You want an integration test
of the shape `frontend → triton → app-agent` running in your
app's own CI, against in-process fakes rather than a deployed
Consul / Vault / OIDC issuer (the harness ships those fakes; you
don't stand up any real backing service). Everything below is
supported surface: see FR-T-1..5 for the contract and ADR-16 for
the stability guarantee.

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
  `crates/triton-tests/fixtures/manifest-valid.yaml` plus a set of
  per-adapter variants alongside it in `fixtures/`.

A working out-of-workspace example lives at
`examples/consumer-smoke/` (ACC-13) — copy its shape.

## Setting up the test crate

Two prerequisites the snippets below assume:

1. **Your crate is its own Cargo workspace.** With Triton as a
   sibling/path dependency, declare an empty `[workspace]` table in
   your `Cargo.toml` so Cargo doesn't pull your crate into Triton's
   workspace (see `examples/consumer-smoke/Cargo.toml`).
2. **The `triton` binary is built first.** `TritonProcess` spawns the
   compiled binary — it reads `CARGO_BIN_EXE_triton` if set, else
   walks up to `target/debug/triton` (preferred) or
   `target/release/triton`. In downstream CI, run `cargo build`
   inside the Triton checkout (or export `CARGO_BIN_EXE_triton`)
   before your tests.

```toml
# your-app/Cargo.toml
[workspace]                       # root this crate locally

[dev-dependencies]
triton-tests = { path = "../triton/crates/triton-tests" }
# or: { git = "https://.../triton", rev = "<commit-sha>" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde_json = "1"
```

## Minimal pattern (dev-token, no OIDC)

A real `frontend → triton → app-agent` dispatch needs **both** a
`FakeConsul` (to resolve your tool) and a `FakeVault` (the upstream
router mints a per-call OIDC token for your agent before
dispatching). The binary refuses to boot with `TRITON_CONSUL_URL`
set but no Vault — they go together. This compiles against the
checked-out harness (mirrors `crates/triton-tests/tests/upstream.rs`):

```rust
use std::collections::HashMap;
use std::time::Duration;
use serde_json::json;
use triton_tests::{
    TritonProcess,
    upstream_fixture::{FakeConsul, FakeVault, FakeAgent},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontend_calls_triton_calls_my_agent() {
    // Your agent — the upstream Nomad job would do this in prod.
    let agent = FakeAgent::start_echoing().await;

    // A fake Consul that resolves `my-tool` to your agent.
    let consul = FakeConsul::start(&[("my-tool", agent.host_port())]).await;

    // The upstream router mints a per-call agent token via Vault.
    let vault = FakeVault::start_minting("vault-minted-agent-token").await;

    // Spawn Triton: no OIDC issuer (dev-token path), empty manifest.
    let triton = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".into(),             "nonprod".into()),
            ("TRITON_CONSUL_URL".into(),      consul.url()),
            ("TRITON_VAULT_URL".into(),       vault.url()),
            ("TRITON_VAULT_TOKEN".into(),     "triton-vault-token".into()),
            ("TRITON_VAULT_OIDC_ROLE".into(), "agent-oidc-swap".into()),
        ]),
    ).await;

    // Drive it like a real frontend would.
    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/my-tool"))
        .bearer_auth("dev-token")
        .json(&json!({ "msg": "hello" }))
        .send().await.unwrap();

    assert_eq!(resp.status(), 200);
    // FakeAgent echoes the body; Triton wraps the result in `result`.
    assert_eq!(agent.hits(), 1);
    // Lethal-trifecta cut: your agent saw the Vault-minted token,
    // never the frontend's dev-token (FR-U-2, NFR-S-3).
    assert_eq!(agent.bearers_seen()[0], "vault-minted-agent-token");
}
```

Per FR-T-3, every fixture binds an ephemeral loopback port and
`Drop`-cleans, so `cargo test --jobs N` is safe.

## OIDC-mode pattern

If you want to exercise the real JWT-verification path (because
your app code mints tokens too, for example):

`TestIssuer` is a real local issuer (Ed25519 keypair + JWKS
endpoint). It signs whatever claims you hand it via
`sign_jwt(claims)` — there is no `mint_token` helper, so build the
claim set yourself. Note `TRITON_OIDC_ISSUER` and
`TRITON_OIDC_AUDIENCE` must be set **together** (the binary refuses
to boot with one but not the other). Mirrors
`crates/triton-tests/tests/oidc.rs`:

```rust
use std::time::{SystemTime, UNIX_EPOCH};
use serde_json::json;
use triton_tests::{TritonProcess, TestIssuer};

let issuer = TestIssuer::start().await;
let triton = TritonProcess::spawn_with_env(
    Duration::from_secs(5),
    HashMap::from([
        ("TRITON_OIDC_ISSUER".into(),   issuer.issuer_url()),
        ("TRITON_OIDC_AUDIENCE".into(), "agents-nonprod".into()),
        // ...consul + vault as above if you also dispatch upstream...
    ]),
).await;

let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
let token = issuer.sign_jwt(json!({
    "iss": issuer.issuer_url(),
    "sub": "alice",
    "aud": "agents-nonprod",
    "exp": now + 60,
}));
let resp = reqwest::Client::new()
    .post(triton.rest_url("/v1/tools/my-tool"))
    .bearer_auth(&token)
    .json(&json!({ "msg": "hello" }))
    .send().await.unwrap();
```

When `TRITON_OIDC_ISSUER` is set, `dev-token` is rejected — same
behaviour as production. Switch back to the minimal pattern when
you don't need a real JWT.

## Chat-adapter pattern

For tests that drive an inbound webhook (Telegram, Discord, etc.)
through Triton and assert the outbound platform call:

1. Start a `FakeVault::start_kv_v2(token, entries)` and write your
   adapter's credentials at the manifest's `vault://` paths
   (`webhook_secret`, `bot_token`, `senders`, `correlation_key`).
2. Spawn the chat-platform fake from `chat_courier_fixture`:
   `FakeTelegramApi::start()` (or `with_profile(...)` to simulate
   `ok:false` rate-limit / blocked responses). Discord, MS Teams,
   Google Chat have their own fakes in the same module
   (`FakeBotFramework`, `FakeGoogleJwks`, …).
3. Spawn Triton with `TRITON_MANIFEST_PATH` pointing at a manifest
   that names your adapter, `TRITON_VAULT_URL` / `TRITON_VAULT_TOKEN`
   for credential resolution, and the env-gated platform-API base
   var (`TRITON_TELEGRAM_API_BASE`) set to the fake's `url()`. Per
   FR-T-5 the base override is only honoured when `TRITON_ENV=local`.
4. POST a signed inbound to the webhook listener — its address is
   `proc.chat_webhook_addr` (the path is `/telegram/webhook`); the
   Telegram signature is the `X-Telegram-Bot-Api-Secret-Token`
   header. Assert on `FakeTelegramApi::captured()` (returns
   `Vec<SentMessage>`; photos via `captured_photos()`).

Worked reference: `crates/triton-tests/tests/telegram_courier.rs`.
Other adapters and the cross-channel parity tests live alongside it
under `crates/triton-tests/tests/` (e.g. `discord.rs`).

## Resolver-tool pattern (`identity.kind: upstream`, FR-I-7)

If your adapter resolves senders via one of your tools (instead of an
operator `sender_table`), test the resolver round-trip end to end:

1. In the test manifest, declare the adapter with
   `identity.kind: upstream`, `resolver_tool: <your-resolver>`, and
   `tool: <your-command-tool>`.
2. Register **both** tools to a real upstream endpoint — either a
   `FakeAgent` per tool or, for a real worked example, one agent serving
   both and branching on `X-Triton-Tool`. With `TRITON_STATIC_UPSTREAMS`
   that is `"<command>=<host:port>,<resolver>=<host:port>"`; with Consul,
   one `tag:agent:<tool>` per tool.
3. Use `FakeAgent::start_returning(json!({ "sub": …, "scopes": […],
   "tenant": … }))` for the resolver to pin the resolved principal, and
   `FakeAgent::start_always_failing()` to exercise the rejection path.
4. POST a signed inbound from a sender no table knows. Assert: the
   resolver received `{platform, sender}` (`bodies_seen()`); a dispatch
   audit under `…:identity` with `result: ok`; the command dispatch's
   `who`/`tenant` equal the resolver's reply; and the reply was
   couriered. For the failure agent, assert the inbound is refused `401`
   with **no** command dispatch and nothing couriered.

Worked references: `crates/triton-tests/tests/whatsapp_upstream_identity.rs`
(FakeAgents) and `examples/adk-hello-agent/tests/resolver_e2e.rs` (a real
agent serving `resolve_identity` + `hello`). Contract:
`doc/upstream-agent-contract.md` §5.

## What you don't get

- **No rate-limit override.** Production defaults apply (NFR-P-3).
  If a test exceeds the per-adapter budget, the harness will
  back-pressure / drop the same way production does.
- **No metrics scrape from the harness.** `/metrics` is bound on
  the tailnet listener; the harness doesn't curl it. Add your
  own `reqwest::get` if you need to assert on a counter.
- **No audit-bucket capture.** Audit lines go to the spawned
  process's stdout. `TritonProcess::stdout_snapshot()` returns them
  as `Vec<String>` (raw lines); parse each with
  `serde_json::from_str` if your test needs to assert on the audit
  pair (`trace_id` linkage, phase discriminator, etc.).
  `stderr_snapshot()` is the matching accessor for stderr.
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
