# 09 — Audit and logging: who emits what

Triton owns the audit trail. Your app emits its own *diagnostic* logs.
Keep the two straight and never leak secrets into either.

Source: `doc/architecture.md` §8.2, FR-AU-1..4; the upstream audit
emitter at `crates/triton-upstream/src/lib.rs` (`emit(&AuditRecord)`).

## What Triton audits (not you)

Every inbound call produces a **linked audit pair** sharing one
`trace_id`:

- A **dispatcher** record at receipt (`phase: dispatch` for chat
  channels; the inbound line for the HTTP trio).
- An **outbound** record: `phase: upstream` from the upstream router
  (your dispatch), or `phase: post` from a chat courier.
- Boundary rejections (bad bearer, bad signature) emit a
  `phase: rejected` record *before* the dispatcher runs.

Each line carries: `who`, `what`, `when` (RFC 3339 UTC), `env`,
`result` (`ok` | `error:<class>`), `protocol`, `tool`, `subject`,
`tenant`, `latency_ms`, `status`, `trace_id` (FR-AU-2).

**You do not emit these.** When Triton dispatches to your agent, it
writes the `phase: upstream` line itself. If your agent also emitted
an audit line per call, you'd double-count and break the one-pair
invariant. Don't.

## What your app logs

Your agent's own diagnostic logging is fine and encouraged — it's the
`kind: "log"` channel (level ∈ debug/info/warn/error), distinct from
`kind: "audit"`. Use it for your internal logic: "computed stats over
N rows", "cache miss", "downstream DB slow". Emit JSON lines to
stdout; the substrate collector ships them. Do **not** link a log
shipper (no Loki/Vector/OTel exporter) — that's substrate's job
(ADR-7, the hard prohibition in `references/10`).

## The non-negotiable: never log secrets

FR-AU-3 / NFR-S: tokens, JWKS private material, and Vault-minted
tokens MUST NEVER appear in audit lines or any log. This applies to
your agent too:

- The Vault-minted bearer Triton sends you → never log it. Log the
  verified `sub` only.
- Any platform credential or DB password → never log it.
- If you must correlate, log a token-hash *prefix*, not the token.

Triton's own tests assert this (`echo.rs`:
`assert!(!serialised.contains("dev-token"))`). Hold your agent to the
same bar — a leaked token in a log line is an incident, and these
logs land in a locked GCS bucket where they're hard to scrub.

## Correlating across the boundary

You can't see Triton's `trace_id` unless you propagate it yourself,
and v0.2 does not pass it into the upstream body. If you need to tie
your agent's logs to a Triton audit line, log your own request
identifier and timestamp; an operator joins on `(tool, subject,
when)` from the `phase: upstream` audit record. Don't invent a
side-channel to smuggle the trace_id — keep the dispatch body clean
(it's the validated args, nothing else).

## In tests

`TritonProcess::stdout_snapshot()` returns the spawned binary's
stdout lines; parse them as `serde_json::Value` to assert on the
audit pair (e.g. both phases share `trace_id`, the raw token never
appears). See `crates/triton-tests/tests/upstream.rs` for the
`wait_for_audit` pattern.
