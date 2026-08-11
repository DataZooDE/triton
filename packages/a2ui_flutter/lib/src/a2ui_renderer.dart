import 'package:flutter/material.dart';

import 'a2ui_v08_renderer.dart';
import 'a2ui_v09_renderer.dart';
import 'component_builder.dart';
import 'envelope.dart';

/// Dispatches to the appropriate renderer. Two independent trees
/// underneath — see ADR-4 in `doc/architecture.md`.
///
/// Unwrapping `result` and resolving the version live in [A2uiEnvelope], not
/// here, so a host that renders the same wire with its own design system
/// shares those rules instead of reimplementing them. This widget is the
/// Material presentation sitting on top of them.
class A2UIRenderer extends StatelessWidget {
  const A2UIRenderer({
    super.key,
    required this.envelope,
    this.version,
    this.onAction,
    this.onOpenResource,
    this.componentBuilder,
  });

  final Map<String, dynamic> envelope;

  /// The A2UI version the caller negotiated (e.g. '0.8' / '0.9').
  /// Authoritative when set, because the wire envelope omits it.
  final String? version;

  final void Function(String tool, Map<String, dynamic> args)? onAction;

  /// A `sources` item was clicked: open its `ui://` resource inline.
  /// Sources never auto-open — this fires only on an explicit tap.
  final void Function(String uri)? onOpenResource;

  /// A host's per-node override. See [A2uiComponentBuilder].
  final A2uiComponentBuilder? componentBuilder;

  @override
  Widget build(BuildContext context) {
    final parsed = A2uiEnvelope.parse(envelope, version: version);
    final inner = <String, dynamic>{'stream': parsed.stream};
    switch (parsed.version) {
      case '0.8':
        return A2UIv08Renderer(
            envelope: inner,
            onAction: onAction,
            onOpenResource: onOpenResource,
            componentBuilder: componentBuilder);
      case '0.9':
        return A2UIv09Renderer(
            envelope: inner,
            onAction: onAction,
            onOpenResource: onOpenResource,
            componentBuilder: componentBuilder);
      default:
        return Card(
          color: Colors.amber.shade100,
          child: Padding(
            padding: const EdgeInsets.all(12),
            child: Text(
              'Unknown A2UI version: ${parsed.version ?? 'missing'}.\n'
              'Add a renderer for it to package:a2ui_flutter.',
            ),
          ),
        );
    }
  }
}
