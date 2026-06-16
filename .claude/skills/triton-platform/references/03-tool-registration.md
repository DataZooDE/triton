# 03 — Registering a tool so Triton can reach it

Two things make a tool reachable. Both are on the **Triton deployment
side** (the operator owns them): the `TRITON_STATIC_UPSTREAMS` map
entry that points a tool name at your agent's `host:port`, and the
`adapter.yaml` manifest entry. Know the boundary so you ask for the
right thing — you supply the *values*, the operator wires them.

## 1. The static-upstream map entry — the operator owns this

Triton resolves a tool name to a fixed `host:port` from the
`TRITON_STATIC_UPSTREAMS` env var (FR-U-1;
`crates/triton-upstream/src/static_upstream.rs`):

```
TRITON_STATIC_UPSTREAMS=my-tool=my-agent.internal:8080,other-tool=other:8080
```

Key facts:

- **Routing is by tool name.** Each `name=host:port` pair maps one
  tool to one agent endpoint. Tool names must be **globally unique**
  across all agents — two agents can't both claim `my-tool`.
- **No service catalog.** There is no Consul, no `tag:agent:<name>`
  registration, no health-check-driven discovery. Adding or removing
  a tool is an edit to this env var on the Triton deploy
  (substrate-side Kamal config), then a redeploy. (This replaced the
  Consul `tag:agent:<name>` model in the Kamal migration, ADR-0013.)
- **A static-upstream name shadows an in-process tool of the same
  name** — if Triton also has a built-in `echo` tool and the map
  names `echo`, dispatch goes to your agent (the in-process
  registration is skipped, logged at boot).
- Your agent is reachable **only over the tailnet**, never public —
  only Triton is exposed. The substrate's registry exposure setting,
  not your agent, governs that.

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

### Credentials are `env://` refs, never literals

Every credential field in the manifest (bot tokens, webhook secrets,
correlation keys, identity tables) MUST be an `env://<VARNAME>`
reference outside `local` env. The substrate injects the value as
container env from GCP Secret Manager via kamal `.kamal/secrets`;
Triton resolves the ref from its own environment at boot
(`crates/triton-secrets/src/lib.rs`). Literal values are admitted in
`local` dev only; outside `local` they are refused (FR-L-6, NFR-S-5,
M-SECRETS-1). `vault://` refs still parse but **fail boot closed** —
Vault was decommissioned in the Kamal migration. So if you hand the
operator a manifest fragment with a literal or a `vault://` secret in
it, their prod boot will reject it; use `env://` refs in anything you
share.

## Boot-time validation you can rely on

Triton refuses to start on any unknown `kind` / `signature` /
`identity` / `degrade` key, any missing `degrade` rule, or any
literal credential in production (FR-L-4..6). Practical consequence:
a malformed manifest fails fast and loud at alloc start, never
silently at request time. If your tool "isn't reachable," check the
alloc's startup logs first — the validator names the offending key.

## What you do NOT do

- Don't hardcode your agent's address inside your own code — the
  operator names it once in `TRITON_STATIC_UPSTREAMS` on the Triton
  deploy.
- Don't edit Triton's manifest schema; if your tool needs a surface
  concept the schema can't express, that's a PR to the Triton repo
  (→ `references/10`).
