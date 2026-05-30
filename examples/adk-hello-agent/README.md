# `adk-hello-agent` — an adk-rust agent fronted by Triton

A minimal "hello world" upstream agent for the Triton agent-ingress
gateway, built with [adk-rust](https://github.com/zavora-ai/adk-rust). It
runs a real adk-rust `LlmAgent` (Anthropic Claude) as its brain, but
**replaces adk-rust's own A2A server with Triton's `POST /` upstream
contract** — so Triton becomes the agent's REST / MCP / A2A / chat front
door.

> **An upstream agent does not implement frontends.** REST, MCP, A2A and
> the six chat channels are *Triton's* surfaces. This agent speaks exactly
> one contract — `POST /` with a Vault-minted OIDC bearer, raw args JSON
> in, a canonical A2UI `surface` out. Register the tool once (Consul
> `tag:agent:hello` + the manifest fragment) and Triton fans it across
> every frontend for free. The integration test *demonstrates* that; the
> agent never codes a frontend.

## What "replace adk-rust's A2A interface" means here

adk-rust ships `adk-server`, which exposes its own REST/SSE and A2A
(`POST /awp/a2a`) endpoints. This crate deliberately depends on adk-rust's
*library* crates only (`agents`, `models`, `anthropic`, `runner`,
`sessions`) — **not** `server`. The brain (an `LlmAgent` run through
`Runner`) lives behind a tiny `Brain` trait in `src/agent.rs`; the wire
surface in `src/main.rs` is a ~150-line axum service speaking Triton's
contract. Triton owns every protocol; adk-rust owns only the thinking.

## Layout

| File | Purpose |
|---|---|
| `src/agent.rs` | `Brain` trait. `LlmBrain` (real adk-rust `LlmAgent` + Anthropic) and `StaticBrain` (deterministic, no key). |
| `src/main.rs` | Triton's `POST /` contract: verify the bearer, run the brain, return a canonical A2UI `surface` (narration + a "Greet again" button). |
| `adapter-manifest.fragment.yaml` | The `tools.hello` slice (+ a Telegram degrade example) for the operator's `adapter.yaml`. |
| `agent.nomad.hcl` | Nomad job: `tag:agent:hello`, OIDC verifier wiring, `ANTHROPIC_API_KEY` from Vault. |
| `tests/triton_e2e.rs` | No-mock e2e: real Triton + real agent + Consul/Vault fakes; asserts the greeting through REST, MCP and A2A. |
| `tests/live_llm.rs` | The genuine LLM path, `#[ignore]`d (needs a key + network). |

## Run it

```bash
# Deterministic brain (no key needed):
cargo run                       # listens on :8080, dev-token auth

# Live LLM brain:
ANTHROPIC_API_KEY=sk-… cargo run

# Ship it (dev-token compiled out, OIDC required):
cargo build --release --no-default-features
```

## Test it

The hermetic e2e needs the `triton` binary built in the parent repo:

```bash
cargo build -p triton-bin                                   # at the Triton root
cargo test --manifest-path examples/adk-hello-agent/Cargo.toml --test triton_e2e
# live path:
ANTHROPIC_API_KEY=sk-… cargo test \
  --manifest-path examples/adk-hello-agent/Cargo.toml --test live_llm -- --ignored
```

This crate is a **standalone Cargo workspace** (note the `[workspace]` in
`Cargo.toml`): adk-rust's large dependency tree is kept out of the Triton
workspace build. `triton-tests` is consumed as a `path` dev-dependency.

## Extracting to a standalone template repo

To lift this into its own GitHub template repo, copy the crate out and
swap the `triton-tests` path dependency for a git or published one:

```toml
[dev-dependencies]
triton-tests = { git = "https://github.com/DataZooDE/triton", rev = "<sha>" }
```

Everything else (the `Brain` seam, the `POST /` handler, the manifest
fragment, the Nomad job) is already self-contained.
