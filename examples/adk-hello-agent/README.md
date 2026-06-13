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

The normative wire contract this example implements — dispatch shape,
sender-identity carriage in the bearer, response envelope, and the
`upstream` identity-resolver contract — is pinned in
[`doc/upstream-agent-contract.md`](../../doc/upstream-agent-contract.md).

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
| `src/main.rs` | Triton's `POST /` contract: verify the bearer, branch on `X-Triton-Tool`, run the brain (`hello`) or resolve a sender (`resolve_identity`), return a canonical A2UI `surface` or a `{sub, scopes, tenant}` reply. |
| `adapter-manifest.fragment.yaml` | The `tools.hello` slice (+ a Telegram degrade example, + a commented `identity.kind: upstream` block) for the operator's `adapter.yaml`. |
| `agent.nomad.hcl` | Nomad job: `tag:agent:hello`, OIDC verifier wiring, `ANTHROPIC_API_KEY` from Vault. |
| `tests/triton_e2e.rs` | No-mock e2e: real Triton + real agent + Consul/Vault fakes; asserts the greeting through REST, MCP and A2A. |
| `tests/resolver_e2e.rs` | No-mock e2e for `identity.kind: upstream` (FR-I-7): a WhatsApp inbound from an unknown sender resolves via `resolve_identity`, dispatches `hello` as the resolved principal, and couriers the reply; a refused sender rejects `401`. |
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

## Resolving chat senders (`identity.kind: upstream`, FR-I-7)

A chat adapter usually maps a platform sender id to a subject via an
operator-curated `sender_table`. When you'd rather let the agent decide
— e.g. flow unknown senders into conversational onboarding without an
operator step — declare `identity.kind: upstream` and point it at a
**resolver tool** this agent serves:

```yaml
adapters:
  whatsapp:
    kind: whatsapp_cloud
    tool: hello                 # command tool a plain message dispatches
    identity:
      kind: upstream
      resolver_tool: resolve_identity
```

On an inbound from a sender no table knows, Triton dispatches
`resolve_identity` to this agent (a **separate** call *before* the
command dispatch, audited under its own `messenger:whatsapp:identity`
protocol label) with:

```json
{ "platform": "whatsapp", "sender": "<wa_id>" }
```

The tool MUST return the resolved principal — and that is all that
distinguishes "resolved" from "rejected":

```json
{ "sub": "wa:<wa_id>", "scopes": ["chat"], "tenant": "demo" }
```

To refuse a sender, reply non-2xx (or with an empty `sub`/`tenant`):
Triton rejects the inbound `401` and never dispatches the command tool —
no guessed principal ever reaches your brain. `resolve_identity` in
`src/main.rs` shows both branches (it refuses any sender prefixed
`blocked`). The command dispatch then runs as the resolved `sub` (it is
the `who` in Triton's audit). See `doc/upstream-agent-contract.md` §5 for
the normative contract and `tests/resolver_e2e.rs` for the worked
round-trip.

> **Carriage:** by default only the resolved `sub` reaches the agent on
> the *command* dispatch (in the bearer; §3). To also receive the
> resolved `scope` + `tenant`, the operator sets
> `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true` (#110); otherwise key
> your own lookup off `sub`.

## Extracting to a standalone template repo

To lift this into its own GitHub template repo, copy the crate out and
swap the `triton-tests` path dependency for a git or published one:

```toml
[dev-dependencies]
triton-tests = { git = "https://github.com/DataZooDE/triton", rev = "<sha>" }
```

Everything else (the `Brain` seam, the `POST /` handler, the manifest
fragment, the Nomad job) is already self-contained.
