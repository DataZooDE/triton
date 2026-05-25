# 03 — Registering a tool so Triton can reach it

Two things make a tool reachable. One you own (Consul registration in
your Nomad job); one the Triton operator owns (the `adapter.yaml`
manifest entry). Know the boundary so you ask for the right thing.

## 1. Consul registration — you own this

Your Nomad job registers a Consul service tagged
`agent:<tool_name>`. Triton's upstream router resolves exactly that
tag (FR-U-1; `crates/triton-upstream/src/consul.rs`). There is **no
static endpoint config** anywhere — adding or removing a tool is a
Nomad job push, not a Triton change (ADR-8).

```hcl
service {
  name = "my-tool"
  tags = ["agent:my-tool"]   # ← this is the discovery key
  port = "http"
  # NO urlprefix- tag: agents are invisible to Fabio (G-7/G-S6).
  check { type = "http"  path = "/healthz"  interval = "10s"  timeout = "2s" }
}
```

Full job: `templates/agent.nomad.hcl`. The `urlprefix-` prohibition
matters — only Triton is public; your agent is tailnet-only.

## 2. Manifest entry — the operator owns this

The v0.2 `adapter.yaml` manifest is Triton's single source of truth
for adapter wiring, tool registration, identity strategies, surface
mapping, and rate limits (ADR-13). It lives with the **Triton
deployment** (substrate side), not in your repo. You supply the
*content* of your tool's entry; the operator merges it.

A tool entry declares which surface components it can emit:

```yaml
tools:
  my-tool:
    surface_components: [text, narration, buttons]
  echo:
    surface_components: []        # raw-JSON tool, no UI
```

Source/example: `crates/triton-tests/fixtures/manifest-valid.yaml`.
Fork `templates/adapter-manifest.yaml` for the full fragment.

### Why `surface_components` matters: boot-time coverage

At cold start Triton closed-checks that **for every component type
your tool declares, every chat-channel adapter has a `degrade` rule**
(FR-L-5, M-COVERAGE-1). If your tool says it emits `buttons` but the
Telegram adapter has no `degrade.buttons` rule, **the gateway refuses
to start**. So:

- Declare `surface_components` honestly — list exactly what your tool
  can emit, no more.
- If you add a `dashboard` component later, the operator must add a
  `degrade.dashboard` rule (and a rasteriser) to every text-first
  adapter, or boot fails. Coordinate the change (→ `references/05`).

### Credentials are Vault refs, never literals

Every credential field in the manifest (bot tokens, webhook secrets,
correlation keys, identity tables) MUST be a
`vault://<path>#<field>` reference in production. Literal values are
admitted in dev mode only, with a runtime warning; production builds
refuse to start on any literal (FR-L-6, NFR-S-5, M-SECRETS-1). This
is the operator's concern, but if you hand them a manifest fragment
with a literal secret in it, their prod boot will reject it — use
Vault refs in anything you share.

## Boot-time validation you can rely on

Triton refuses to start on any unknown `kind` / `signature` /
`identity` / `degrade` key, any missing `degrade` rule, or any
literal credential in production (FR-L-4..6). Practical consequence:
a malformed manifest fails fast and loud at alloc start, never
silently at request time. If your tool "isn't reachable," check the
alloc's startup logs first — the validator names the offending key.

## What you do NOT do

- Don't hardcode your agent's address anywhere — Consul resolves it.
- Don't edit Triton's manifest schema; if your tool needs a surface
  concept the schema can't express, that's a PR to the Triton repo
  (→ `references/10`).
