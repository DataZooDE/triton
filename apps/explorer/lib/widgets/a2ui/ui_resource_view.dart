import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../providers/api_provider.dart';
import 'html_embed_stub.dart' if (dart.library.html) 'html_embed_web.dart';

/// Fetches an MCP-Apps `ui://` resource via Triton's `resources/read`
/// (Triton proxies it to the owning upstream — e.g. a Peacock report — per
/// #143) and embeds the returned self-contained HTML in a sandboxed iframe.
///
/// This is what turns the Explorer into a generic surface for *interactive*
/// upstreams: any tool result that carries `_meta.ui.resourceUri` can be
/// rendered inline, with no knowledge of the upstream's component vocabulary.
/// On non-web targets it degrades to the HTML source (no DOM to host).
class UiResourceView extends ConsumerStatefulWidget {
  const UiResourceView({
    super.key,
    required this.uri,
    this.height = 600,
    this.onPrompt,
  });

  /// The `ui://<authority>/…` resource to read and embed.
  final String uri;
  final double height;

  /// The embedded runtime posted `{type:'mcp:prompt', text}` — a document
  /// action asking the host to send `text` as a NEW USER TURN in the chat.
  final void Function(String text)? onPrompt;

  @override
  ConsumerState<UiResourceView> createState() => _UiResourceViewState();
}

class _UiResourceViewState extends ConsumerState<UiResourceView> {
  /// Monotonic so each (re)load gets a fresh platform-view id — the web
  /// embedder keys its iframe factory by id and can't re-register one.
  static int _seq = 0;

  bool _loading = false;
  String? _error;
  String? _html;
  String _viewId = 'uiResource-0';

  @override
  void initState() {
    super.initState();
    _load();
  }

  @override
  void didUpdateWidget(UiResourceView old) {
    super.didUpdateWidget(old);
    if (old.uri != widget.uri) _load();
  }

  Future<void> _load() async {
    setState(() {
      _loading = true;
      _error = null;
    });
    try {
      final res = await ref.read(mcpClientProvider).readResource(widget.uri);
      if (!mounted) return;
      setState(() {
        _html = res.text;
        _viewId = 'uiResource-${_seq++}';
        _loading = false;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.toString();
        _loading = false;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) {
      return const Padding(
        padding: EdgeInsets.all(24),
        child: Center(child: CircularProgressIndicator()),
      );
    }
    if (_error != null) {
      return Card(
        color: Theme.of(context).colorScheme.errorContainer,
        child: Padding(
          padding: const EdgeInsets.all(12),
          child: Text('resources/read failed: $_error'),
        ),
      );
    }
    final h = _html;
    if (h == null || h.isEmpty) {
      return const Padding(
        padding: EdgeInsets.all(12),
        child: Text('No content returned for this resource.'),
      );
    }
    return embedHtml(
      h,
      viewId: _viewId,
      height: widget.height,
      onCallServerTool: _callServerTool,
      onPrompt: widget.onPrompt,
    );
  }

  /// The MCP-Apps host bridge: the embedded runtime asks us to call a tool
  /// (e.g. `render_report` to fetch its rows / re-render on drill); dispatch it
  /// through Triton over MCP and hand back the result the runtime renders.
  Future<Object?> _callServerTool(String name, Object? args) async {
    final argsMap =
        (args is Map) ? args.cast<String, dynamic>() : <String, dynamic>{};
    final r = await ref.read(mcpClientProvider).invoke(name, argsMap);
    // `raw` is the MCP `structuredContent`, under which Triton nests the
    // upstream's own MCP tool result (`{_meta, structuredContent:{rows…}}`).
    // Hand the runtime that inner result — it's the exact shape its standalone
    // HTTP path consumes (`data.structuredContent.rows`).
    final inner = r.raw['result'];
    return inner is Map ? inner : r.raw;
  }
}
