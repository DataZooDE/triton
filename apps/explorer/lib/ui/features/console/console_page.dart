import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/a2a_client.dart';
import '../../../api/mcp_client.dart';
import '../../../api/models.dart';
import '../../../api/rest_client.dart';
import '../../../providers/api_provider.dart';
import '../../../providers/runtime_provider.dart';
import '../../../widgets/a2ui/a2ui_renderer.dart';
import '../../../widgets/chat_channel_previews.dart';
import '../../../widgets/json_viewer.dart';
import '../../../widgets/panel_help.dart';

/// The Console — one place to understand the protocols, inject messages,
/// and make small corrections against a live tool.
///
/// It merges what used to be the Playground (pick a tool, invoke, render
/// the A2UI surface, see chat-channel degradation) and the Adapters page
/// (fire all three protocols, compare parity, MCP handshake, error
/// taxonomy) into a single editable surface: pick a tool and a protocol,
/// see the *exact* wire frame Triton expects, edit it, and send precisely
/// what you typed. The same `{tool, args}` can go to REST, MCP or A2A —
/// or all three at once — and the response renders both raw and as an
/// A2UI surface.
const _consoleWhat =
    'Pick a tool, choose a protocol (REST / MCP / A2A) and an A2UI '
    'version, and see the exact request envelope Triton expects. Edit it '
    'and Send to inject your own message or make a small correction.';
const _consoleHow =
    "Select a tool on the left. Edit the JSON envelope, then 'Send' (one "
    "protocol) or 'Send to all 3' (parity). Click a button/selection in a "
    'rendered surface to re-invoke the tool. Expand the sections below for '
    'the MCP handshake and the one-failure-three-idioms error taxonomy.';

enum _Protocol {
  rest('REST'),
  mcp('MCP'),
  a2a('A2A');

  const _Protocol(this.label);
  final String label;
}

class ConsolePage extends ConsumerStatefulWidget {
  const ConsolePage({super.key});

  @override
  ConsumerState<ConsolePage> createState() => _ConsolePageState();
}

class _ConsolePageState extends ConsumerState<ConsolePage> {
  ToolDescriptor? _selected;
  _Protocol _protocol = _Protocol.rest;
  String? _a2uiVersion;

  /// The canonical args, kept as the source of truth so switching
  /// protocol re-frames the *same* call rather than resetting it.
  Map<String, dynamic> _args = const {};

  final _envelopeController = TextEditingController();
  String? _envelopeError;

  /// Lets you target a tool that isn't in `/v1/tools` — e.g. an upstream
  /// agent Triton dispatches into by Consul tag (the `hello` adk-rust
  /// agent), which the in-process registry doesn't list.
  final _customController = TextEditingController();

  final Map<_Protocol, InvocationResult?> _results = {
    _Protocol.rest: null,
    _Protocol.mcp: null,
    _Protocol.a2a: null,
  };
  _Protocol _activeTab = _Protocol.rest;
  bool _sending = false;

  @override
  void dispose() {
    _envelopeController.dispose();
    _customController.dispose();
    super.dispose();
  }

  /// Select an arbitrary tool name (not necessarily in `/v1/tools`). We
  /// assume it may return an A2UI surface so the version selector shows.
  void _useCustomTool() {
    final name = _customController.text.trim();
    if (name.isEmpty) return;
    _selectTool(
      ToolDescriptor(name: name, inputSchema: const {}, returnsA2ui: true),
    );
  }

  // ── client + frame helpers ──────────────────────────────────────────

  WireRequest _buildFor(_Protocol p, Map<String, dynamic> args) {
    final a2ui = (_selected?.returnsA2ui ?? false) ? _a2uiVersion : null;
    switch (p) {
      case _Protocol.rest:
        return ref
            .read(restClientProvider)
            .buildRequest(_selected!.name, args, a2uiVersion: a2ui);
      case _Protocol.mcp:
        return ref
            .read(mcpClientProvider)
            .buildRequest(_selected!.name, args, a2uiVersion: a2ui);
      case _Protocol.a2a:
        return ref
            .read(a2aClientProvider)
            .buildRequest(_selected!.name, args, a2uiVersion: a2ui);
    }
  }

  Future<InvocationResult> _sendFrame(_Protocol p, WireRequest req) {
    switch (p) {
      case _Protocol.rest:
        return ref.read(restClientProvider).sendRaw(req);
      case _Protocol.mcp:
        return ref.read(mcpClientProvider).sendRaw(req);
      case _Protocol.a2a:
        return ref.read(a2aClientProvider).sendRaw(req);
    }
  }

  /// Pull the tool args back out of an (edited) frame so a protocol
  /// switch carries the user's edits forward.
  Map<String, dynamic> _argsFromBody(_Protocol p, Map<String, dynamic> body) {
    try {
      switch (p) {
        case _Protocol.rest:
          return body;
        case _Protocol.mcp:
          return ((body['params'] as Map?)?['arguments'] as Map?)
                  ?.cast<String, dynamic>() ??
              const {};
        case _Protocol.a2a:
          return (((body['parts'] as List?)?.first as Map?)?['data']
                      as Map?)?['args']
                  ?.cast<String, dynamic>() ??
              const {};
      }
    } catch (_) {
      return const {};
    }
  }

  String _pretty(Object? json) =>
      const JsonEncoder.withIndent('  ').convert(json);

  void _reseedEnvelope() {
    if (_selected == null) return;
    _envelopeController.text = _pretty(_buildFor(_protocol, _args).body);
    _envelopeError = null;
  }

  // ── events ──────────────────────────────────────────────────────────

  void _selectTool(ToolDescriptor t) {
    setState(() {
      _selected = t;
      _args = const {};
      _results.updateAll((_, _) => null);
      _a2uiVersion = t.returnsA2ui ? _a2uiVersion : null;
      _reseedEnvelope();
    });
  }

  void _setProtocol(_Protocol p) {
    setState(() {
      _protocol = p;
      _activeTab = p;
      _reseedEnvelope();
    });
  }

  void _setA2ui(String? v) {
    setState(() {
      _a2uiVersion = v;
      _reseedEnvelope();
    });
  }

  void _onEnvelopeChanged(String s) {
    setState(() {
      try {
        jsonDecode(s);
        _envelopeError = null;
      } catch (_) {
        _envelopeError = 'Invalid JSON';
      }
    });
  }

  /// Send the current editor contents over the active protocol.
  Future<void> _send() async {
    if (_selected == null) return;
    final Map<String, dynamic> body;
    try {
      body = (jsonDecode(_envelopeController.text) as Map)
          .cast<String, dynamic>();
    } catch (_) {
      setState(() => _envelopeError = 'Invalid JSON — fix before sending');
      return;
    }
    final p = _protocol;
    setState(() {
      _sending = true;
      _activeTab = p;
    });
    final req = _buildFor(p, const {}).withBody(body);
    final r = await _sendFrame(p, req);
    if (!mounted) return;
    if (r.traceId != null) {
      ref.read(selectedTraceProvider.notifier).state = r.traceId;
    }
    setState(() {
      _results[p] = r;
      _args = _argsFromBody(p, body);
      _sending = false;
    });
  }

  /// Fire the same call (the canonical args) over all three protocols.
  Future<void> _sendAll() async {
    if (_selected == null) return;
    // Adopt any edits in the current editor first.
    try {
      final body = (jsonDecode(_envelopeController.text) as Map)
          .cast<String, dynamic>();
      _args = _argsFromBody(_protocol, body);
    } catch (_) {
      /* keep last good args */
    }
    setState(() => _sending = true);
    final entries = await Future.wait(
      _Protocol.values.map((p) async {
        final r = await _sendFrame(p, _buildFor(p, _args));
        return MapEntry(p, r);
      }),
    );
    if (!mounted) return;
    setState(() {
      for (final e in entries) {
        _results[e.key] = e.value;
      }
      _sending = false;
    });
  }

  /// A rendered A2UI Button/Selection/Form re-invokes the tool over the
  /// active protocol (FR-D-3, the stateless round-trip).
  Future<void> _onA2uiAction(String tool, Map<String, dynamic> args) async {
    final p = _activeTab;
    setState(() => _sending = true);
    final r = await _sendFrame(p, _buildFor(p, args));
    if (!mounted) return;
    setState(() {
      _results[p] = r;
      _sending = false;
    });
  }

  // ── build ───────────────────────────────────────────────────────────

  @override
  Widget build(BuildContext context) {
    final tools = ref.watch(toolsProvider);
    return Scaffold(
      appBar: AppBar(
        title: const Text('Console'),
        actions: [
          IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh tool list',
            onPressed: () => ref.invalidate(toolsProvider),
          ),
        ],
      ),
      body: tools.when(
        loading: () => const Center(child: CircularProgressIndicator()),
        error: (e, _) => Padding(
          padding: const EdgeInsets.all(24),
          child: Card(
            color: Theme.of(context).colorScheme.errorContainer,
            child: Padding(
              padding: const EdgeInsets.all(16),
              child: Text('Could not load /v1/tools: $e'),
            ),
          ),
        ),
        data: (list) {
          if (list.isEmpty) {
            return const Center(
              child: Padding(
                padding: EdgeInsets.all(24),
                child: Text(
                  'Triton reports no registered tools.\n\n'
                  'Check the dispatcher registry on the gateway.',
                  textAlign: TextAlign.center,
                ),
              ),
            );
          }
          return Row(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              SizedBox(
                width: 240,
                child: Column(
                  children: [
                    Padding(
                      padding: const EdgeInsets.all(8),
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.stretch,
                        children: [
                          TextField(
                            key: const Key('custom-tool-field'),
                            controller: _customController,
                            decoration: const InputDecoration(
                              isDense: true,
                              border: OutlineInputBorder(),
                              labelText: 'Tool name',
                              hintText: 'e.g. hello (upstream agent)',
                              helperText: 'Target a tool not in /v1/tools',
                            ),
                            onSubmitted: (_) => _useCustomTool(),
                          ),
                          const SizedBox(height: 6),
                          Align(
                            alignment: Alignment.centerLeft,
                            child: TextButton.icon(
                              icon: const Icon(Icons.add, size: 18),
                              label: const Text('Use tool'),
                              onPressed: _useCustomTool,
                            ),
                          ),
                        ],
                      ),
                    ),
                    const Divider(height: 1),
                    Expanded(
                      child: ListView.builder(
                        itemCount: list.length,
                        itemBuilder: (context, i) {
                          final t = list[i];
                          final tag = t.upstream
                              ? 'upstream agent'
                              : (t.returnsA2ui ? 'returns A2UI' : null);
                          return ListTile(
                            title: Text(t.name),
                            subtitle: tag == null
                                ? null
                                : Text(
                                    tag,
                                    style: const TextStyle(fontSize: 11),
                                  ),
                            selected: _selected?.name == t.name,
                            onTap: () => _selectTool(t),
                          );
                        },
                      ),
                    ),
                  ],
                ),
              ),
              const VerticalDivider(width: 1),
              Expanded(
                child: _selected == null
                    ? const Center(child: Text('Pick a tool'))
                    : _detail(context),
              ),
            ],
          );
        },
      ),
    );
  }

  Widget _detail(BuildContext context) {
    final tool = _selected!;
    final guide = toolGuide(tool);
    final frame = _buildFor(_protocol, _args);
    final result = _results[_activeTab];
    return SingleChildScrollView(
      padding: const EdgeInsets.all(20),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(tool.name, style: Theme.of(context).textTheme.headlineSmall),
          PanelHelp(
            what: '$_consoleWhat\n\n${guide.what}',
            how: '${guide.how}\n\n$_consoleHow',
            margin: const EdgeInsets.only(top: 12),
          ),
          const SizedBox(height: 16),

          // Protocol + A2UI selectors.
          Wrap(
            spacing: 24,
            runSpacing: 12,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  const Text('Protocol:'),
                  const SizedBox(width: 12),
                  SegmentedButton<_Protocol>(
                    segments: const [
                      ButtonSegment(value: _Protocol.rest, label: Text('REST')),
                      ButtonSegment(value: _Protocol.mcp, label: Text('MCP')),
                      ButtonSegment(value: _Protocol.a2a, label: Text('A2A')),
                    ],
                    selected: {_protocol},
                    onSelectionChanged: (s) => _setProtocol(s.first),
                  ),
                ],
              ),
              if (tool.returnsA2ui)
                Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    const Text('A2UI:'),
                    const SizedBox(width: 12),
                    SegmentedButton<String?>(
                      segments: const [
                        ButtonSegment(value: null, label: Text('JSON')),
                        ButtonSegment(value: '0.8', label: Text('v0.8')),
                        ButtonSegment(value: '0.9', label: Text('v0.9')),
                      ],
                      selected: {_a2uiVersion},
                      onSelectionChanged: (s) => _setA2ui(s.first),
                    ),
                  ],
                ),
            ],
          ),
          const SizedBox(height: 16),

          // The editable wire frame.
          Text(
            'Request envelope',
            style: Theme.of(context).textTheme.titleMedium,
          ),
          const SizedBox(height: 4),
          SelectableText(
            '${frame.method} ${frame.url}\n'
            '${frame.headers.entries.map((e) => '${e.key}: ${e.value}').join('\n')}',
            style: const TextStyle(fontFamily: 'monospace', fontSize: 11),
          ),
          const SizedBox(height: 8),
          TextField(
            key: const Key('envelope-editor'),
            controller: _envelopeController,
            minLines: 6,
            maxLines: 18,
            style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
            decoration: InputDecoration(
              border: const OutlineInputBorder(),
              isDense: true,
              errorText: _envelopeError,
              helperText:
                  'Edit the JSON body, then Send — this is the exact '
                  'frame posted to Triton.',
            ),
            onChanged: _onEnvelopeChanged,
          ),
          const SizedBox(height: 12),
          Row(
            children: [
              FilledButton.icon(
                icon: _sending
                    ? const SizedBox(
                        width: 16,
                        height: 16,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      )
                    : const Icon(Icons.send),
                label: const Text('Send'),
                onPressed: _sending || _envelopeError != null ? null : _send,
              ),
              const SizedBox(width: 12),
              OutlinedButton.icon(
                icon: const Icon(Icons.bolt),
                label: const Text('Send to all 3'),
                onPressed: _sending ? null : _sendAll,
              ),
            ],
          ),
          const SizedBox(height: 24),

          // Per-protocol responses.
          Text('Response', style: Theme.of(context).textTheme.titleMedium),
          const SizedBox(height: 8),
          SegmentedButton<_Protocol>(
            segments: [
              for (final p in _Protocol.values)
                ButtonSegment(
                  value: p,
                  label: Text(p.label),
                  icon: _results[p] == null
                      ? null
                      : Icon(
                          _results[p]!.ok ? Icons.check_circle : Icons.error,
                          size: 16,
                        ),
                ),
            ],
            selected: {_activeTab},
            onSelectionChanged: (s) => setState(() => _activeTab = s.first),
          ),
          const SizedBox(height: 12),
          _responseView(context, result),

          // Chat-channel degradation for A2UI tools.
          if (tool.returnsA2ui && result != null && result.ok) ...[
            const SizedBox(height: 24),
            const Divider(),
            const SizedBox(height: 12),
            ChatChannelPreviews(toolName: tool.name, args: _args),
          ],

          const SizedBox(height: 24),
          const Divider(),
          const _McpDetailSection(),
          const Divider(height: 1),
          const _ErrorTaxonomySection(),
        ],
      ),
    );
  }

  Widget _responseView(BuildContext context, InvocationResult? r) {
    if (r == null) {
      return Text(
        'Not sent yet — press Send.',
        style: Theme.of(context).textTheme.bodySmall,
      );
    }
    final showSurface =
        (_selected?.returnsA2ui ?? false) && _a2uiVersion != null && r.ok;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Wrap(
          spacing: 8,
          runSpacing: 4,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            Chip(
              label: Text('${r.statusCode} • ${r.elapsed.inMilliseconds}ms'),
              visualDensity: VisualDensity.compact,
              backgroundColor: r.ok
                  ? Theme.of(context).colorScheme.secondaryContainer
                  : Theme.of(context).colorScheme.errorContainer,
            ),
            if (r.errorCode != null)
              Chip(
                label: Text('rpc ${r.errorCode}'),
                visualDensity: VisualDensity.compact,
              ),
            if (r.taskState != null)
              Chip(
                avatar: const Icon(Icons.task_alt, size: 16),
                label: Text('task: ${r.taskState}'),
                visualDensity: VisualDensity.compact,
              ),
            if (r.traceId != null)
              SelectableText(
                'trace ${r.traceId}',
                style: Theme.of(context).textTheme.bodySmall,
              ),
          ],
        ),
        const SizedBox(height: 8),
        if (r.error != null)
          Card(
            color: Theme.of(context).colorScheme.errorContainer,
            child: Padding(
              padding: const EdgeInsets.all(12),
              child: Text(r.error!),
            ),
          ),
        if (showSurface) ...[
          Text(
            'Rendered surface — MCP App / web client',
            style: Theme.of(context).textTheme.titleSmall,
          ),
          const SizedBox(height: 6),
          Card(
            child: Padding(
              padding: const EdgeInsets.all(12),
              child: A2UIRenderer(
                envelope: r.raw,
                version: _a2uiVersion,
                onAction: _onA2uiAction,
              ),
            ),
          ),
          const SizedBox(height: 12),
        ],
        Text('Raw envelope', style: Theme.of(context).textTheme.titleSmall),
        const SizedBox(height: 6),
        JsonViewer(r.raw),
      ],
    );
  }
}

/// MCP-only handshake. `initialize` negotiates a protocol version and
/// advertises capabilities; `resources/read` fetches the runtime stub
/// Triton serves at `ui://triton/runtime.html`. Lazy: only fires when
/// the tile is first expanded.
class _McpDetailSection extends ConsumerStatefulWidget {
  const _McpDetailSection();

  @override
  ConsumerState<_McpDetailSection> createState() => _McpDetailSectionState();
}

class _McpDetailSectionState extends ConsumerState<_McpDetailSection> {
  static const _runtimeUri = 'ui://triton/runtime.html';

  bool _loading = false;
  bool _loaded = false;
  String? _error;
  McpServerInfo? _info;
  McpResource? _resource;

  Future<void> _load() async {
    if (_loaded || _loading) return;
    setState(() {
      _loading = true;
      _error = null;
    });
    final mcp = ref.read(mcpClientProvider);
    try {
      final info = await mcp.initialize();
      final resource = await mcp.readResource(_runtimeUri);
      if (!mounted) return;
      setState(() {
        _info = info;
        _resource = resource;
        _loaded = true;
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
    return ExpansionTile(
      leading: const Icon(Icons.handshake_outlined),
      title: const Text('MCP handshake & resources'),
      subtitle: const Text('initialize + resources/read — MCP only'),
      onExpansionChanged: (open) {
        if (open) _load();
      },
      childrenPadding: const EdgeInsets.fromLTRB(24, 0, 24, 16),
      expandedCrossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (_loading)
          const Padding(
            padding: EdgeInsets.all(16),
            child: Center(child: CircularProgressIndicator()),
          )
        else if (_error != null)
          Card(
            color: Theme.of(context).colorScheme.errorContainer,
            child: Padding(
              padding: const EdgeInsets.all(12),
              child: Text('initialize/resources failed: $_error'),
            ),
          )
        else if (_info != null) ...[
          Text('initialize', style: Theme.of(context).textTheme.titleSmall),
          const SizedBox(height: 8),
          _kv(context, 'protocolVersion', _info!.protocolVersion),
          _kv(
            context,
            'serverInfo',
            '${_info!.serverName} ${_info!.serverVersion}',
          ),
          _kv(context, 'capabilities', _info!.capabilities.keys.join(', ')),
          const SizedBox(height: 16),
          Text(
            'resources/read  ·  $_runtimeUri',
            style: Theme.of(context).textTheme.titleSmall,
          ),
          const SizedBox(height: 8),
          _kv(context, 'mimeType', _resource?.mimeType ?? ''),
          const SizedBox(height: 8),
          Container(
            width: double.infinity,
            padding: const EdgeInsets.all(12),
            decoration: BoxDecoration(
              color: Theme.of(context).colorScheme.surfaceContainerHigh,
              borderRadius: BorderRadius.circular(8),
            ),
            child: SelectableText(
              _resource?.text ?? '',
              style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
            ),
          ),
        ],
      ],
    );
  }

  Widget _kv(BuildContext context, String k, String v) => Padding(
    padding: const EdgeInsets.symmetric(vertical: 2),
    child: Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        SizedBox(
          width: 140,
          child: Text(
            k,
            style: Theme.of(context).textTheme.bodySmall?.copyWith(
              color: Theme.of(context).colorScheme.onSurfaceVariant,
            ),
          ),
        ),
        Expanded(child: SelectableText(v)),
      ],
    ),
  );
}

/// Fires deliberately-failing calls across the trio and tabulates how
/// each protocol reports the same failure. Demonstrates FR-D-4: one
/// error taxonomy, three idioms (REST/A2A → HTTP status + class,
/// MCP → JSON-RPC code in an HTTP-200 envelope).
class _ErrorTaxonomySection extends ConsumerStatefulWidget {
  const _ErrorTaxonomySection();

  @override
  ConsumerState<_ErrorTaxonomySection> createState() =>
      _ErrorTaxonomySectionState();
}

class _ScenarioRow {
  _ScenarioRow(this.name, this.detail, this.rest, this.mcp, this.a2a);
  final String name;
  final String detail;
  final InvocationResult rest;
  final InvocationResult mcp;
  final InvocationResult a2a;
}

class _ErrorTaxonomySectionState extends ConsumerState<_ErrorTaxonomySection> {
  bool _running = false;
  List<_ScenarioRow> _rows = const [];

  Future<void> _run() async {
    setState(() => _running = true);
    final rest = ref.read(restClientProvider);
    final mcp = ref.read(mcpClientProvider);
    final a2a = ref.read(a2aClientProvider);

    // Auth scenario needs unauthenticated clients. Reuse the resolved
    // base URLs from the providers, just drop the token.
    final dio = ref.read(dioProvider);
    final restNoAuth = RestClient(dio, baseUrl: rest.baseUrl, token: '');
    final mcpNoAuth = McpClient(dio, baseUrl: mcp.baseUrl, token: '');
    final a2aNoAuth = A2aClient(dio, baseUrl: a2a.baseUrl, token: '');

    Future<_ScenarioRow> scenario(
      String name,
      String detail, {
      required Future<InvocationResult> Function(RestClient) onRest,
      required Future<InvocationResult> Function(McpClient) onMcp,
      required Future<InvocationResult> Function(A2aClient) onA2a,
      bool noAuth = false,
    }) async {
      final results = await Future.wait([
        onRest(noAuth ? restNoAuth : rest),
        onMcp(noAuth ? mcpNoAuth : mcp),
        onA2a(noAuth ? a2aNoAuth : a2a),
      ]);
      return _ScenarioRow(name, detail, results[0], results[1], results[2]);
    }

    final rows = await Future.wait([
      // narrate requires `subject`; an empty body fails validation.
      scenario(
        'validation',
        'narrate {} — missing required "subject"',
        onRest: (c) => c.invoke('narrate', const {}),
        onMcp: (c) => c.invoke('narrate', const {}),
        onA2a: (c) => c.invoke('narrate', const {}),
      ),
      scenario(
        'not found',
        'invoke "__missing__" — unknown tool',
        onRest: (c) => c.invoke('__missing__', const {}),
        onMcp: (c) => c.invoke('__missing__', const {}),
        onA2a: (c) => c.invoke('__missing__', const {}),
      ),
      scenario(
        'auth',
        'echo with no bearer token',
        onRest: (c) => c.invoke('echo', const {}),
        onMcp: (c) => c.invoke('echo', const {}),
        onA2a: (c) => c.invoke('echo', const {}),
        noAuth: true,
      ),
    ]);

    if (!mounted) return;
    setState(() {
      _rows = rows;
      _running = false;
    });
  }

  /// Short per-protocol cell: HTTP status + error class for REST/A2A,
  /// JSON-RPC code for MCP (whose failures ride an HTTP-200 envelope).
  String _cell(InvocationResult r, {required bool mcp}) {
    final cls =
        (r.raw['error'] as String?) ?? (r.raw['metadata']?['error'] as String?);
    if (mcp) {
      if (r.errorCode != null) {
        return 'rpc ${r.errorCode}${cls != null ? ' · $cls' : ''}';
      }
      return r.ok ? '200 ok' : '200 ${r.error ?? '?'}';
    }
    return '${r.statusCode}${cls != null ? ' · $cls' : ''}';
  }

  @override
  Widget build(BuildContext context) {
    return ExpansionTile(
      leading: const Icon(Icons.rule),
      title: const Text('Error taxonomy'),
      subtitle: const Text('one failure, three idioms — FR-D-4'),
      childrenPadding: const EdgeInsets.fromLTRB(24, 0, 24, 16),
      expandedCrossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Align(
          alignment: Alignment.centerLeft,
          child: FilledButton.icon(
            icon: _running
                ? const SizedBox(
                    width: 16,
                    height: 16,
                    child: CircularProgressIndicator(strokeWidth: 2),
                  )
                : const Icon(Icons.play_arrow),
            label: const Text('Run error scenarios'),
            onPressed: _running ? null : _run,
          ),
        ),
        const SizedBox(height: 12),
        if (_rows.isNotEmpty)
          SingleChildScrollView(
            scrollDirection: Axis.horizontal,
            child: DataTable(
              columns: const [
                DataColumn(label: Text('scenario')),
                DataColumn(label: Text('REST')),
                DataColumn(label: Text('MCP')),
                DataColumn(label: Text('A2A')),
              ],
              rows: [
                for (final row in _rows)
                  DataRow(
                    cells: [
                      DataCell(
                        Tooltip(message: row.detail, child: Text(row.name)),
                      ),
                      DataCell(Text(_cell(row.rest, mcp: false))),
                      DataCell(Text(_cell(row.mcp, mcp: true))),
                      DataCell(Text(_cell(row.a2a, mcp: false))),
                    ],
                  ),
              ],
            ),
          ),
        const SizedBox(height: 12),
        Text(
          'Not shown live (need server config / upstream behaviour, '
          'covered by backend integration tests): rate-limit '
          '429 / rpc -32002, circuit-open 503, upstream timeout 504.',
          style: Theme.of(context).textTheme.bodySmall?.copyWith(
            color: Theme.of(context).colorScheme.onSurfaceVariant,
          ),
        ),
      ],
    );
  }
}
