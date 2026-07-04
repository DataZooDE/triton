import 'package:flutter/material.dart';

/// Non-web fallback for [embedHtml]. Flutter widget tests run on the Dart
/// VM, where there is no DOM to host an `<iframe>`; show the resource's HTML
/// source instead. The web build ([html_embed_web.dart]) mounts the live,
/// sandboxed iframe.
Widget embedHtml(
  String html, {
  required String viewId,
  double height = 600,
  Future<Object?> Function(String name, Object? args)? onCallServerTool,
  void Function(String text)? onPrompt,
}) {
  return SizedBox(
    height: height,
    child: Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Text('Embedded UI resource (rendered as an iframe on web)'),
            const SizedBox(height: 8),
            Expanded(
              child: SingleChildScrollView(
                child: SelectableText(
                  html,
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 11),
                ),
              ),
            ),
          ],
        ),
      ),
    ),
  );
}
