# 05 — Surfaces on chat channels: degrade rules and caps

> **Gemini Enterprise and native A2UI.** Producer-built v0.9 surfaces in
> `surface.a2ui` bypass the degrade path entirely: they go verbatim to A2UI
> clients only, and chat channels render `components`. See `references/01` §(c).

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

## Conversation components per channel

`send_message` actions are signed per channel:

- **Teams:** `Action.Execute` `data.ct`, bound to tenant + sender, 7-day
  TTL; fixed messages carry `associatedInputs: none`; card inputs only fill
  a template's placeholders, never the signed text.
- **Gemini Enterprise:** in the clicked component's **id** (`a<N>-<token>`,
  the only field GE echoes back), key `TRITON_A2UI_ACTION_KEY` (same on
  every replica; boot fails on a malformed value). Bound to the platform
  only — the GE principal is not known when the card is built — which is
  safe for `send_message` (it runs under the clicker's own principal):
  30-day TTL. Open-doc actions carry a signed `/docs` URL and live 24 h.
  The token body is readable (integrity, not confidentiality).
- Budget: a token is capped at 2048 bytes on GE and 1536 on Teams; a
  control whose request does not fit is dropped and logged. Keep requests
  to a sentence.
- Only `https://` links are actionable; any other scheme is dropped in the
  canonical model, on every channel. For local end-to-end tests,
  `TRITON_ALLOW_HTTP_LINK_HOSTS=127.0.0.1,localhost` keeps `http://` links to
  the listed hosts; the binary refuses it unless `TRITON_ENV=local`.

| Component | Gemini Enterprise (A2UI v0.9, composite catalog) | Teams (Adaptive Card 1.4) | Text (`a2ui::text`) |
|---|---|---|---|
| `list` | `MaterialColumn` per row; actionable label is a `MaterialButton` | `Container` + `selectAction` | `1. label — detail`, URL or `› message` below |
| `table` | `MaterialTable` + one button per actionable row | `ColumnSet` grid, row `selectAction` | numbered `cell · Col: cell` rows |
| `chips` | `MaterialButton`s (stroked), 2 per `MaterialRow` | `ActionSet` of `Action.Execute` | `› <complete message>` per chip |
| `tree` | nested `MaterialExpansionPanel` (client-side expand); **no "only gaps" toggle** (no visibility binding in the catalog) — gap nodes start expanded, marked ⚠ | nested `Container`s, header `Action.ToggleVisibility`; "only gaps" = one toggle over every gap-free node | indented map, two spaces per level |
| `notice` | `MaterialIcon` (info/warning) + `MaterialText` | `Container` style `accent`/`warning` | `ⓘ …` / `⚠ …` |
| `progress` | `MaterialProgressBar` + icon rows | status `TextBlock`s + `%` | `✓ / ▶ / ○` lines + `[#####-----] 50 %` |
| `key_value` | `MaterialTable` (label, value) | `FactSet`, `before → value` | `Label: before → value` |
| `selection` + `submit` | **one button per option** (GE posts `context:{}`, so a client-side radio value never arrives) | `Input.ChoiceSet` (`expanded` = radio) + preview + `Action.Execute` | `› <template filled per option>` |
| `form` + `send_template` | one select field in the template → one button per option; otherwise **preview text only** (GE cannot submit inputs) | `Input.*` + "will be sent as" + `Action.Execute`; callback fills `{name}` from inputs, refuses a missing one | `› <template with ‹Label›>` + options |
| `button` `open_url` | `MaterialButton` + `functionCall openUrl` only with `TRITON_GE_OPEN_URL=1` (unqualified live); else omitted — put the link in the prose | `Action.OpenUrl` | `label: url` |
| `embed` | `Canvas` + `IFrameUrl` surface with `TRITON_GE_EMBED=1` (host must be on GE's allowlist); else the fallback inline | fallback inline + `Action.Submit` task/fetch → URL task module (host in the app's `validDomains`) | fallback text + `title: url` |

Degrade rules for text adapters: `lists/tables: numbered_list`,
`chips: copy_lines`, `trees: indented_text`, `progress: status_lines`,
`key_values: key_value_lines`, `embeds: inline_fallback`; Teams:
`adaptive_card` / `adaptive_card_column_set`, `embeds: task_module`.

## Theming — peacock owns ALL of it

There is **no theme config in Triton** (the manifest `theme:` block was
removed in #176; stale blocks parse-and-ignore). The deployment's brand
is ONE CSS file of `--pk-*` tokens registered with the report upstream
(peacock `PEACOCK_BRAND_CSS`); it themes every chart PNG, every `ui://`
iframe, AND the chat-card chrome — adapters that render cards fetch the
resolved brand per reply via the upstream's `get_theme` tool. Operator
wiring: add `get_theme=<peacock>` to `TRITON_STATIC_UPSTREAMS`. No
`get_theme` upstream registered ⇒ unbranded cards (the pre-theme look).
