import 'dart:convert';

import 'package:flutter/material.dart';

/// Lightweight JSON pretty-printer. No syntax highlighting yet —
/// `flutter_highlight` adds noticeable bundle weight; defer until a
/// page actually needs colorized JSON.
class JsonViewer extends StatelessWidget {
  const JsonViewer(this.value, {super.key});

  final Object? value;

  @override
  Widget build(BuildContext context) {
    final encoder = const JsonEncoder.withIndent('  ');
    final text = () {
      try {
        return encoder.convert(value);
      } catch (_) {
        return value.toString();
      }
    }();
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: Theme.of(context).colorScheme.surfaceContainerHigh,
        borderRadius: BorderRadius.circular(8),
      ),
      child: SelectableText(
        text,
        style: const TextStyle(
          fontFamily: 'monospace',
          fontSize: 13,
          height: 1.4,
        ),
      ),
    );
  }
}
