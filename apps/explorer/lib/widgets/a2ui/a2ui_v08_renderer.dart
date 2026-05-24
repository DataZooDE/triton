import 'package:flutter/material.dart';

/// A2UI v0.8 renderer. **No shared base** with v0.9, per ADR-4 — this
/// file owns the full v0.8 envelope shape so the schema can evolve
/// without rippling. The envelope looks like:
///
/// ```json
/// {
///   "version": "0.8",
///   "stream": [
///     { "Component": { "Text": { "text": "..." } } },
///     { "Component": { "Button": { "label": "...", "action": {...} } } },
///     { "Component": { "Narration": { "text": "..." } } }
///   ]
/// }
/// ```
///
/// Unknown component kinds are rendered as a yellow debug card so the
/// operator sees what's missing instead of a silent skip.
class A2UIv08Renderer extends StatelessWidget {
  const A2UIv08Renderer({
    super.key,
    required this.envelope,
    this.onAction,
  });

  final Map<String, dynamic> envelope;
  final void Function(String tool, Map<String, dynamic> args)? onAction;

  @override
  Widget build(BuildContext context) {
    final stream = (envelope['stream'] as List?) ?? const [];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final raw in stream) _node(context, raw),
      ],
    );
  }

  Widget _node(BuildContext context, dynamic raw) {
    if (raw is! Map) return _unknown('not an object');
    final component = (raw['Component'] as Map?)?.cast<String, dynamic>();
    if (component == null) return _unknown('missing Component wrapper');
    if (component.length != 1) {
      return _unknown('expected one key in Component, got ${component.keys}');
    }
    final entry = component.entries.single;
    final kind = entry.key;
    final body = (entry.value as Map?)?.cast<String, dynamic>() ?? const {};
    switch (kind) {
      case 'Text':
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          child: Text((body['text'] as String?) ?? ''),
        );
      case 'Narration':
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          child: Text(
            (body['text'] as String?) ?? '',
            style: const TextStyle(fontStyle: FontStyle.italic),
          ),
        );
      case 'Button':
        final label = (body['label'] as String?) ?? '';
        final action = (body['action'] as Map?)?.cast<String, dynamic>();
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 6),
          child: Align(
            alignment: Alignment.centerLeft,
            child: FilledButton.tonal(
              onPressed: onAction == null || action == null
                  ? null
                  : () => onAction!(
                        action['tool'] as String,
                        ((action['args'] as Map?)?.cast<String, dynamic>()) ??
                            const {},
                      ),
              child: Text(label),
            ),
          ),
        );
      default:
        return _unknown('unknown v0.8 component kind: $kind');
    }
  }

  Widget _unknown(String message) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 4),
        child: Card(
          color: Colors.amber.shade100,
          child: Padding(
            padding: const EdgeInsets.all(8),
            child: Text(message),
          ),
        ),
      );
}
