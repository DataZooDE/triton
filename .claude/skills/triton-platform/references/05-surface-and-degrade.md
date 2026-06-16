# 05 — Surfaces on chat channels: degrade rules and caps

If your tool's surface (→ `references/02`) is delivered to a chat
channel — Telegram, WhatsApp Web, Signal, MS Teams, Discord, Google
Chat — Triton's **surface mapper** projects the canonical envelope
onto a platform-neutral `PlatformMessage`, per the adapter's
`degrade` rule table in `adapter.yaml`. You don't write the mapper;
you design surfaces that survive degradation. Source:
`crates/triton-chat-*` adapters, `doc/architecture.md` §8.7, FR-A-9..13.

## The richness spectrum

The same surface lands differently depending on the channel
(`doc/architecture.md` §8.7 table):

| Adapter | Native vocabulary | What `degrade` does |
|---|---|---|
| Signal | text + media | buttons/selections/forms → **numbered prompts** |
| WhatsApp Web | text + media + quote + reactions | numbered prompts; text chunked at 4000 chars |
| Google Chat | text + media + threads | numbered prompts (Card v2 deferred) |
| Telegram | text + media + inline keyboards | native inline keyboard; ≤ 8 buttons/row |
| Discord | text + media + components v2 | native components; ≤ 25-item select |
| MS Teams | text + Adaptive Cards | near-lossless Adaptive Card projection |

## `degrade` rule keys (per component type)

The manifest's `adapter.<name>.degrade` maps each component type to a
rule. Observed values (`crates/triton-tests/fixtures/manifest-valid.yaml`):

- `text`, `narration`: `passthrough`
- `buttons`, `selections`: `inline_keyboard` (Telegram),
  `components_v2` (Discord), `numbered_prompts` (text-first)
- `forms`: `numbered_prompts` or `components_v2`
- `dashboard`: `rasterised_png`

Every component type your tool declares in `surface_components` must
have a rule in **every** chat adapter, or boot fails (FR-L-5; →
`references/03`).

## SurfaceLimits — caps you must respect

The mapper enforces per-platform caps at its edge and will reject or
reshape oversize surfaces (FR-A-10, M-RICHNESS-1):

- **Discord**: 25-item selection cap → oversize selects are rejected
  with `UnsupportedSurface`.
- **Telegram**: 8 buttons per row → larger button rows paginate into
  multiple `ButtonSet` fragments (label on the first page only).
- **WhatsApp Web**: 4000-char text chunk → long text splits into
  multiple `Text` fragments.

Design implication: keep selection lists ≤ 25 items if you target
Discord; keep button rows small. If you exceed a cap, the surface is
reshaped (text/buttons) or rejected (selections) — a `dashboard`
without a configured rasteriser on a text-first adapter is also
rejected with `UnsupportedSurface`.

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
