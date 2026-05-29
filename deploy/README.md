# Deploying a demo Triton to the Hetzner substrate

A runnable, **tailnet-only, nonprod** demo of the Triton agent-ingress
gateway showcasing the MCP/A2A/REST trio + A2UI via the explorer, the
real dispatch → upstream-agent → Vault-token path, and a Telegram
channel. Low-friction (`dev-token`), **never public**.

> **Substrate model:** workloads are Docker containers on Nomad
> (`driver = "docker"`), images on `ghcr.io/datazoo` pinned by SHA
> (never `:latest`). Fabio fronts public HTTPS via `urlprefix-` Consul
> tags; tailnet-only services omit that tag and are reached via Consul
> DNS. See the `substrate-platform` skill for the full contract.

## Components

| Job | Image | What | Exposure |
|---|---|---|---|
| `dz-triton` | `ghcr.io/datazoo/dz-triton` | the gateway (`deploy/gateway/`) | tailnet-only |
| `dz-triton-demo-agent` | `ghcr.io/datazoo/dz-triton-demo-agent` | upstream agent, `tag:agent:demo-stats` (`apps/demo-agent/`) | tailnet-only |
| `dz-triton-explorer` | `ghcr.io/datazoo/dz-triton-explorer` | Flutter web UI (`apps/explorer/deploy/`) | tailnet-only |

## ⚠️ Demo posture (read this)

- The `dz-triton` image is built with the **`dev-token`** Cargo feature
  ON (triton-bin's default). The deployed binary accepts
  `Bearer dev-token` and registers `demo_panel` + the dev tools. This
  is a deliberate, **tailnet-only, nonprod** affordance. It MUST NOT
  get a Fabio `urlprefix-` tag and MUST NOT run in prod. A production
  image is built with `cargo build --release --no-default-features`
  (dev-token compiled out — ADR-10 / FR-T-1).
- No OIDC issuer is configured (`TRITON_OIDC_ISSUER` unset). Auth on
  the HTTP trio is `dev-token`; the Telegram leg uses the manifest's
  `sender_table`.
- Telegram uses the **`long_poll`** inbound (no public webhook needed);
  the worker polls `api.telegram.org` outbound. Requires
  `api.telegram.org` on the substrate egress allowlist.

## Prerequisites (confirm with the operator; substrate is live)

- GHCR push access to `ghcr.io/datazoo`.
- Vault `apps-dz` policy exists (read on `kv/data/apps/dz/*`) and you
  can write `kv/data/apps/dz/triton/nonprod/telegram`.
- `agent-oidc-swap` Vault role exists (upstream per-call token mint).
- `api.telegram.org` on the egress allowlist.
- A Telegram bot (via BotFather) → bot token; your Telegram numeric id.

## 1. Seed Vault (Telegram credentials)

`TRITON_ENV=nonprod` runs manifest validation in **production** mode,
so the manifest's credentials MUST be `vault://` refs (no literals).
Triton resolves them at boot via `TRITON_VAULT_URL` + `TRITON_VAULT_TOKEN`.

```sh
vault kv put kv/apps/dz/triton/nonprod/telegram \
  bot_token='123456:BotFatherToken' \
  webhook_secret='unused-in-long-poll' \
  senders='{"<your-telegram-id>":{"sub":"demo","scopes":["chat"],"tenant":"demo"}}' \
  correlation_key='replace-with-32+-random-bytes'
```

## 2. Build + push images (pin by SHA)

```sh
# gateway (full workspace; dev-token ON via default features)
docker build -f deploy/gateway/Dockerfile -t ghcr.io/datazoo/dz-triton:$VER .
# demo agent
docker build -f apps/demo-agent/deploy/Dockerfile -t ghcr.io/datazoo/dz-triton-demo-agent:$VER .
# explorer
docker build -f apps/explorer/deploy/Dockerfile -t ghcr.io/datazoo/dz-triton-explorer:$VER .
docker push ghcr.io/datazoo/dz-triton:$VER   # … and the other two
```

`$VER` convention: `<YYYY-MM-DD>-<git-short-sha>`. Capture the pushed
`@sha256:…` digest for each — the Nomad jobs pin by digest.

> Whether image build/push runs in **this** repo's CI or the substrate
> CI follows the explorer's existing path — confirm with the operator.

## 3. Hand off to the operator (substrate repo)

The Nomad jobs live here (`deploy/gateway/triton.nomad.hcl`,
`apps/demo-agent/deploy/demo-agent.nomad.hcl`,
`apps/explorer/deploy/explorer.nomad.hcl`). The operator runs, from the
substrate repo, for each job:

```sh
nomad job run -var=version=$VER -var=image=ghcr.io/datazoo/<job>@sha256:<digest> <job>.nomad.hcl
# blue/green:  /deploy-green <job> $VER   →  validate over tailnet  →  /promote <job>
```

## 4. Verify (over the tailnet)

- **Health/version:** `curl http://dz-triton.service.consul:8003/healthz` and `/version`.
- **MCP + A2UI:** point `deploy/local-e2e/mcp-smoke.sh` (or an MCP host) at
  `dz-triton.service.consul:8001` — `initialize`, `tools/list`
  (`echo`,`narrate`,`demo_panel`,`demo-stats`), `tools/call demo_panel`
  v0.9 (six component types) + v0.8, `echo`.
- **Upstream path:** `tools/call demo-stats` → routed via Consul to
  `dz-triton-demo-agent` → returns its dashboard surface;
  `nomad alloc logs dz-triton` shows a `phase: dispatch` + `phase:
  upstream` audit pair sharing one `trace_id`; the agent received a
  Vault-minted bearer (not dev-token).
- **Explorer:** open `http://dz-triton-explorer.service.consul:8080`
  over the tailnet; render `demo_panel` v0.8/v0.9 side-by-side.
- **Telegram:** DM the bot from the enrolled account → the long-poll
  worker dispatches → reply arrives; logs show `phase: dispatch` +
  `phase: post` under `protocol: messenger:telegram`.

## Open items to confirm

- Exact `TRITON_VAULT_TOKEN` wiring under Nomad workload identity (the
  `vault{}` stanza + `template` in `triton.nomad.hcl` reads
  `{{ env "VAULT_TOKEN" }}` — confirm the token path with the operator).
- Where images are built (this repo's CI vs substrate CI).
- Explorer behaviour with no OIDC issuer advertised (expected: falls
  back to `dev-token`; verify).
- Demo-agent token verification — accept-and-log for the demo; harden
  per `substrate-platform/references/11` before any non-demo use.
- `dashboard` over the Telegram leg needs a rasterizer; the demo shows
  `demo-stats` via MCP/explorer (native v0.9 dashboard), not Telegram.
