# Vendored A2UI v0.9 schemas (runtime + tests)

Official A2UI v0.9 schemas, `include_str!`-ed by
`src/a2ui/passthrough.rs` to vet producer-built passthrough surfaces
(#370), and read by the conversation-component golden tests in
`crates/triton-tests`. Vendored, not fetched: the binary never resolves a
schema over the network, and a schema change upstream is a reviewed bump
here. Files are byte-identical to the source; both users only alias `$id`s
in memory (`common_types.json` refers to the active catalog as the relative
`catalog.json`).

| File | Source (pinned) | sha256 |
|---|---|---|
| `a2ui-v0_9/server_to_client.json` | https://raw.githubusercontent.com/a2ui-project/a2ui/825beac2813a6d0ccef3b6fb4124ad6c3c8419f0/specification/v0_9/json/server_to_client.json | `056d2522e4e2d9eb72b29399fbab44f44341a6179d3243d19ccf0c699a5cbf56` |
| `a2ui-v0_9/common_types.json` | …/825beac…/specification/v0_9/json/common_types.json | `a13897d7dbe14c37512c5cbc9b8561782fc3b01e3a6ff114c1f12c82275b5884` |
| `a2ui-v0_9/client_to_server.json` | …/825beac…/specification/v0_9/json/client_to_server.json | `76c9a6f54e40bbcc1ed7b36b0d56563b82122ea7f9d8379787c4afcf29cc95e0` |
| `a2ui-v0_9/basic_catalog.json` | …/825beac…/specification/v0_9/catalogs/basic/catalog.json | `8cc94d0a482e67048f9fc989964ca5da56fe42f531d919315a508989fb22e13e` |
| `a2ui-v0_9/gemini_enterprise_composite_catalog.json` | …/825beac…/samples/community/agent/adk/gemini_enterprise/v0_9/gemini_enterprise_composite_catalog.json (`$id` = the gstatic catalog id Triton declares) | `8ee40b042b9b6d5c2675219690683f7c5ca3fd18d8c9f50f8f0657cd2590fb84` |

Bumping: replace the files from one a2ui commit, update the table, and run
`cargo test -p triton-core a2ui::passthrough` plus the
`conversation_components` / `a2ui_passthrough_vetting` integration tests.
