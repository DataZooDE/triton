import 'package:flutter/material.dart';

/// A2UI v0.9 renderer. **No shared base** with v0.8 per ADR-4. The
/// envelope uses lowercase `type`, no `Component` wrapper, action
/// data inlined:
///
/// ```json
/// {
///   "version": "0.9",
///   "stream": [
///     { "type": "text", "text": "..." },
///     { "type": "narration", "text": "..." },
///     { "type": "button", "label": "...", "action": { "tool": "...", "args": {...} } }
///   ]
/// }
/// ```
class A2UIv09Renderer extends StatelessWidget {
  const A2UIv09Renderer({
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
    final map = raw.cast<String, dynamic>();
    final type = map['type'] as String?;
    switch (type) {
      case 'text':
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          child: Text((map['text'] as String?) ?? ''),
        );
      case 'narration':
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          child: Text(
            (map['text'] as String?) ?? '',
            style: const TextStyle(fontStyle: FontStyle.italic),
          ),
        );
      case 'button':
        final label = (map['label'] as String?) ?? '';
        final action = (map['action'] as Map?)?.cast<String, dynamic>();
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 6),
          child: Align(
            alignment: Alignment.centerLeft,
            child: FilledButton(
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
        return _unknown('unknown v0.9 type: $type');
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
