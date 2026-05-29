# 02 — A2UI envelopes: what your tool returns

When your tool drives a UI surface, return a canonical `surface`
object. **You do not build the versioned envelope** — Triton's
builders do, per the version the caller negotiated (ADR-4). You emit
one shape; Triton projects it to v0.8 *or* v0.9 *or* a chat-channel
`PlatformMessage`.

Source: `crates/triton-core/src/a2ui/mod.rs` (the canonical `Surface`
+ `Component` types), `v08.rs`, `v09.rs` (the two builders).

## The shape you return

```json
{ "surface": { "components": [ <component>, … ] } }
```

Each component is tagged by `kind` (snake_case). The full vocabulary
(both builders handle every variant — the compiler enforces it):

| `kind` | Fields | Use for |
|---|---|---|
| `text` | `value` | Plain text. |
| `narration` | `text` | LLM-generated prose, semantically distinct from `text`. |
| `button` | `label`, `tool`, `args` | A tap that re-invokes `tool` with `args`. |
| `selection` | `prompt`, `options[]`, `tool`, `args_key` | Pick one option; re-invokes `tool` with the chosen value bound to `args_key`. |
| `form` | `title`, `fields[]`, `submit_label`, `tool` | Multi-field input submitted as one tool call. |
| `dashboard` | `title`, `tiles[]` | Read-only grid of summary tiles. Rasterised to PNG on text-first chat channels (→ `references/05`). |

`options` items are `{label, value}`. `form` fields are `{name,
label, kind, required}` where `kind ∈ {string, integer, boolean}`.
`dashboard` tiles are `{label, value, trend?}`.

## Example

```json
{ "surface": { "components": [
  { "kind": "text", "value": "Choose a report window:" },
  { "kind": "selection",
    "prompt": "Window",
    "options": [
      { "label": "Last 7 days",  "value": "7d" },
      { "label": "Last 30 days", "value": "30d" }
    ],
    "tool": "compute_stats",
    "args_key": "window" }
] } }
```

## How Triton renders it (you don't choose)

- **REST**: `Accept: application/json+a2ui` → v0.8; `;version=0.9` →
  v0.9. (FR-A-3.)
- **A2A**: `Message.metadata.a2ui_version: "v0.9"`.
- **MCP**: the version bound to the negotiated MCP App.
- **Chat channels**: the surface mapper projects to a
  `PlatformMessage` per the adapter's `degrade` rules (→
  `references/05`).

The **v0.9 A2UI envelope** Triton builds from your `surface` is:

```json
{ "version": "0.9", "stream": [
  { "type": "text", "text": "…" },
  { "type": "button", "label": "…", "action": { "tool": "…", "args": {…} } }
] }
```

Note the wire form flattens and renames (`kind`→`type`,
`value`→`text`, inline `action`). **Do not emit this yourself** —
emit the canonical `{surface:{components:[…]}}` and let the builder
flatten it. The two differ on purpose (`crates/triton-core/src/a2ui/v09.rs`).

### Where the envelope lives in the transport — read this

A caller never sees that envelope at the top level. It is always
**nested under a `result` key** inside the dispatcher envelope
`{ latency_ms, trace_id, result }`, and each protocol wraps it once
more:

| Protocol | Path to the `{version, stream}` envelope | Trace id |
|---|---|---|
| REST `POST /v1/tools/<tool>` | `body.result` | `body.trace_id` |
| MCP `tools/call` | `result.structuredContent.result` | `result._meta.trace_id` |
| A2A `POST /message:send` | `parts[0].data.result` | `metadata.trace_id` |

So a REST `;version=0.9` response is:

```json
{ "latency_ms": 7, "trace_id": "…",
  "result": { "version": "0.9", "stream": [ … ] } }
```

A client that reads top-level `version`/`stream` finds neither and
mis-renders (this exact bug shipped once in the Flutter explorer —
`apps/explorer/lib/widgets/a2ui/a2ui_renderer.dart`). **Unwrap `result`
first.** Because `version` rides *inside* `result`, you can dispatch on
`result.version` rather than tracking the `Accept` you sent. v0.8 is
the same shape, but each stream entry is a PascalCase
`{ "Component": { "Text": {…} } }` wrapper instead of a flat `type`.

## Which version to target

Default to **v0.9** unless an existing MCP App or A2A peer in your
deployment pins v0.8. Since you return the version-agnostic `surface`,
this choice is really the *caller's* via content negotiation — your
agent stays out of it. The only place version matters to you is your
integration test, where you assert against whichever envelope your
caller negotiates (→ `references/08`).

## Parity caveat for tests

A2UI parity tests compare **parsed dicts, not raw bytes** — JSON key
order is not stable across serde paths. If you assert on a Triton
response in your own test, parse to `serde_json::Value` and compare
structurally. (`doc/realizations.md` §1; FR-A-4.)
