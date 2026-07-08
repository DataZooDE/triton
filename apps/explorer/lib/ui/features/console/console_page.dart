import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/models.dart';
import '../../../providers/api_provider.dart';
import '../../../widgets/a2ui/a2ui_renderer.dart';
import '../../../widgets/a2ui/ui_resource_view.dart';
import '../../../widgets/channel/channel_view.dart';
import '../../../widgets/json_viewer.dart';
import '../../../widgets/trace/trace_view.dart';

/// The Console — a **chat-first** entry point to the gateway. You talk to an
/// agent (or run any tool) in a transcript; the message box maps to the tool's
/// primary text argument and the answer renders as its A2UI surface, with any
/// `ui://` report embedded inline. The protocol / A2UI version / extra
/// arguments / raw frame live in a collapsible **Options** panel on the side —
/// the chat stays simple, the debugging affordances are one click away.
///
/// Defaults to **MCP** so MCP-Apps results (embedded reports) work out of the
/// box. Each agent turn has an **inspect** button that opens a per-message
/// trace sidebar — the tools it called and the Escurel data each touched,
/// plus the gateway timeline for its `trace_id`.
class ConsolePage extends ConsumerStatefulWidget {
  const ConsolePage({super.key});

  @override
  ConsumerState<ConsolePage> createState() => _ConsolePageState();
}

enum _Protocol {
  rest('REST'),
  mcp('MCP'),
  a2a('A2A');

  const _Protocol(this.label);
  final String label;
}

/// One entry in the transcript.
class _Turn {
  _Turn.user(this.text) : isUser = true, result = null, version = null;
  _Turn.agent(this.result, this.version) : isUser = false, text = null;

  final bool isUser;
  final String? text;
  final InvocationResult? result;
  final String? version;

  /// The `ui://` resource a Sources chip on this turn opened (click-to-open;
  /// nothing is set until the user taps a chip). Rendered below the surface,
  /// alongside — not replacing — the turn's auto-opened report.
  String? openedResource;

  /// Which surface this answer is being viewed as (see [kChatChannels]):
  /// `web` is the canonical A2UI render; a chat channel previews the same
  /// answer through that platform's mapper.
  String channel = kWebChannel;

  /// Cached `/v1/surface/render` payloads per channel, so re-selecting a
  /// channel is instant and never re-hits the endpoint.
  final Map<String, Map<String, dynamic>> renders = {};

  /// The channel whose render is currently in flight (spinner), if any.
  String? loadingChannel;
}

class _ConsolePageState extends ConsumerState<ConsolePage> {
  ToolDescriptor? _tool;
  bool _showDemoTools = false;

  // Options (the "variations on the side").
  _Protocol _protocol = _Protocol.mcp;
  String _a2uiVersion = '0.9';
  bool _showOptions = false;
  bool _streaming = false;

  /// Extra (non-primary) arguments, keyed by schema property name.
  final Map<String, dynamic> _extraArgs = {};
  final Map<String, TextEditingController> _extraCtrls = {};

  final _composer = TextEditingController();
  final _scroll = ScrollController();
  final List<_Turn> _turns = [];
  bool _sending = false;

  /// The agent turn whose trace sidebar is open (tap a bubble's inspect
  /// button); `null` when the sidebar is closed.
  _Turn? _inspect;

  @override
  void dispose() {
    _composer.dispose();
    _scroll.dispose();
    for (final c in _extraCtrls.values) {
      c.dispose();
    }
    super.dispose();
  }

  // ── tool + args ─────────────────────────────────────────────────────

  void _selectTool(ToolDescriptor t) {
    setState(() {
      _tool = t;
      _turns.clear();
      _extraArgs.clear();
      for (final c in _extraCtrls.values) {
        c.dispose();
      }
      _extraCtrls.clear();
      for (final name in _secondaryArgKeys(t)) {
        _extraCtrls[name] = TextEditingController();
      }
    });
  }

  /// The argument the chat box maps to: a string property, preferring the
  /// common chat keys; falls back to `question` when the schema is unknown
  /// (upstream agents register without one — see the introspection follow-up).
  String _primaryArgKey(ToolDescriptor t) {
    final props =
        (t.inputSchema['properties'] as Map?)?.cast<String, dynamic>() ?? {};
    for (final k in [
      'question',
      'message',
      'prompt',
      'text',
      'input',
      'query',
    ]) {
      final p = props[k];
      if (p is Map && (p['type'] == 'string' || p['type'] == null)) return k;
    }
    for (final e in props.entries) {
      if (e.value is Map && (e.value as Map)['type'] == 'string') return e.key;
    }
    return 'question';
  }

  /// Schema properties other than the primary one — rendered as form fields in
  /// the Options panel.
  List<String> _secondaryArgKeys(ToolDescriptor t) {
    final props =
        (t.inputSchema['properties'] as Map?)?.cast<String, dynamic>() ?? {};
    final primary = _primaryArgKey(t);
    return props.keys.where((k) => k != primary).toList(growable: false);
  }

  Map<String, dynamic> _argsFor(String message) {
    final t = _tool!;
    return {
      _primaryArgKey(t): message,
      for (final e in _extraArgs.entries)
        if (e.value != null && '${e.value}'.isNotEmpty) e.key: e.value,
    };
  }

  // ── send ────────────────────────────────────────────────────────────

  Future<InvocationResult> _dispatch(
    String tool,
    Map<String, dynamic> args,
    _Protocol p,
    String? a2ui,
  ) {
    switch (p) {
      case _Protocol.rest:
        return ref
            .read(restClientProvider)
            .invoke(tool, args, a2uiVersion: a2ui);
      case _Protocol.mcp:
        return ref
            .read(mcpClientProvider)
            .invoke(tool, args, a2uiVersion: a2ui);
      case _Protocol.a2a:
        return ref
            .read(a2aClientProvider)
            .invoke(tool, args, a2uiVersion: a2ui);
    }
  }

  Future<void> _send() async {
    final text = _composer.text.trim();
    if (text.isEmpty) return;
    _composer.clear();
    await _sendText(text);
  }

  /// Send `text` as a user turn — from the composer, or handed up by an
  /// embedded document's `prompt` action (`mcp:prompt`). Either way it is a
  /// real turn: user bubble, dispatch on the selected protocol, agent bubble.
  Future<void> _sendText(String text) async {
    final t = _tool;
    if (t == null || text.isEmpty || _sending) return;
    setState(() {
      _turns.add(_Turn.user(text));
      _sending = true;
    });
    _scrollToEnd();
    final r = await _dispatch(t.name, _argsFor(text), _protocol, _a2uiVersion);
    if (!mounted) return;
    setState(() {
      _turns.add(_Turn.agent(r, _a2uiVersion));
      _sending = false;
    });
    _scrollToEnd();
  }

  /// An embedded runtime (auto-opened report or a clicked source document)
  /// posted a prepared prompt — send it as a new user turn.
  void _promptFromEmbed(String text) {
    _sendText(text);
  }

  /// View an agent turn as a different surface. `web` is the canonical
  /// render (already in hand). Any chat channel is fetched ONCE from
  /// `POST /v1/surface/render`, which maps the SAME surface this bubble is
  /// already showing — so there's no tool re-invocation (and no second,
  /// possibly-different agent turn). The result is cached on the turn.
  Future<void> _selectChannel(_Turn turn, String channel) async {
    if (turn.channel == channel) return;
    setState(() => turn.channel = channel);
    if (channel == kWebChannel || turn.renders.containsKey(channel)) return;
    setState(() => turn.loadingChannel = channel);
    try {
      final res = await ref
          .read(restClientProvider)
          .renderSurface(adapter: channel, result: turn.result!.raw);
      if (!mounted) return;
      setState(() {
        turn.renders[channel] = res;
        turn.loadingChannel = null;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() {
        turn.renders[channel] = {'rendered': false, 'reason': e.toString()};
        turn.loadingChannel = null;
      });
    }
  }

  /// A rendered surface button/selection re-invokes a tool. A cross-tool button
  /// (e.g. an agent's `render_report`) is an MCP-Apps renderer — always
  /// dispatch it over MCP so its `ui://` report comes back, regardless of the
  /// selected protocol.
  Future<void> _onA2uiAction(String tool, Map<String, dynamic> args) async {
    final crossTool = _tool == null || tool != _tool!.name;
    final p = crossTool ? _Protocol.mcp : _protocol;
    final a2ui = crossTool ? null : _a2uiVersion;
    setState(() => _sending = true);
    final r = await _dispatch(tool, args, p, a2ui);
    if (!mounted) return;
    setState(() {
      _turns.add(_Turn.agent(r, a2ui));
      _sending = false;
    });
    _scrollToEnd();
  }

  void _scrollToEnd() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_scroll.hasClients) {
        _scroll.animateTo(
          _scroll.position.maxScrollExtent,
          duration: const Duration(milliseconds: 200),
          curve: Curves.easeOut,
        );
      }
    });
  }

  // ── build ───────────────────────────────────────────────────────────

  @override
  Widget build(BuildContext context) {
    final tools = ref.watch(toolsProvider);
    return Scaffold(
      appBar: AppBar(
        title: Text(_tool == null ? 'Console' : 'Chat · ${_tool!.name}'),
        actions: [
          IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh tools',
            onPressed: () => ref.invalidate(toolsProvider),
          ),
          IconButton(
            icon: Icon(_showOptions ? Icons.tune : Icons.tune_outlined),
            tooltip: 'Options',
            onPressed: () => setState(() => _showOptions = !_showOptions),
          ),
        ],
      ),
      body: tools.when(
        loading: () => const Center(child: CircularProgressIndicator()),
        error: (e, _) => Center(
          child: Padding(
            padding: const EdgeInsets.all(24),
            child: Text('Could not load /v1/tools: $e'),
          ),
        ),
        data: (list) => _layout(context, list),
      ),
    );
  }

  Widget _layout(BuildContext context, List<ToolDescriptor> list) {
    if (list.isEmpty) {
      return const Center(child: Text('Triton reports no registered tools.'));
    }
    // Default to the first upstream agent.
    if (_tool == null) {
      final initial = list.firstWhere(
        (t) => t.upstream,
        orElse: () => list.first,
      );
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted && _tool == null) _selectTool(initial);
      });
    }
    final hasDemo = list.any((t) => !t.upstream);
    final shown =
        (_showDemoTools ? list : list.where((t) => t.upstream).toList())
          ..sort((a, b) {
            if (a.upstream != b.upstream) return a.upstream ? -1 : 1;
            return a.name.compareTo(b.name);
          });

    return Row(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _toolRail(shown, hasDemo),
        const VerticalDivider(width: 1),
        Expanded(child: _chat(context)),
        if (_inspect != null) ...[
          const VerticalDivider(width: 1),
          SizedBox(
            width: 360,
            child: MessageTraceSidebar(
              // Keyed on the trace_id so switching between two bubbles' sidebars
              // re-fetches the right gateway timeline instead of reusing the
              // first FutureBuilder.
              key: ValueKey('trace-sidebar-${_inspect!.result!.traceId}'),
              traceId: _inspect!.result!.traceId,
              toolTrace: _inspect!.result!.toolTrace,
              onClose: () => setState(() => _inspect = null),
            ),
          ),
        ],
        if (_showOptions) ...[
          const VerticalDivider(width: 1),
          SizedBox(width: 320, child: _optionsPanel(context)),
        ],
      ],
    );
  }

  Widget _toolRail(List<ToolDescriptor> shown, bool hasDemo) => SizedBox(
    width: 210,
    child: Column(
      children: [
        if (hasDemo)
          CheckboxListTile(
            dense: true,
            controlAffinity: ListTileControlAffinity.leading,
            contentPadding: const EdgeInsets.symmetric(horizontal: 8),
            title: const Text('Show all tools', style: TextStyle(fontSize: 12)),
            value: _showDemoTools,
            onChanged: (v) => setState(() => _showDemoTools = v ?? false),
          ),
        Expanded(
          child: ListView.builder(
            itemCount: shown.length,
            itemBuilder: (_, i) {
              final t = shown[i];
              return ListTile(
                dense: true,
                title: Text(t.name),
                subtitle: t.upstream
                    ? const Text('agent', style: TextStyle(fontSize: 11))
                    : null,
                leading: Icon(
                  t.upstream ? Icons.smart_toy_outlined : Icons.bolt,
                  size: 18,
                  color: t.upstream ? Colors.amber.shade700 : null,
                ),
                selected: _tool?.name == t.name,
                onTap: () => _selectTool(t),
              );
            },
          ),
        ),
      ],
    ),
  );

  Widget _chat(BuildContext context) {
    return Column(
      children: [
        Expanded(
          child: _turns.isEmpty
              ? _emptyState(context)
              : ListView.builder(
                  controller: _scroll,
                  padding: const EdgeInsets.all(16),
                  itemCount: _turns.length,
                  itemBuilder: (_, i) => _bubble(context, _turns[i]),
                ),
        ),
        if (_sending) const LinearProgressIndicator(minHeight: 2),
        _composerBar(context),
      ],
    );
  }

  Widget _emptyState(BuildContext context) => Center(
    child: Padding(
      padding: const EdgeInsets.all(24),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(
            Icons.forum_outlined,
            size: 40,
            color: Theme.of(context).colorScheme.outline,
          ),
          const SizedBox(height: 12),
          Text(
            _tool == null
                ? 'Pick a tool on the left.'
                : 'Say something to ${_tool!.name}.',
            style: Theme.of(context).textTheme.titleMedium,
          ),
          const SizedBox(height: 4),
          Text(
            'Over ${_protocol.label}, A2UI v$_a2uiVersion. Tune it under Options (top-right).',
            style: Theme.of(context).textTheme.bodySmall,
          ),
        ],
      ),
    ),
  );

  Widget _bubble(BuildContext context, _Turn turn) {
    if (turn.isUser) {
      return Align(
        alignment: Alignment.centerRight,
        child: Container(
          margin: const EdgeInsets.symmetric(vertical: 6),
          padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
          constraints: const BoxConstraints(maxWidth: 560),
          decoration: BoxDecoration(
            color: Theme.of(context).colorScheme.primary,
            borderRadius: BorderRadius.circular(14),
          ),
          child: Text(
            turn.text ?? '',
            style: TextStyle(color: Theme.of(context).colorScheme.onPrimary),
          ),
        ),
      );
    }
    final r = turn.result!;
    return Align(
      alignment: Alignment.centerLeft,
      child: Container(
        margin: const EdgeInsets.symmetric(vertical: 6),
        constraints: const BoxConstraints(maxWidth: 760),
        child: Card(
          margin: EdgeInsets.zero,
          child: Padding(
            padding: const EdgeInsets.all(12),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                if (r.error != null)
                  Text(
                    r.error!,
                    style: TextStyle(
                      color: Theme.of(context).colorScheme.error,
                    ),
                  )
                else if (turn.channel != kWebChannel)
                  ChannelBubble(
                    adapter: turn.channel,
                    payload: turn.renders[turn.channel],
                    result: r,
                    loading: turn.loadingChannel == turn.channel,
                  )
                else if (turn.version != null)
                  A2UIRenderer(
                    envelope: r.raw,
                    version: turn.version,
                    onAction: _onA2uiAction,
                    onOpenResource: (uri) =>
                        setState(() => turn.openedResource = uri),
                  )
                else
                  JsonViewer(r.raw),
                // The channel selector — only when there's a canonical
                // surface to map (a2ui turn, no error). `web` shows the
                // native render; a chat channel previews the same answer.
                if (r.error == null && turn.version != null) ...[
                  const SizedBox(height: 8),
                  ChannelChips(
                    selected: turn.channel,
                    onSelected: (ch) => _selectChannel(turn, ch),
                  ),
                ],
                // The report/document embeds are web-surface affordances —
                // a channel preview shows only what that channel would post.
                if (turn.channel == kWebChannel && r.uiResourceUri != null) ...[
                  const SizedBox(height: 10),
                  UiResourceView(
                    uri: r.uiResourceUri!,
                    onPrompt: _promptFromEmbed,
                  ),
                ],
                // A Sources chip opened a document — embedded below the
                // surface (and below the auto-opened report, when both).
                if (turn.channel == kWebChannel &&
                    turn.openedResource != null) ...[
                  const SizedBox(height: 10),
                  UiResourceView(
                    uri: turn.openedResource!,
                    onPrompt: _promptFromEmbed,
                  ),
                ],
                const SizedBox(height: 8),
                _turnFooter(context, turn),
              ],
            ),
          ),
        ),
      ),
    );
  }

  Widget _turnFooter(BuildContext context, _Turn turn) {
    final r = turn.result!;
    final style = Theme.of(context).textTheme.bodySmall?.copyWith(
      color: Theme.of(context).colorScheme.onSurfaceVariant,
    );
    // A reply is inspectable when it carries a trace_id (gateway timeline) or
    // a tool trace (which tools/data it touched) — usually both.
    final inspectable =
        r.traceId != null || (r.toolTrace?.isNotEmpty ?? false);
    return Wrap(
      spacing: 10,
      runSpacing: 4,
      crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        Text('${r.statusCode} · ${r.elapsed.inMilliseconds}ms', style: style),
        if (inspectable)
          // Open THIS message's trace in the sidebar (tools + data touched +
          // the gateway timeline) — no separate Trace tab to navigate to.
          InkWell(
            key: const Key('inspect-trace'),
            onTap: () => setState(() => _inspect = turn),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                const Icon(Icons.travel_explore, size: 13),
                const SizedBox(width: 3),
                Text(
                  r.traceId != null
                      ? 'inspect · trace ${r.traceId!.substring(0, 8)}'
                      : 'inspect',
                  style: style?.copyWith(decoration: TextDecoration.underline),
                ),
              ],
            ),
          ),
      ],
    );
  }

  Widget _composerBar(BuildContext context) {
    final disabled = _tool == null || _sending;
    return SafeArea(
      top: false,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(12, 6, 12, 12),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.end,
          children: [
            Expanded(
              child: TextField(
                key: const Key('chat-composer'),
                controller: _composer,
                minLines: 1,
                maxLines: 5,
                enabled: _tool != null,
                textInputAction: TextInputAction.send,
                onSubmitted: (_) => _send(),
                decoration: InputDecoration(
                  hintText: _tool == null
                      ? 'Pick a tool first'
                      : 'Message ${_tool!.name}…',
                  border: const OutlineInputBorder(),
                  isDense: true,
                  contentPadding: const EdgeInsets.symmetric(
                    horizontal: 12,
                    vertical: 12,
                  ),
                ),
              ),
            ),
            const SizedBox(width: 8),
            FilledButton(
              onPressed: disabled ? null : _send,
              style: FilledButton.styleFrom(
                padding: const EdgeInsets.symmetric(
                  horizontal: 18,
                  vertical: 14,
                ),
              ),
              child: const Icon(Icons.send, size: 18),
            ),
          ],
        ),
      ),
    );
  }

  // ── options panel (the "variations on the side") ────────────────────

  Widget _optionsPanel(BuildContext context) {
    final t = _tool;
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        Text('Options', style: Theme.of(context).textTheme.titleMedium),
        const SizedBox(height: 12),
        const Text('Protocol'),
        const SizedBox(height: 6),
        SegmentedButton<_Protocol>(
          segments: const [
            ButtonSegment(value: _Protocol.mcp, label: Text('MCP')),
            ButtonSegment(value: _Protocol.rest, label: Text('REST')),
            ButtonSegment(value: _Protocol.a2a, label: Text('A2A')),
          ],
          selected: {_protocol},
          onSelectionChanged: (s) => setState(() => _protocol = s.first),
        ),
        const SizedBox(height: 6),
        Text(
          'MCP carries embedded ui:// reports; REST/A2A are plain.',
          style: Theme.of(context).textTheme.bodySmall,
        ),
        const SizedBox(height: 16),
        const Text('A2UI render'),
        const SizedBox(height: 6),
        SegmentedButton<String>(
          segments: const [
            ButtonSegment(value: 'raw', label: Text('JSON')),
            ButtonSegment(value: '0.8', label: Text('v0.8')),
            ButtonSegment(value: '0.9', label: Text('v0.9')),
          ],
          selected: {_a2uiVersion == '' ? 'raw' : _a2uiVersion},
          onSelectionChanged: (s) =>
              setState(() => _a2uiVersion = s.first == 'raw' ? '' : s.first),
        ),
        if (t != null && _secondaryArgKeys(t).isNotEmpty) ...[
          const SizedBox(height: 20),
          Text('Arguments', style: Theme.of(context).textTheme.titleSmall),
          const SizedBox(height: 4),
          Text(
            'The message box fills "${_primaryArgKey(t)}".',
            style: Theme.of(context).textTheme.bodySmall,
          ),
          const SizedBox(height: 8),
          for (final name in _secondaryArgKeys(t)) _argField(t, name),
        ],
        const SizedBox(height: 20),
        ExpansionTile(
          tilePadding: EdgeInsets.zero,
          title: const Text('Advanced'),
          children: [
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              dense: true,
              title: const Text('Stream (REST)'),
              value: _streaming,
              onChanged: (v) => setState(() => _streaming = v),
            ),
            if (_turns.isNotEmpty && _turns.last.result != null) ...[
              const SizedBox(height: 8),
              Text(
                'Last raw envelope',
                style: Theme.of(context).textTheme.bodySmall,
              ),
              const SizedBox(height: 4),
              SizedBox(
                height: 180,
                child: SingleChildScrollView(
                  child: SelectableText(
                    const JsonEncoder.withIndent(
                      '  ',
                    ).convert(_turns.last.result!.raw),
                    style: const TextStyle(
                      fontFamily: 'monospace',
                      fontSize: 11,
                    ),
                  ),
                ),
              ),
            ],
          ],
        ),
      ],
    );
  }

  Widget _argField(ToolDescriptor t, String name) {
    final prop =
        ((t.inputSchema['properties'] as Map?)?[name] as Map?)
            ?.cast<String, dynamic>() ??
        const {};
    final type = prop['type'] as String?;
    if (type == 'boolean') {
      return SwitchListTile(
        contentPadding: EdgeInsets.zero,
        dense: true,
        title: Text(name),
        value: _extraArgs[name] as bool? ?? false,
        onChanged: (v) => setState(() => _extraArgs[name] = v),
      );
    }
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: TextField(
        controller: _extraCtrls[name],
        decoration: InputDecoration(
          labelText: name,
          helperText: prop['description'] as String?,
          border: const OutlineInputBorder(),
          isDense: true,
        ),
        onChanged: (v) {
          dynamic val = v;
          if (type == 'integer') val = int.tryParse(v) ?? v;
          if (type == 'number') val = double.tryParse(v) ?? v;
          _extraArgs[name] = val;
        },
      ),
    );
  }
}
