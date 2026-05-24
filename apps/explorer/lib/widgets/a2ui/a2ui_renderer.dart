import 'package:flutter/material.dart';

import 'a2ui_v08_renderer.dart';
import 'a2ui_v09_renderer.dart';

/// Dispatches to the appropriate renderer based on the envelope's
/// `version` field. Two independent trees underneath — see ADR-4 in
/// `doc/architecture.md`.
class A2UIRenderer extends StatelessWidget {
  const A2UIRenderer({
    super.key,
    required this.envelope,
    this.onAction,
  });

  final Map<String, dynamic> envelope;
  final void Function(String tool, Map<String, dynamic> args)? onAction;

  @override
  Widget build(BuildContext context) {
    final version = envelope['version'] as String?;
    switch (version) {
      case '0.8':
        return A2UIv08Renderer(envelope: envelope, onAction: onAction);
      case '0.9':
        return A2UIv09Renderer(envelope: envelope, onAction: onAction);
      default:
        return Card(
          color: Colors.amber.shade100,
          child: Padding(
            padding: const EdgeInsets.all(12),
            child: Text(
              'Unknown A2UI version: ${version ?? 'missing'}.\n'
              'Add a renderer for it under widgets/a2ui/.',
            ),
          ),
        );
    }
  }
}
