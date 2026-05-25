# triton-platform templates — fork this if…

Each template encodes the conventions in the references. Copy it into
your app, then replace the marked placeholders. Don't write any of
these from scratch.

| Fork this… | …if you are |
|---|---|
| `upstream-agent-axum/` | Building a **tool that Triton calls**. A working Rust crate: one axum route at `/`, OIDC-bearer verification (with a dev-token escape hatch gated out of release), and a tool that returns an A2UI v0.9 surface. Start here for any agent. See `references/01`, `references/02`, `references/04`. |
| `consumer-integration-test/` | Writing a **`frontend → triton → app-agent` test** in your own CI. A drop-in `tests/triton_e2e.rs` plus the `[dev-dependencies]` snippet. Boots a real Triton with `FakeConsul` + `FakeVault` + `FakeAgent`, no mocks. See `references/08`. |
| `adapter-manifest.yaml` | Handing the **Triton operator** the manifest entry that registers your tool and (optionally) its chat-channel `degrade` rules. You supply the content; the operator merges it into the deployment's `adapter.yaml`. See `references/03`, `references/05`. |
| `agent.nomad.hcl` | Deploying your agent as a **Nomad job**. The `tag:agent:<name>` Consul registration, Vault verifier policy, tailnet-only binding. Pairs with `substrate-platform/templates/llm-agent.nomad.hcl` for the rest of the job shape. See `references/03`, `references/11`. |

## Typical order for "build me an agent for Triton"

1. `upstream-agent-axum/` — write the tool.
2. `consumer-integration-test/` — prove `frontend → triton → agent`
   works against a real Triton binary.
3. `adapter-manifest.yaml` — give the operator your registration
   fragment.
4. `agent.nomad.hcl` — deploy (with `substrate-platform` for the
   Vault/Consul specifics).

## Placeholder convention

Placeholders are `<angle-bracketed>`: `<my-tool>`, `<co>`, `<app>`,
`<image>`. Search-and-replace before use. Comments marked `// EDIT:`
flag the lines you must change.
