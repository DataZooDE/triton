# Changelog — triton-platform skill

The skill version tracks the consumer-facing contract, not the Triton
binary version. The Triton repo's checked-out git ref is the true
version pin (see `SKILL.md` → "How this skill is installed").

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
