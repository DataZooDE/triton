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
| **OIDC verification** | The *callee shape*: issuer = Triton's self-issuer (`TRITON_SELF_ISSUER`), audience = `agents-<env>`, reject `none`/symmetric — `references/04`. | The *crypto*: JWKS discovery, cache/rotation, Rust/Python/Go verify recipes. |
| **App-to-app auth** | Don't relay the inbound token; mint your own to call other services — `references/04`. | The per-app identity + `aud` contract, mint+verify recipes. |
| **Tool discovery** | The routing key Triton resolves: a tool name in `TRITON_STATIC_UPSTREAMS=name=host:port` (no Consul) — `references/03`. | Tailnet naming, internal DNS, how services reach each other. |
| **Secret references** | Manifest credentials must be `env://VARNAME` refs (`vault://` fails closed) — `references/03`. | Secret Manager → kamal `.kamal/secrets` → container env injection. |
| **Deploy shape** | Your agent is a Kamal app reachable on the tailnet; the operator names it in Triton's `TRITON_STATIC_UPSTREAMS`. | The Kamal `deploy.yml` + `apps/registry.yml` row, image pinning, exposure (internal/external). |
| **Logging / metrics / health** | What Triton audits vs what you log — `references/09`. | `/healthz` contract, structured logs, Prometheus/GCP metrics. |
| **Deploy lifecycle** | — | Developer↔operator handoff, the two-actor PR/apply model. |

## A practical split for "build me an agent for Triton"

1. **Shape the tool** with this skill: wire contract (`01`), A2UI
   surface (`02`), what to verify (`04`), chat degradation (`05`).
2. **Wire the deployment** with `substrate-platform`: package your
   agent as a Kamal app + an `apps/registry.yml` row, follow its
   secret-injection and OIDC-verification recipes for the actual code.
   Then ask the Triton operator to add your `tool=host:port` to
   Triton's `TRITON_STATIC_UPSTREAMS` (substrate-side config).
3. **Test it** with this skill's `references/08` + the
   `consumer-integration-test` template — real Triton, real fakes,
   no mocks.

## When neither skill covers it

A capability gap in *Triton* → PR to the Triton repo. A capability
gap in the *substrate* (a bucket, a Secret Manager entry, a hostname,
a tag) → PR to the substrate repo. Both escalation paths are in
`references/10`
and `substrate-platform/references/09-out-of-bounds.md` respectively.
Don't improvise infra from inside your app.
