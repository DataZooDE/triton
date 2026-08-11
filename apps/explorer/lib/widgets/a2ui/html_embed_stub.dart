import 'package:flutter/material.dart';

import 'html_embed_source.dart';

/// The arm selected when the target has neither `dart:html` nor `dart:io`.
/// In practice nothing reaches it — web takes [html_embed_web.dart] and every
/// native target takes [html_embed_webview.dart] — but a conditional import
/// needs a default that always resolves, and it must not pretend to embed
/// anything. Shows the resource's source.
Widget embedHtml(
  String html, {
  required String viewId,
  double height = 600,
  Future<Object?> Function(String name, Object? args)? onCallServerTool,
  void Function(String text)? onPrompt,
}) =>
    embedHtmlSource(html, height: height);
