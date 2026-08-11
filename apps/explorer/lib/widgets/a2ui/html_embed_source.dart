import 'package:flutter/material.dart';

/// Last-resort rendering of a `ui://` resource: its HTML **source**, as text.
///
/// This is not an embedder — it is what a target gets when it has nowhere to
/// put a live document. Two callers, for two different reasons:
///
///   * `html_embed_stub.dart` — the Dart VM `flutter test` runs on, where
///     there is neither a DOM nor a webview platform implementation;
///   * `html_embed_webview.dart` — Linux/Windows/macOS, where
///     `webview_flutter` ships no implementation at all (#201 defers desktop
///     explicitly rather than crashing on a missing platform instance).
///
/// It says so on the tin, because a user who sees markup should be able to
/// tell "this target can't host it" from "the report is broken".
Widget embedHtmlSource(String html, {double height = 600}) {
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
