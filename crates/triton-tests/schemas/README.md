# Vendored JSON schemas (test-only)

Official schemas the conversation-component golden tests validate Triton's
channel output against (`tests/conversation_components.rs`). Vendored, not
fetched, so the test is hermetic and a schema change upstream is a reviewed
bump here. Files are byte-identical to the source; the test only aliases
`$id`s in memory (see below).

| File | Source (pinned) | sha256 |
|---|---|---|
| `a2ui-v0_9/server_to_client.json` | https://raw.githubusercontent.com/a2ui-project/a2ui/825beac2813a6d0ccef3b6fb4124ad6c3c8419f0/specification/v0_9/json/server_to_client.json | `056d2522e4e2d9eb72b29399fbab44f44341a6179d3243d19ccf0c699a5cbf56` |
| `a2ui-v0_9/common_types.json` | …/825beac…/specification/v0_9/json/common_types.json | `a13897d7dbe14c37512c5cbc9b8561782fc3b01e3a6ff114c1f12c82275b5884` |
| `a2ui-v0_9/client_to_server.json` | …/825beac…/specification/v0_9/json/client_to_server.json | `76c9a6f54e40bbcc1ed7b36b0d56563b82122ea7f9d8379787c4afcf29cc95e0` |
| `a2ui-v0_9/basic_catalog.json` | …/825beac…/specification/v0_9/catalogs/basic/catalog.json | `8cc94d0a482e67048f9fc989964ca5da56fe42f531d919315a508989fb22e13e` |
| `a2ui-v0_9/gemini_enterprise_composite_catalog.json` | …/825beac…/samples/community/agent/adk/gemini_enterprise/v0_9/gemini_enterprise_composite_catalog.json (`$id` = the gstatic catalog id Triton declares) | `8ee40b042b9b6d5c2675219690683f7c5ca3fd18d8c9f50f8f0657cd2590fb84` |
| `adaptive-cards/adaptive-card-1.5.0.json` | https://raw.githubusercontent.com/microsoft/AdaptiveCards/dd065282fdf42eeb2ba8e19afddb0d8d36f8f1c9/schemas/1.5.0/adaptive-card.json | `32c91fb285180a266a55b3475dd0396e63fe079311714d3a14963b41f5e17920` |

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
