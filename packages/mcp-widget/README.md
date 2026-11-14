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
- **Tests.** `npm test` runs `src/fill.js` against the shared vectors
  (`crates/triton-tests/fixtures/conversation/fill_vectors.parts.json`, pinned
  by the Rust test). The browser flows run in
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
