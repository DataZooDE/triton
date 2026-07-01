# triton

**Rust multi-protocol agent-ingress gateway** for the DataZoo / Hetzner agent
substrate. Triton sits between your upstream **agents** and the outside world:
clients reach it over MCP, A2A, REST, or a **chat channel** (Google Chat,
Telegram, Teams, Discord, Signal, WhatsApp); Triton authenticates the caller,
resolves their identity, dispatches to the right agent with a freshly minted
per-call token, and renders the agent's canonical **A2UI surface** into
whatever the channel can display.

Agents never speak the channels themselves — they emit **one** protocol- and
channel-agnostic surface, and Triton owns every frontend.

## What it does

- **One agent, many surfaces.** An agent replies with an A2UI `surface`
  (`{ components: [...] }`); Triton maps it to MCP-Apps, an A2A/REST JSON
  payload, or a native chat card. Parity across surfaces is by construction.
- **Ingress + egress auth.** Inbound callers are verified per protocol
  (per-channel signed JWTs, an OIDC audience, or the dev-token fallback in
  dev builds). Triton then mints a **per-call OIDC token** to the upstream
  agent (workload→workload, no shared secret) — or falls back to a static
  bearer in dev. Agents never receive the inbound bearer.
- **Identity resolution.** A chat sender (`users/<id>`, a Telegram id, …) is
  resolved to a `{sub, scopes, tenant}` principal via a `sender_table`,
  `self_enrol` pairing, or an upstream resolver tool — so downstreams get
  per-user authz, tenancy, and audit.
- **One audit pivot.** Every inbound is audited (`dispatch` / `post` /
  `rejected`) in one JSON-line shape, keyed by principal + tenant.
- **Governance.** Config via `TRITON_*` env; secrets are Vault refs in prod
  (literals refused outside `local`); a per-adapter egress allowlist (NFR-S-4)
  bounds where each adapter may call.

## Surfaces & ports

Listeners (defaults; `TRITON_SINGLE_PORT=true` nests MCP/A2A/REST on one):

| Surface | Default port | Env |
|---|---|---|
| MCP (incl. MCP-Apps) | 8001 | `TRITON_MCP_PORT` |
| A2A | 8002 | `TRITON_A2A_PORT` |
| REST | 8003 | `TRITON_REST_PORT` |
| Chat webhooks (`/<adapter>/webhook`) | 8004 | `TRITON_CHAT_WEBHOOK_PORT` |
| Metrics (tailnet-only) | 9090 | `TRITON_METRICS_PORT` |

**MCP-Apps** (`#143`): Triton proxies `resources/read` of an upstream-owned
`ui://` resource, relays `callServerTool` / `updateModelContext`, and can
delegate dashboard→PNG rasterisation to an upstream
(`TRITON_RASTERIZE_UPSTREAM`) — e.g. an interactive renderer like `peacock`.

## Chat channels

Each adapter is configured in `adapter.yaml` (`kind:`, inbound signature,
identity strategy, degrade rules, rate limits). Transports and richness
differ:

| Channel | Transport / inbound auth | Interactive surface |
|---|---|---|
| **Google Chat** | synchronous webhook; Google-signed JWT (legacy `chat@system` or OIDC add-on actor) | **full** — text · buttons · dropdown (`Selection`) · form · dashboard (tile grid **or** rasterised chart PNG) with a verified click-callback round-trip |
| **Telegram** | `long_poll` or `webhook`; secret-token | text · buttons (inline keyboard) · dashboard (rasterised PNG photo) |
| **Discord / WhatsApp / Signal** | webhook; per-platform signature | text · buttons (via `triton-correlation`) |
| **Microsoft Teams** | Bot Framework; inbound BF JWT, **async** Bot Connector out (OAuth client credentials) | text-only today (Adaptive Card render/callback is the follow-up) |

### The interactive surface (A2UI components)

Agents emit a closed set of components; adapters render what the channel
supports and defer the rest:

`Text` · `Narration` · `Button` · `Selection` (pick-one) · `Form`
(multi-field) · `Dashboard` (metric tiles / chart).

Interactive components round-trip safely via **`triton-correlation`**: the
component's `(tool, args)` is signed into an HMAC token carried on the
widget; on the callback (Google Chat `CARD_CLICKED`, Telegram
`callback_data`, …) Triton **verifies** the token — a forged callback can't
drive an arbitrary tool — merges any user-entered form/selection values, and
re-dispatches. Google Chat additionally echoes the tapped control in the
reply (chat renders no user message for a click) and serves rasterised chart
PNGs on demand at a signed `…/img/{token}` route.

## Crates

| Crate | Role |
|---|---|
| `triton-core` | A2UI surface + component vocabulary, `Dispatcher`, `Principal`, ratelimit |
| `triton-manifest` | `adapter.yaml` schema |
| `triton-secrets` | secret resolution (literals in dev, `vault://` in prod) |
| `triton-identity` | JWT verification primitives |
| `triton-correlation` | HMAC sign/verify for interactive-widget callbacks |
| `triton-rasterizer` | Dashboard → SVG → PNG (pure-Rust resvg/tiny-skia) + upstream delegate |
| `triton-adapters-http` | MCP / A2A / REST listeners |
| `triton-chat-*` | Google Chat, Telegram, Teams, Discord, Signal, WhatsApp |
| `triton-embed` | embeddable trio (used by downstream agents' e2e harnesses) |
| `triton-upstream` | static-upstream dispatch + minted-token client |
| `triton-bin` | the `triton` binary: settings, wiring, lifecycle |
| `triton-tests` | no-mock integration harness (`TritonProcess`, `FakeGoogleJwks`, …) |

## Configuration & running

12-factor: config via `TRITON_*` env, JSON-line logs to stdout, `/healthz`,
`/version`, graceful SIGTERM drain. Minimal local run:

```sh
cargo build -p triton-bin
TRITON_ENV=local \
TRITON_MANIFEST_PATH=adapter.yaml \
TRITON_STATIC_UPSTREAMS=assistant=127.0.0.1:8090 \
target/debug/triton
```

- `TRITON_STATIC_UPSTREAMS=name=host:port` registers an upstream agent by
  name (dev). With `TRITON_JWT_SIGNING_KEY` + `TRITON_SELF_ISSUER` + a served
  JWKS, Triton mints per-call OIDC tokens instead of the static bearer.
- Outside `TRITON_ENV=local`, credentials must be `vault://…` refs.

## Testing

No mocks at the boundary under test (CLAUDE.md §1): integration tests spawn
the **real** `triton` binary over real HTTP with real signed JWTs (e.g.
`FakeGoogleJwks` serves a real cert). Pre-push gate:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

BUSL-1.1 · © DataZoo GmbH
