# triton-platform templates — fork this if…

Each template encodes the conventions in the references. Copy it into
your app, then replace the marked placeholders. Don't write any of
these from scratch.

| Fork this… | …if you are |
|---|---|
| `upstream-agent-axum/` | Building a **tool that Triton calls**. A working Rust crate: one axum route at `/`, RS256-JWT-against-Triton's-JWKS bearer verification (with a dev-token escape hatch gated out of release), and a tool that returns an A2UI v0.9 surface. Start here for any agent. See `references/01`, `references/02`, `references/04`. |
| `consumer-integration-test/` | Writing a **`frontend → triton → app-agent` test** in your own CI. A drop-in `tests/triton_e2e.rs` plus the `[dev-dependencies]` snippet. Boots a real Triton wired to your agent via `TRITON_STATIC_UPSTREAMS` (dev-token), with a real `FakeAgent` — no mocks, no Consul, no Vault. See `references/08`. |
| `adapter-manifest.yaml` | Handing the **Triton operator** the manifest entry that registers your tool and (optionally) its chat-channel `degrade` rules. You supply the content; the operator merges it into the deployment's `adapter.yaml`. Credentials are `env://VARNAME` refs (no Vault). See `references/03`, `references/05`. |

## Typical order for "build me an agent for Triton"

1. `upstream-agent-axum/` — write the tool.
2. `consumer-integration-test/` — prove `frontend → triton → agent`
   works against a real Triton binary.
3. `adapter-manifest.yaml` — give the operator your registration
   fragment, and ask them to add `<tool>=<your host:port>` to
   `TRITON_STATIC_UPSTREAMS`.
4. Deploy your agent via the substrate's Kamal config (see the
   `substrate-platform` skill) — there is no Nomad/Consul/Vault any
   more. The operator wires `TRITON_STATIC_UPSTREAMS` in Triton's own
   deploy config; in prod Triton mints RS256 JWTs your agent verifies
   against its JWKS.

## How Triton finds and authenticates your agent

- **Discovery is a static map.** Triton resolves a tool name to a fixed
  `host:port` from `TRITON_STATIC_UPSTREAMS=<tool>=<host:port>` and
  POSTs to `/`. No Consul, no `tag:agent:<name>` self-registration —
  your agent is just an HTTP server the operator names in that env var.
- **Auth is a per-call RS256 JWT.** Outside dev, Triton mints a
  short-TTL RS256 JWT and your agent verifies it against Triton's JWKS
  at `<TRITON_SELF_ISSUER>/.well-known/jwks.json`. In dev it is the
  static `dev-token`.

## Placeholder convention

Placeholders are `<angle-bracketed>`: `<my-tool>`, `<co>`, `<app>`,
`<image>`. Search-and-replace before use. Comments marked `// EDIT:`
flag the lines you must change.
