// The non-web embedder for `ui://` resources (#201).
//
// Until this file existed, the conditional import in `html_embed.dart` had two
// arms — web, and "everything else" — and everything else meant the stub, so
// on Android and iOS an MCP-Apps resource rendered as a monospace dump of its
// own HTML source.
//
// THE DESIGN DECISION THAT MATTERS
//
// The obvious implementation — `controller.loadHtmlString(html)` — is wrong,
// and it fails in a way that reads as "the bridge is broken" rather than "the
// embedder is misdesigned". The MCP-Apps contract has the guest talk to its
// HOST across a frame boundary:
//
//     guest → host : parent.postMessage({type:'mcp:callServerTool', reqId, name, arguments})
//     host  → guest: contentWindow.postMessage({type:'mcp:callServerTool:result', reqId, result})
//     guest → host : parent.postMessage({type:'mcp:prompt', text})
//
// Load the guest at the webview's top level and `window.parent === window`, so
// every message the guest posts to its parent comes straight back to itself
// and the host never sees it. `window.parent` is not assignable, so it cannot
// be shimmed away.
//
// So we keep a REAL frame boundary: the webview loads a tiny host document
// that holds the guest in a sandboxed `<iframe srcdoc=…>` — the same element,
// the same `sandbox="allow-scripts"`, the same srcdoc feed as
// `html_embed_web.dart`. The host document relays messages to Flutter over a
// JavaScript channel and posts replies back into `iframe.contentWindow`.
//
// The guest is therefore byte-identical across web and mobile: no upstream (a
// Peacock report, an Escurel document) needs to know which host it landed in.
//
// Verified on a real Android device by
// `integration_test/ui_resource_embed_test.dart`.

import 'dart:async';
import 'dart:convert';
import 'dart:io' show Platform;

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:webview_flutter/webview_flutter.dart';

import 'html_embed_source.dart';

/// Name of the JavaScript channel the host document posts through.
const String _channel = 'McpHost';

/// Whether `webview_flutter` has a platform implementation on
/// [operatingSystem] (values as [Platform.operatingSystem] reports them).
///
/// This has to be a RUNTIME decision: the conditional import can only ask
/// "does this target have `dart:io`?", which is true of Linux, Windows and
/// macOS — and of the Dart VM that `flutter test` runs on — as much as it is
/// of Android and iOS. `webview_flutter` ships only `webview_flutter_android`
/// and `webview_flutter_wkwebview`, so constructing a [WebViewController]
/// anywhere else throws on a missing platform instance. #201 defers desktop
/// explicitly; this predicate is where that deferral lives.
@visibleForTesting
bool webViewAvailableOn(String operatingSystem) =>
    operatingSystem == 'android' || operatingSystem == 'ios';

/// The host document. `__HTML__` is replaced with the guest HTML encoded as a
/// JS string literal — see [hostDocumentFor].
const String _hostDocumentTemplate = r'''
<!doctype html>
<meta charset="utf-8">
<style>
  html, body { margin:0; padding:0; height:100%; background:transparent; }
  iframe { border:none; width:100%; height:100%; display:block; }
</style>
<body>
<iframe id="guest" sandbox="allow-scripts"></iframe>
<script>
  var guest = document.getElementById('guest');
  guest.srcdoc = __HTML__;

  // guest → host. Forward only the message types the bridge defines; anything
  // else is ignored exactly as the web embedder ignores it.
  window.addEventListener('message', function (e) {
    var d = e.data;
    if (!d || typeof d !== 'object') return;
    if (d.type === 'mcp:callServerTool' ||
        d.type === 'mcp:prompt' ||
        d.type === 'mcp:updateModelContext') {
      McpHost.postMessage(JSON.stringify(d));
    }
  });

  // host → guest. Called from Dart via runJavaScript.
  window.__mcpDeliver = function (payloadJson) {
    guest.contentWindow.postMessage(JSON.parse(payloadJson), '*');
  };
</script>
</body>
''';

/// Build the host document that carries [html] as its guest.
///
/// Encoding the guest needs more than [jsonEncode], and the failure is nearly
/// silent. A `ui://` resource is a complete HTML document, so it contains
/// `</script>` — and the HTML parser terminates the HOST's script block at
/// that byte sequence no matter that it sits inside a JS string literal. The
/// host script then dies with `Uncaught SyntaxError`, no bridge is ever
/// installed, and the embed sits there blank with nothing logged anywhere.
///
/// Escaping `</` as `<\/` is inert in JS (`\/` is `/`) and stops the parser
/// seeing a closing tag. `<!--` gets the same treatment: it opens an HTML
/// comment state in legacy script parsing.
///
/// Every real report hits this; it is not an edge case.
@visibleForTesting
String hostDocumentFor(String html) => _hostDocumentTemplate.replaceFirst(
      '__HTML__',
      jsonEncode(html).replaceAll('</', r'<\/').replaceAll('<!--', r'<\!--'),
    );

/// Embed a self-contained `ui://` resource on a non-web target. Signature
/// matches `html_embed_stub.dart` / `html_embed_web.dart`; see
/// `html_embed.dart` for how the three are selected.
Widget embedHtml(
  String html, {
  required String viewId,
  double height = 600,
  Future<Object?> Function(String name, Object? args)? onCallServerTool,
  void Function(String text)? onPrompt,
}) {
  if (!webViewAvailableOn(Platform.operatingSystem)) {
    return embedHtmlSource(html, height: height);
  }
  return SizedBox(
    height: height,
    child: _WebViewEmbed(
      // A fresh id means a fresh resource: rebuild the controller rather than
      // leaving the previous document in place, mirroring how the web
      // embedder registers a new view factory per id.
      key: ValueKey(viewId),
      html: html,
      onCallServerTool: onCallServerTool,
      onPrompt: onPrompt,
    ),
  );
}

class _WebViewEmbed extends StatefulWidget {
  const _WebViewEmbed({
    super.key,
    required this.html,
    this.onCallServerTool,
    this.onPrompt,
  });

  final String html;
  final Future<Object?> Function(String name, Object? args)? onCallServerTool;
  final void Function(String text)? onPrompt;

  @override
  State<_WebViewEmbed> createState() => _WebViewEmbedState();
}

class _WebViewEmbedState extends State<_WebViewEmbed> {
  late final WebViewController _controller;

  @override
  void initState() {
    super.initState();
    _controller = WebViewController()
      ..setJavaScriptMode(JavaScriptMode.unrestricted)
      ..setBackgroundColor(const Color(0x00000000))
      ..addJavaScriptChannel(_channel, onMessageReceived: _onGuestMessage)
      // Worth keeping: a resource whose script fails to parse renders as a
      // silent blank box, and this is the only place that says why.
      ..setOnConsoleMessage((m) {
        if (kDebugMode) debugPrint('ui:// console: ${m.message}');
      })
      ..loadHtmlString(hostDocumentFor(widget.html));
  }

  void _onGuestMessage(JavaScriptMessage message) {
    final Object? decoded = jsonDecode(message.message);
    if (decoded is! Map) return;
    switch (decoded['type']) {
      case 'mcp:callServerTool':
        unawaited(_fulfil(decoded));
      case 'mcp:prompt':
        final text = decoded['text']?.toString().trim();
        if (text != null && text.isNotEmpty) widget.onPrompt?.call(text);
      // 'mcp:updateModelContext' is accepted and ignored, matching the web
      // embedder — a host could relay it to its model.
    }
  }

  Future<void> _fulfil(Map<Object?, Object?> request) async {
    final handler = widget.onCallServerTool;
    if (handler == null) return;

    Object? result;
    try {
      result = await handler(
        request['name']?.toString() ?? '',
        request['arguments'],
      );
    } catch (e) {
      // Mirror the web embedder: a failed tool call comes back as a result
      // carrying an error, not as a dropped message. A guest awaiting `reqId`
      // would otherwise hang forever.
      result = {
        'error': {'message': e.toString()},
      };
    }
    if (!mounted) return;

    final payload = jsonEncode({
      'type': 'mcp:callServerTool:result',
      'reqId': request['reqId'],
      'result': result,
    });
    // Encode once more so the JSON survives as a JS string argument.
    await _controller.runJavaScript(
      'window.__mcpDeliver(${jsonEncode(payload)})',
    );
  }

  @override
  Widget build(BuildContext context) => WebViewWidget(controller: _controller);
}
