// The web-only iframe host. `dart:html` is deprecated in favour of
// package:web, but the Explorer doesn't depend on package:web and this is the
// single, conditionally-imported web file — the classic API keeps it
// dependency-free. Loaded only on web (see ui_resource_view.dart).
// ignore_for_file: deprecated_member_use, avoid_web_libraries_in_flutter
import 'dart:html' as html;
import 'dart:js_interop';
import 'dart:js_interop_unsafe';
import 'dart:ui_web' as ui_web;

import 'package:flutter/material.dart';

/// View ids whose factory has already been registered. `registerViewFactory`
/// throws if the same id is registered twice, so callers pass a fresh id
/// whenever the HTML changes (see `UiResourceView`).
final Set<String> _registered = <String>{};

class _McpRequest {
  _McpRequest(this.reqId, this.name, this.args);
  final String? reqId;
  final String name;
  final Object? args;
}

/// Parse an `mcp:callServerTool` message. `event.data` may arrive either as a
/// Dart `Map` (dart:html deserialises some structured clones) or as a raw JS
/// object — handle both. Returns null for anything else.
_McpRequest? _readMcpRequest(Object raw) {
  if (raw is Map) {
    if (raw['type'] != 'mcp:callServerTool') return null;
    return _McpRequest(
      raw['reqId']?.toString(),
      raw['name']?.toString() ?? '',
      raw['arguments'],
    );
  }
  try {
    final obj = raw as JSObject;
    final type = (obj.getProperty('type'.toJS) as JSString?)?.toDart;
    if (type != 'mcp:callServerTool') return null;
    return _McpRequest(
      (obj.getProperty('reqId'.toJS) as JSString?)?.toDart,
      (obj.getProperty('name'.toJS) as JSString?)?.toDart ?? '',
      obj.getProperty('arguments'.toJS).dartify(),
    );
  } catch (_) {
    return null;
  }
}

/// Parse an `mcp:prompt` message (`{type:'mcp:prompt', text}`): the embedded
/// runtime hands the host a prepared prompt to send as a NEW USER TURN in the
/// chat (a document's skill-page `prompt` action). Same Map-or-JSObject
/// duality as [_readMcpRequest]. Returns the text, or null for anything else.
String? _readMcpPrompt(Object raw) {
  if (raw is Map) {
    if (raw['type'] != 'mcp:prompt') return null;
    return raw['text']?.toString();
  }
  try {
    final obj = raw as JSObject;
    final type = (obj.getProperty('type'.toJS) as JSString?)?.toDart;
    if (type != 'mcp:prompt') return null;
    return (obj.getProperty('text'.toJS) as JSString?)?.toDart;
  } catch (_) {
    return null;
  }
}

/// Embed self-contained HTML in a **sandboxed** `<iframe>` on Flutter web.
/// The iframe is fed via `srcdoc`, so it loads no external URL; the `sandbox`
/// attribute isolates the embedded upstream report from the Explorer, while
/// still allowing its own scripts (CanvasKit / Vega runtime) to execute. This
/// is the MCP-Apps render path: Triton proxies `resources/read` of a
/// `ui://<authority>/…` resource to the owning upstream, and we host the
/// returned single-file runtime here.
///
/// [onCallServerTool] wires the MCP-Apps **host bridge**: the embedded runtime
/// fetches its data (and re-renders on drill) by posting
/// `{type:'mcp:callServerTool', reqId, name, arguments}` to its parent; we
/// fulfil it (dispatching the tool through Triton) and post the result back as
/// `{type:'mcp:callServerTool:result', reqId, result}`. `mcp:updateModelContext`
/// is accepted and ignored (a host could relay it to its model).
Widget embedHtml(
  String htmlStr, {
  required String viewId,
  double height = 600,
  Future<Object?> Function(String name, Object? args)? onCallServerTool,
  void Function(String text)? onPrompt,
}) {
  if (!_registered.contains(viewId)) {
    ui_web.platformViewRegistry.registerViewFactory(viewId, (int _) {
      final el = html.IFrameElement()
        ..srcdoc = htmlStr
        ..style.border = 'none'
        ..style.width = '100%'
        ..style.height = '100%'
        ..setAttribute('sandbox', 'allow-scripts');
      if (onCallServerTool != null || onPrompt != null) {
        html.window.onMessage.listen((event) async {
          final raw = event.data;
          if (raw == null) return;
          // `mcp:prompt` → a new user turn. Several embeds may be live at
          // once (auto-opened report + clicked sources); only THIS iframe's
          // messages may fire, or one click sends N prompts.
          if (onPrompt != null && event.source == el.contentWindow) {
            final prompt = _readMcpPrompt(raw);
            if (prompt != null) {
              if (prompt.trim().isNotEmpty) onPrompt(prompt.trim());
              return;
            }
          }
          if (onCallServerTool == null) return;
          final reading = _readMcpRequest(raw);
          if (reading == null) return; // not a callServerTool message
          Object? result;
          try {
            result = await onCallServerTool(reading.name, reading.args);
          } catch (e) {
            result = {
              'error': {'message': e.toString()},
            };
          }
          el.contentWindow?.postMessage({
            'type': 'mcp:callServerTool:result',
            'reqId': reading.reqId,
            'result': result,
          }, '*');
        });
      }
      return el;
    });
    _registered.add(viewId);
  }
  return SizedBox(height: height, child: HtmlElementView(viewType: viewId));
}
