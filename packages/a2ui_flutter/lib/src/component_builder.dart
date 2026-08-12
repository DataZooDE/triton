import 'package:flutter/widgets.dart';

/// A host's chance to render one stream node itself.
///
/// Consulted **before** every built-in rule — before the version tree's kind
/// switch, before consecutive buttons collapse into a row, before an inline
/// report next to a resource button is suppressed. The host's opinion wins
/// outright, because a hook that a placement rule can override is a hook a
/// host cannot reason about.
///
/// Return null to decline, which falls straight through to the built-in
/// behaviour. That is what makes the seam per-node: a host with an opinion
/// about one kind does not thereby take responsibility for the rest of the
/// vocabulary.
///
/// [node] is the raw stream entry, in the shape its version uses — flat with
/// a lowercase `type` in v0.9, wrapped in a PascalCase `Component` in v0.8.
/// It is handed over whole rather than pre-digested, because a host adding a
/// kind the vocabulary does not define needs fields this package has never
/// heard of.
typedef A2uiComponentBuilder = Widget? Function(
  BuildContext context,
  Map<String, dynamic> node,
);

/// What to draw for a stream node no renderer in this package recognises.
///
/// [A2uiComponentBuilder] cannot express this. Returning null from it means
/// *decline*, which falls through to the built-in amber debug card — so a host
/// has no way to say "render nothing, deliberately". That default is right for
/// an operator console (the Explorer: an unmapped kind is exactly what you want
/// to see) and wrong on an end-user's phone, where a debug card is noise about
/// a problem the reader cannot act on.
///
/// A builder rather than a `strict`/`lenient` flag: a flag names two points on
/// a line hosts want a third point on — hide it, show the card, or draw a
/// host-shaped "this needs an app update" of your own. The return type is
/// non-nullable precisely because null is what the other hook already means;
/// a host that wants nothing returns `SizedBox.shrink()`, and says so.
///
/// Consulted only *after* [A2uiComponentBuilder] has declined and the version's
/// own switch has found no rule. Leaving it null keeps the debug card.
typedef A2uiUnknownComponentBuilder = Widget Function(
  BuildContext context,
  Map<String, dynamic> node,
);

/// A `button` that carries a `resource` was tapped.
///
/// A sibling of `onAction` rather than a widening of it. `onAction` is
/// `(tool, args)`, and a v0.9 button may name an MCP-App resource
/// (`ui://…`) *alongside* its tool call — the report-as-surface pattern. The
/// three ways to reach it are not equivalent:
///
///  * widening `onAction` to a third parameter would break every existing
///    two-argument host at compile time, and this package is already consumed;
///  * routing it through `onOpenResource` (the `sources` chip seam) would drop
///    the tool call, and a resource button is an *action* that also names a
///    view — not a link;
///  * so: a separate optional callback carrying all three. A host that has not
///    heard of it keeps today's `onAction` dispatch exactly, which is what
///    makes this additive.
typedef A2uiResourceActionCallback = void Function(
  String tool,
  Map<String, dynamic> args,
  String resource,
);
