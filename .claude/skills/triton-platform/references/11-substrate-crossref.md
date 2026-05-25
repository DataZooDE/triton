# 11 — Cross-reference: triton-platform vs substrate-platform

Triton runs *on* the DataZoo Hetzner substrate, and your app deploys
to the same substrate. So two skills touch the same surface from
different angles. The rule of thumb:

> **This skill (`triton-platform`) names what Triton expects of your
> app. The `substrate-platform` skill names how the substrate
> delivers the platform primitives your app (and Triton) rely on.**

If `substrate-platform` is also symlinked into your repo (it usually
is, for any substrate workload), reach for it on the topics below.

## Where the two overlap

| Topic | This skill says… | `substrate-platform` says… |
|---|---|---|
| **OIDC verification** | The *callee shape*: issuer = substrate issuer, audience = your agent, reject `none`/symmetric — `references/04`. | The *crypto*: discovery, JWKS cache/rotation, Rust/Python/Go verify recipes — `references/11-oidc-verification.md`. |
| **App-to-app auth** | Don't relay the inbound token; mint your own to call other services — `references/04`. | The per-app role + `aud` contract, mint+verify recipes — `references/14-app-to-app-auth.md`. |
| **Consul registration** | The discovery key Triton resolves: `tag:agent:<tool>`, no `urlprefix-` — `references/03`. | Service stanza conventions, health checks, Fabio routing, internal DNS — `references/03-consul-and-fabio.md`. |
| **Vault references** | Manifest credentials must be `vault://` refs — `references/03`. | Vault-from-a-job `template` stanzas, KV layout — `references/02-vault-from-a-job.md`. |
| **Nomad job shape** | The agent stanza tags + verifier wiring — `templates/agent.nomad.hcl`. | Full job templates (llm-agent, web-service, batch) + `update`/canary conventions — `templates/` and `references/01`. |
| **Logging / metrics / health** | What Triton audits vs what you log — `references/09`. | `/healthz` contract, structured logs, Prometheus metrics — `references/07-logging-metrics-healthchecks.md`. |
| **Deploy lifecycle** | — | Developer↔operator handoff, blue/green — `references/08-deploy-lifecycle.md`. |

## A practical split for "build me an agent for Triton"

1. **Shape the tool** with this skill: wire contract (`01`), A2UI
   surface (`02`), what to verify (`04`), chat degradation (`05`).
2. **Wire the deployment** with `substrate-platform`: fork its
   `llm-agent.nomad.hcl`, add the `tag:agent:<tool>` Consul service
   from this skill's `templates/agent.nomad.hcl`, follow its Vault
   and OIDC recipes for the actual code.
3. **Test it** with this skill's `references/08` + the
   `consumer-integration-test` template — real Triton, real fakes,
   no mocks.

## When neither skill covers it

A capability gap in *Triton* → PR to the Triton repo. A capability
gap in the *substrate* (a bucket, a policy, a hostname, a tag) → PR
to the substrate repo. Both escalation paths are in `references/10`
and `substrate-platform/references/09-out-of-bounds.md` respectively.
Don't improvise infra from inside your app.
