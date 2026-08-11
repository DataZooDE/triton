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
