// The web-only iframe host. `dart:html` is deprecated in favour of
// package:web, but the Explorer doesn't depend on package:web and this is the
// single, conditionally-imported web file — the classic API keeps it
// dependency-free. Loaded only on web (see ui_resource_view.dart).
// ignore_for_file: deprecated_member_use, avoid_web_libraries_in_flutter
import 'dart:html' as html;
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
Widget embedHtml(String htmlStr, {required String viewId, double height = 600}) {
  if (!_registered.contains(viewId)) {
    ui_web.platformViewRegistry.registerViewFactory(viewId, (int _) {
      final el = html.IFrameElement()
        ..srcdoc = htmlStr
        ..style.border = 'none'
        ..style.width = '100%'
        ..style.height = '100%'
        ..setAttribute('sandbox', 'allow-scripts');
      return el;
    });
    _registered.add(viewId);
  }
  return SizedBox(height: height, child: HtmlElementView(viewType: viewId));
}
