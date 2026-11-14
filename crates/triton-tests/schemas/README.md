# Vendored JSON schemas (test-only)

Official schemas the conversation-component golden tests validate Triton's
channel output against (`tests/conversation_components.rs`). Vendored, not
fetched, so the test is hermetic and a schema change upstream is a reviewed
bump here. Files are byte-identical to the source; the test only aliases
`$id`s in memory (see below).

| File | Source (pinned) | sha256 |
|---|---|---|
| `adaptive-cards/adaptive-card-1.5.0.json` | https://raw.githubusercontent.com/microsoft/AdaptiveCards/dd065282fdf42eeb2ba8e19afddb0d8d36f8f1c9/schemas/1.5.0/adaptive-card.json | `32c91fb285180a266a55b3475dd0396e63fe079311714d3a14963b41f5e17920` |

The A2UI v0.9 schemas moved to `crates/triton-core/schemas/` (#370): the
runtime vets producer passthrough surfaces against them, and the tests read
the same copy from there.

How the test uses them:

- **A2UI v0.9.** `server_to_client.json` references the active catalog as
  `catalog.json` (relative to `https://a2ui.org/specification/v0_9/`). The
  test registers the **Gemini Enterprise composite catalog** under that URL
  (its own `$id` rewritten in memory to the alias), which is exactly the
  catalog Triton declares in `createSurface.catalogId`, so every message
  Triton sends Gemini is validated against the components GE actually knows.
  Format assertions are on (`openUrl` URLs must be URIs).
- **Adaptive Cards.** The open-source 1.5 schema does not know the Teams host
  extension `msteams` (`{"width": "Full"}` on the card), so the test strips
  that one top-level key before validating; everything else is validated
  as sent.
