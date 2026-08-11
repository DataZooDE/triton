/// The ONE place that picks an `embedHtml` implementation for the running
/// target. Everything that embeds a `ui://` resource imports this file, so a
/// test exercises exactly the arm a shipped build would.
///
/// Three arms, because a Flutter host is one of three different things:
///
///   * web — a real DOM, so the resource goes straight into a sandboxed
///     `<iframe>` (`html_embed_web.dart`);
///   * `dart:io` — Android and iOS get a webview hosting that same iframe,
///     while desktop and the Dart VM `flutter test` runs on fall back to the
///     resource's source. That last split has to happen at RUNTIME: the
///     conditional import cannot tell Android from Linux, and
///     `webview_flutter` has no desktop implementation
///     (`html_embed_webview.dart`);
///   * neither — the stub, so the import always resolves.
library;

export 'html_embed_stub.dart'
    if (dart.library.html) 'html_embed_web.dart'
    if (dart.library.io) 'html_embed_webview.dart';
