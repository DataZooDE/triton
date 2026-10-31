# 05 — Surfaces on chat channels: degrade rules and caps

> **Gemini Enterprise and native A2UI.** Producer-built v0.9 surfaces in
> `surface.a2ui` bypass the degrade path entirely: they go verbatim to A2UI
> clients only, and chat channels render `components`. See
> `references/01` §(c).

If your tool's surface (→ `references/02`) is delivered to a chat
channel — Telegram, WhatsApp Web, Signal, MS Teams, Discord, Google
Chat — Triton's **surface mapper** projects the canonical envelope
onto a platform-neutral `PlatformMessage`, per the adapter's
`degrade` rule table in `adapter.yaml`. You don't write the mapper;
you design surfaces that survive degradation. Source:
`crates/triton-chat-*` adapters, `doc/architecture.md` §8.7, FR-A-9..13.

## The richness spectrum

Each chat crate has its own mapper (`crates/triton-chat-*/src/surface_mapper.rs`)
and declares what its channel can show once, as `pub const SURFACE:
SurfaceProfile` (#352). The mapper reads its caps from that const, and
Triton sends the same profile to your agent as `_triton.surface`, so you
can see these caps at runtime instead of hard-coding them:

| Channel | Text cap | Tables | Buttons | Charts (`report`) |
|---|---|---|---|---|
| WhatsApp (Cloud + Web bridge) | 4096 B | raw text | 3 reply buttons, rest deferred | dropped |
| Twilio WhatsApp / RCS | 1600 B | raw text | deferred | dropped |
| Signal | 2000 B | raw text | deferred | dropped |
| Telegram | 4096 B | raw text | inline keyboard | dropped |
| Discord | 2000 B | raw text | ≤ 25 components | dropped |
| Google Chat | 32 000 B | Cards v2 table | card buttons | image |
| MS Teams | 24 576 B | Adaptive Card table | card actions | native chart |
| Email | 512 000 B | HTML | links | inline image |

Dashboards are rasterised to an image on WhatsApp, Telegram and Discord.
`sources` degrades to one "Sources: …" label line on the text-first
channels. Source of truth: `doc/architecture.md` §8.7 and §8.7.1.

## `degrade` rule keys (per component type)

The manifest's `adapter.<name>.degrade` maps each component type to a
rule. It is **validation-only**: boot checks coverage (FR-L-5), but no
mapper reads it. Observed values (`crates/triton-tests/fixtures/manifest-valid.yaml`):

- `text`, `narration`, `sources`, `report`: `passthrough`
- `buttons`, `selections`: `inline_keyboard` (Telegram),
  `components_v2` (Discord), `card_v2` (Google Chat),
  `numbered_prompts` (text-first)
- `forms`: `numbered_prompts`, `components_v2` or `card_v2`
- `dashboard`: `rasterised_png` (delegated to the report upstream via
  `TRITON_RASTERIZE_UPSTREAM=render_a2ui_to_png`: peacock renders,
  Triton transports)

Every component type your tool declares in `surface_components` must
have a rule in **every** chat adapter, or boot fails (FR-L-5; →
`references/03`).

## Caps you must respect

Read them from `_triton.surface` rather than this page. Two that bite:

- **A correlation token must fit the platform's callback cap**
  (64 bytes on Telegram and WhatsApp). A button whose `tool` + `args`
  are too long is **deferred**: logged as a warning, never shown. Keep
  follow-up button args tiny.
- **Text over the cap is truncated** with a sentinel line, not split.

## Dashboards need a rasteriser

`dashboard` components are non-negotiably visual. On text-first
adapters the mapper delegates to an out-of-process **Rasterizer** and
emits a `Fragment::Media` carrying the rendered PNG plus a caption
from the dashboard's narration child (FR-A-11, M-RASTER-1). The
rasteriser is realised as either:

- an **upstream tool named `render_a2ui_to_png`** (preferred —
  inherits identity + audit symmetry through the upstream router), or
- a peer sidecar service.

MS Teams projects dashboards onto an Adaptive Card `ColumnSet`
natively and skips the rasteriser. If you emit dashboards and target
any text-first channel, **the operator must configure a rasteriser**
or boot/render fails. If you *are* building the rasteriser, it's just
another upstream agent (→ `references/01`) whose tool is
`render_a2ui_to_png` returning `{png_bytes, caption}`; see
`crates/triton-tests/src/rasterizer_fixture.rs` for the wire shape.

## Correlation tokens (interactive options)

Every button / selection option the mapper emits carries an
HMAC-signed `(tool, args)` token under the adapter's `CorrelationKey`
(FR-A-12, M-CORRELATION-1). When the user taps, the inbound listener
verifies the HMAC in constant time and re-enters the dispatcher with
a fresh `trace_id`. You don't implement this — but know that:

- The token is base64url JSON: the platform *can* read your tool name
  and args (HMAC protects integrity, not confidentiality). Don't put
  secrets in `args`.
- The principal always comes from the inbound sender, never from the
  token body — a hostile platform actor can't forge a token that
  decodes to an unauthorised tool.
- Platform callback-data caps are tight (Telegram 64 bytes). Keep
  `tool` names and `args` for interactive components small, or the
  token won't fit and the mapper degrades/rejects.

## Theming — peacock owns ALL of it

There is **no theme config in Triton** (the manifest `theme:` block was
removed in #176; stale blocks parse-and-ignore). The deployment's brand
is ONE CSS file of `--pk-*` tokens registered with the report upstream
(peacock `PEACOCK_BRAND_CSS`); it themes every chart PNG, every `ui://`
iframe, AND the chat-card chrome — adapters that render cards fetch the
resolved brand per reply via the upstream's `get_theme` tool. Operator
wiring: add `get_theme=<peacock>` to `TRITON_STATIC_UPSTREAMS`. No
`get_theme` upstream registered ⇒ unbranded cards (the pre-theme look).
