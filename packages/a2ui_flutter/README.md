# a2ui_flutter

Reusable Flutter rendering for the A2UI wire contract — the shared core
behind Triton's Explorer and embedding hosts.

Extracted from `apps/explorer/lib/widgets/a2ui/` per triton#202 /
[heron#9](https://github.com/DataZooDE/heron/issues/9), following the shape
escurel uses for `packages/escurel_explorer_kit`: a `publish_to: none`
package in `packages/`, consumed by path, analysed and tested in its own
directory before its first consumer builds.

## Two layers, deliberately separable

**The contract — `A2uiEnvelope`.** How the wire nests and versions a
surface: `result` unwrapping, then version precedence (caller > envelope
field > shape sniff). This is the part of A2UI every host has to get right
and none of them can see is wrong — a host that reads top-level
`version`/`stream` gets a plausible empty surface rather than an error, which
is how that bug shipped once already. Depend on this alone if you render
A2UI with your own design system.

**The default renderer — `A2UIRenderer`.** A Material presentation of the
vocabulary, with `A2UIv08Renderer` / `A2UIv09Renderer` beneath it. The two
version trees stay isolated per ADR-4; the envelope resolves *which* version
is in play and hands the stream over untouched. There is deliberately **no**
normalised cross-version component model — that would be exactly the shared
base ADR-4 refuses.

## Rendering a kind the vocabulary does not define

`componentBuilder` is consulted before every built-in rule, in both version
trees. Return null to decline and fall through.

```dart
A2UIRenderer(
  envelope: envelope,
  componentBuilder: (context, node) =>
      node['type'] == 'diff' ? MyDiff(lines: node['lines']) : null,
)
```

The node arrives whole, in its version's own shape, because a host adding a
kind needs fields this package has never heard of.

## What stayed in the Explorer, and why

`ui_resource_view.dart` and the `html_embed_*` pair. They need an MCP client
and a web platform-view embedder, and the other hosts do not want this
implementation: Heron is not a web app and embeds through
`webview_flutter`; Peacock's runtime *is* the iframe. Extracting them would
have produced a seam with one implementation.

## Consuming it

In this repo:

```yaml
dependencies:
  a2ui_flutter:
    path: ../../packages/a2ui_flutter
```

From another repo, use a sibling checkout pinned to a commit in CI — the
shape `datazoo-ai-engineering` uses against escurel:

```yaml
# consumer pubspec.yaml
a2ui_flutter:
  path: ../../triton/packages/a2ui_flutter
```

```yaml
# consumer CI
env:
  TRITON_REF: <sha>  # pin: the triton this suite was developed against
```

## Tests

```bash
flutter analyze
flutter test
```
