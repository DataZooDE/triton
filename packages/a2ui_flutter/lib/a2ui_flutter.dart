/// Reusable Flutter rendering for the A2UI wire contract.
///
/// Two layers, deliberately separable:
///
///  * **the contract** — [A2uiEnvelope], which knows how the wire nests and
///    versions a surface. A host that renders A2UI with its own design
///    system depends on this and nothing else.
///  * **the default renderer** — [A2UIRenderer] and the version trees beneath
///    it, a Material presentation of the vocabulary. A host that has no
///    opinion about the look depends on this too, and reaches for
///    [A2UIRenderer.componentBuilder] when it has an opinion about one node.
library;

export 'src/a2ui_renderer.dart';
export 'src/a2ui_v08_renderer.dart';
export 'src/a2ui_v09_renderer.dart';
export 'src/component_builder.dart';
export 'src/envelope.dart';
export 'src/markdown_lite.dart';
export 'src/sources_row.dart';
