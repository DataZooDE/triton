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
}) {
  if (!_registered.contains(viewId)) {
    ui_web.platformViewRegistry.registerViewFactory(viewId, (int _) {
      final el = html.IFrameElement()
        ..srcdoc = htmlStr
        ..style.border = 'none'
        ..style.width = '100%'
        ..style.height = '100%'
        ..setAttribute('sandbox', 'allow-scripts');
      if (onCallServerTool != null) {
        // The posted value is a JS object (dart:html doesn't deep-convert it),
        // so read/write it through js_util.
        html.window.onMessage.listen((event) async {
          // Read the posted JS object via js_interop (dart:html hands it over
          // unconverted); reply with a plain Dart Map (dart:html serialises
          // that to a JS object natively — don't mix in a jsify'd value).
          String? reqId;
          String name;
          Object? args;
          try {
            final raw = event.data;
            if (raw == null) return;
            final obj = raw as JSObject;
            final type = (obj.getProperty('type'.toJS) as JSString?)?.toDart;
            if (type != 'mcp:callServerTool') return;
            reqId = (obj.getProperty('reqId'.toJS) as JSString?)?.toDart;
            name = (obj.getProperty('name'.toJS) as JSString?)?.toDart ?? '';
            args = obj.getProperty('arguments'.toJS).dartify();
          } catch (_) {
            return; // not an MCP-Apps message we recognise
          }
          Object? result;
          try {
            result = await onCallServerTool(name, args);
          } catch (e) {
            result = {
              'error': {'message': e.toString()},
            };
          }
          el.contentWindow?.postMessage({
            'type': 'mcp:callServerTool:result',
            'reqId': reqId,
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
