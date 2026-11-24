# mcp-widget: Triton's MCP Apps answer card

The page an MCP Apps host (Claude, ChatGPT) loads for Triton's tools
(`ui://triton/answer-card-<hash>.html`, `text/html;profile=mcp-app`). It
bundles the official A2UI v0.9 renderer (`@a2ui/lit` + `@a2ui/web_core`,
basic catalog) and Triton's host glue (`src/main.js`), and renders the A2UI
messages a `tools/call` result carries in `_meta["triton/a2ui"]`
(`crates/triton-core/src/a2ui/mcp_app.rs`, `basic.rs`).

- **One file, no network.** vite + `vite-plugin-singlefile` inline every
  script and style; the page's CSP has `connect-src 'none'`. Do not add
  anything that fetches (fonts, images by URL, CDNs).
- **Committed output.** `npm run build` writes
  `crates/triton-core/src/a2ui/mcp_widget.html`, which `triton-core`
  `include_str!`s, so cargo never needs Node. `scripts/check.sh mcp-widget`
  (pre-push and CI) runs `npm ci && npm test && npm run build` and fails if
  the committed copy differs. A changed page gets a new `ui://` URI (the URI
  names its hash), so hosts that cache by URI pick it up after "Refresh tools".
- **Controls.** Triton's card fires two events the glue handles itself:
  `triton.send_message` (`{text}`, or a form's `{parts, v:<field>…}` filled by
  `src/fill.js` exactly as the server's `fill_template` would) as `ui/message`,
  and `triton.open_url` (`{url}`, https only) as `ui/open-link`.
- **In-card actions (#369).** `triton.in_card` (`{token, fallback}`) on
  Triton's card, and a producer surface's `triton.in_card:<name>` events (with
  the result's `_meta["triton/in_card"]` token), call the app-only tool
  `triton_ui_event` (`tools/call` over the bridge when the host reports
  `serverTools`, else ChatGPT's `window.openai.callTool`). The result replaces
  the card in place and its text goes to the model with
  `ui/update-model-context` (ChatGPT without the bridge: `setWidgetState`).
  On any failure (refusal, error, a 15 s timeout, an answer the renderer
  takes none of) a Triton control's `fallback` is sent as `ui/message`; a
  producer event has no fallback.
- **Status of a filed item.** A result's `_meta["triton/status"]`
  (`crates/triton-core/src/a2ui/item_status.rs`, contract §4c) is shown under
  the answer (its label, else its state). While it is not terminal and carries
  a token, the card calls `triton_ui_event` with `triton.status` on the
  result's schedule (`src/item-status.js`: 5 s doubling to 60 s, stop after
  30 min), only the status line changes, and it stops at a terminal state,
  after three failed polls in a row, or when a new result arrives. A hidden
  card waits until it is shown.
- **Tests.** `npm test` runs `src/item-status.js`, and `src/fill.js` against
  the shared vectors (`crates/triton-tests/fixtures/conversation/fill_vectors.parts.json`,
  pinned by the Rust test). The browser flows run in
  `deploy/local-e2e/mcp-widget/run.sh` (rodney).

## Bumping a dependency

Versions are exact pins with a lockfile. To bump `@a2ui/lit`/`@a2ui/web_core`
(keep them on the same version):

```bash
cd packages/mcp-widget
npm install --save-exact @a2ui/lit@X.Y.Z @a2ui/web_core@X.Y.Z
npm test && npm run build
cd ../.. && cargo test -p triton-tests --test it mcp_app_widget
deploy/local-e2e/mcp-widget/run.sh
```

Then commit `package.json`, `package-lock.json` and the rebuilt
`mcp_widget.html` together. Check the page size in the PR (the Rust test caps
it at 768 KiB).
