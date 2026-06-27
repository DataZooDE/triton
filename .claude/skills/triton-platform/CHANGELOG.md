# Changelog — triton-platform skill

The skill version tracks the consumer-facing contract, not the Triton
binary version. The Triton repo's checked-out git ref is the true
version pin (see `SKILL.md` → "How this skill is installed").

## 0.4.0 — MCP-Apps proxying + PNG-rasterisation delegation (#143)

Adds the consumer contract for an interactive **upstream renderer**
(e.g. peacock) behind Triton's MCP ingress. New material in `01`:

- **`_meta.ui.*` pass-through.** Return your `ui://` resource link on the
  tool result's `_meta.ui.resourceUri`; Triton lifts it onto the
  `tools/call` response `_meta` for the host.
- **`resources/read` proxy.** Triton forwards `resources/read` of a
  `ui://<authority>/…` URI to you as `POST /` with
  `X-Triton-MCP: resources/read`; reply with `{ contents: [...] }`.
- **`callServerTool` / `updateModelContext`.** Re-renders arrive as
  ordinary stateless `tools/call`s; `updateModelContext` records are
  relayed to you verbatim under `X-Triton-MCP: updateModelContext`.
- **Registration gotcha.** The `ui://` authority resolves through the
  same `TRITON_STATIC_UPSTREAMS` map — register the authority as its own
  key alongside your tool keys.
- **`render_a2ui_to_png` delegation.** Expose the tool returning
  `{ png_base64 }`; operators opt in with
  `TRITON_RASTERIZE_UPSTREAM=render_a2ui_to_png` to route chat dashboard
  rasterisation to you instead of the in-tree sidecar.

The pre-existing wire shape, A2UI envelopes, OIDC verification, and test
harness are unchanged; these surfaces are additive and opt-in.

## 0.3.0 — Consul/Vault → StaticUpstream + RS256-JWT + env:// (Kamal migration)

Catches the consumer contract up to the move off the HashiCorp stack
onto Kamal (ADR-0013). The wire shape, A2UI envelopes, surface/degrade,
OIDC *verification*, audit/logging, and the test-harness `pub` surface
are unchanged; the **discovery, agent-auth, secrets, and deploy
mechanics** were rewritten throughout.

- **Discovery is a static map, not Consul (`SKILL.md`, `00`, `01`,
  `03`).** Triton resolves a tool name to a fixed `host:port` from
  `TRITON_STATIC_UPSTREAMS=name=host:port,…`. There is no service
  catalog and no `tag:agent:<name>` registration; tool names must be
  globally unique. Adding/removing a tool is an edit to that env var on
  the Triton deploy.
- **Agent auth is a Triton-signed RS256 JWT, not a Vault token swap
  (`00`, `01`, `04`, `07`).** Triton mints a short-TTL (≤ 5 min) RS256
  OIDC JWT per dispatch (`TRITON_JWT_SIGNING_KEY` + `TRITON_SELF_ISSUER`
  + `TRITON_JWT_JWKS`, all-or-nothing; `TRITON_JWT_KID`) and serves it
  for verification at `/.well-known/jwks.json` +
  `/.well-known/openid-configuration`. Dev fallback: a static
  `TRITON_STATIC_UPSTREAM_TOKEN` bearer (default `dev-token`) when no
  signer is configured. The dispatch wire shape (`POST /`,
  `X-Triton-Tool`, args body) is unchanged.
- **Secrets are `env://VARNAME` refs, not `vault://` (`03`, `10`,
  `11`).** The substrate injects values as container env from GCP
  Secret Manager via kamal `.kamal/secrets`. `vault://` still parses
  but fails boot closed; literals are `local`-env-only.
- **Substrate is Kamal, not Nomad (`SKILL.md`, `11`).** Images
  `ghcr.io/datazoode/dz-triton*` pinned by SHA; the deploy config
  (`kamal/<app>/deploy.yml` + `apps/registry.yml`) lives in the
  substrate repo. The `agent.nomad.hcl` template was deleted; no Fabio,
  no Consul DNS.
- **Test harness (`08`).** The Pattern-A example now uses
  `TRITON_STATIC_UPSTREAMS` + `FakeAgent` only. `FakeConsul` and
  `FakeVault` are gone; `FakeAgent` gained `start_returning`,
  `tools_seen`, `bodies_seen`.

## 0.2.0 — wire-contract corrections + new auth modes

Catches the consumer contract up to Triton commits #56–#71.

- **A2UI transport nesting (`references/02`, `06`).** Corrected the
  most load-bearing fact: the `{version, stream}` A2UI envelope is
  **nested under a `result` key**, never top-level. Added the
  per-protocol read paths — REST `body.result`, MCP
  `result.structuredContent.result`, A2A `parts[0].data.result` — and
  the trace-id locations. A client that read top-level `version`/
  `stream` mis-rendered (the bug that shipped once in the Flutter
  explorer). You can dispatch on `result.version`.
- **Forwarded-auth sidecar mode (`references/06`, `07`).** Documented
  the third inbound auth path: `TRITON_TRUST_FORWARDED_AUTH=true` +
  `X-Forwarded-Email` from a co-located oauth2-proxy sidecar
  (ADR-0011 / #67), its synthesized `sso-ops` principal, and the strict
  OIDC > forwarded > dev-token precedence.
- **CORS for cross-origin SPAs (`references/06`).**
  `TRITON_CORS_ALLOWED_ORIGINS`, `Access-Control-Allow-Credentials`,
  `withCredentials`/`credentials:"include"`, wildcard refusal.
- **A2A `task_state` (`references/06`).** `metadata.task_state`
  (`completed` on success; absent on error) from the `InMemoryTaskStore`.
- **MCP handshake detail (`references/06`).** `initialize` capability
  negotiation and where `tools/call` puts the structured result +
  `_meta.trace_id`.
- **Bootstrap discovery (`references/06`).** The anonymous
  `GET /v1/runtime` payload SPAs read before login.
- **Error model (`references/06`).** Added the `RateLimited`
  (429 / `ratelimit` / `-32002`) row and clarified MCP carries the code
  in an HTTP-200 envelope.

## 0.1.0 — initial release

- Progressive-disclosure index over twelve references covering both
  integration roles: upstream-agent authoring and frontend/client
  authoring.
- Upstream-agent wire contract (`references/01`), A2UI envelope
  shapes (`references/02`), tool registration via Consul +
  `adapter.yaml` (`references/03`), OIDC bearer verification
  (`references/04`), chat-channel surface degradation
  (`references/05`).
- Frontend/client guidance (`references/06`), the `dev-token` local
  mode (`references/07`), and the `crates/triton-tests` consumer test
  harness (`references/08`).
- Audit/logging hygiene (`references/09`), hard prohibitions and
  escalation (`references/10`), and a cross-reference map to the
  `substrate-platform` skill (`references/11`).
- Four ready-to-fork templates: a Rust upstream-agent skeleton, a
  consumer integration-test skeleton, an `adapter.yaml` fragment, and
  a Nomad job stanza.
- Mapped against Triton spec `doc/requirements.md` §5.8 (FR-T),
  `doc/architecture.md` §8.7–§8.8 and ADR-13/ADR-16, and the worked
  walkthrough in `doc/consumer-integration-tests.md`.
