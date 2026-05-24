# Triton Explorer

Internal exploration UI for the Triton agent-ingress gateway. Lets a
human poke at every feature Triton currently exposes — `/healthz`,
`/version`, `/v1/tools`, the REST/MCP/A2A trio, and the A2UI v0.8 +
v0.9 builders — without reading Rust source.

**Tailnet-only.** This app is deployed inside the DataZoo Hetzner
substrate with no Fabio `urlprefix-` tag, mirroring how Triton's
`/metrics` listener is locked down.

## Local dev

```bash
# 1. Run a local Triton with CORS enabled for the SPA origin and a
#    dev OIDC client_id so /v1/runtime returns enough info:
cargo run -p triton-bin -- \
    --cors-allowed-origins http://localhost:8080,http://localhost:5000 \
    --explorer-client-id triton-explorer-dev

# 2. In another terminal, run the SPA:
cd apps/explorer
flutter pub get
flutter run -d chrome --web-port 5000
```

The SPA reads `/v1/runtime` at boot to discover the OIDC issuer +
client_id. If those env vars aren't set on Triton, the login screen
shows a clear "operator hasn't registered me" message instead of
failing PKCE opaquely.

## Layout

```
lib/
  main.dart                # ProviderScope + MaterialApp
  theme/app_theme.dart     # copy of heron's tokens (teal + Inter)
  api/                     # REST/MCP/A2A clients (PR E2)
  auth/                    # OIDC PKCE + login screen
  providers/               # Riverpod providers
  ui/
    shell/app_shell.dart   # rail nav + IndexedStack
    features/              # one folder per top-level page
```

## PR roadmap (this app)

- E1 (this PR) — scaffold, theme, login screen, page stubs.
- E2 — REST/MCP/A2A Dio clients, tools playground, integration test.
- E3 — A2UI v0.8 + v0.9 renderers, side-by-side diff.
- E4 — Adapters compare, dashboard polish, audit stub.
- E5 — Docker + Nomad jobspec + CI smoke against latest Triton.

See `~/.claude/plans/can-you-think-of-refactored-axolotl.md` for the
full plan.
