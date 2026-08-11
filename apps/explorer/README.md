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

## Targets, and the `ui://` embedder

Web is the product. Linux and Android exist so the *renderers* can be
exercised where a browser can't: `ui://` MCP-Apps resources embed
differently per target and #201 was the bug that fell out of assuming
otherwise.

| Target | `ui://` resource is… | Where |
|---|---|---|
| web | a sandboxed `<iframe srcdoc>` | `html_embed_web.dart` |
| Android / iOS | that same iframe, inside a WebView | `html_embed_webview.dart` |
| Linux / Windows / macOS | its HTML source — `webview_flutter` has no desktop implementation, so support is **deferred**, not silently broken | `html_embed_source.dart` |

The bridge (`callServerTool` / `prompt` / `updateModelContext`) is the
same on every arm that has one, and the guest HTML is byte-identical
across them: an upstream never learns which host it landed in.

Android sign-in is NOT wired up — `auth_manager.dart` builds an OIDC
manager only on web — so the Android build is for renderer work, not a
shipping mobile client.

The device test for the bridge (needs a running emulator or a phone):

```bash
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk   # a JDK 26 default breaks Gradle
cd apps/explorer
flutter test integration_test/ui_resource_embed_test.dart -d emulator-5554
```

It skips itself on desktop rather than passing vacuously. Everything it
cannot see — the guest is encoded so it can't terminate the host's
script block; desktop degrades instead of crashing — is covered on the
VM by `test/html_embed_webview_test.dart`, which CI does run.

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
