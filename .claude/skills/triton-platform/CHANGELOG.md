# Changelog — triton-platform skill

The skill version tracks the consumer-facing contract, not the Triton
binary version. The Triton repo's checked-out git ref is the true
version pin (see `SKILL.md` → "How this skill is installed").

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
