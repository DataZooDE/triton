import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/a2a_client.dart';
import '../../../api/mcp_client.dart';
import '../../../api/models.dart';
import '../../../api/rest_client.dart';
import '../../../providers/api_provider.dart';
import '../../../providers/runtime_provider.dart';
import '../../../widgets/json_viewer.dart';

/// Side-by-side comparison: fires the same {tool, args} against
/// REST, MCP, and A2A in parallel via Future.wait, then shows each
/// response in its own column with status, latency, and trace_id.
/// The user can verify Triton's three-way parity (ACC-1) live.
///
/// Below the parity row two sections show what the three protocols
/// do *not* share: the MCP `initialize`/`resources/read` handshake
/// (MCP-only), and the error taxonomy — the same failure mapped to
/// each protocol's idiom (HTTP status vs JSON-RPC code).
class AdaptersPage extends ConsumerStatefulWidget {
  const AdaptersPage({super.key});

  @override
  ConsumerState<AdaptersPage> createState() => _AdaptersPageState();
}

class _AdaptersPageState extends ConsumerState<AdaptersPage> {
  ToolDescriptor? _selected;
  InvocationResult? _rest;
  InvocationResult? _mcp;
  InvocationResult? _a2a;
  bool _running = false;

  Future<void> _run() async {
    final tool = _selected;
    if (tool == null) return;
    setState(() {
      _running = true;
      _rest = null;
      _mcp = null;
      _a2a = null;
    });
    final rest = ref.read(restClientProvider);
    final mcp = ref.read(mcpClientProvider);
    final a2a = ref.read(a2aClientProvider);
    final results = await Future.wait([
      rest.invoke(tool.name, const {}),
      mcp.invoke(tool.name, const {}),
      a2a.invoke(tool.name, const {}),
    ]);
    if (!mounted) return;
    setState(() {
      _rest = results[0];
      _mcp = results[1];
      _a2a = results[2];
      _running = false;
    });
  }

  @override
  Widget build(BuildContext context) {
    final tools = ref.watch(toolsProvider);
    return Scaffold(
      appBar: AppBar(title: const Text('Adapters')),
      body: tools.when(
        loading: () => const Center(child: CircularProgressIndicator()),
        error: (e, _) => Padding(
          padding: const EdgeInsets.all(24),
          child: Text('Could not load tools: $e'),
        ),
        data: (list) => Column(
          children: [
            Padding(
              padding: const EdgeInsets.all(16),
              child: Row(
                children: [
                  DropdownButton<ToolDescriptor>(
                    hint: const Text('Pick a tool'),
                    value: _selected,
                    onChanged: (t) => setState(() => _selected = t),
                    items: [
                      for (final t in list)
                        DropdownMenuItem(value: t, child: Text(t.name)),
                    ],
                  ),
                  const SizedBox(width: 16),
                  FilledButton.icon(
                    icon: _running
                        ? const SizedBox(
                            width: 16,
                            height: 16,
                            child: CircularProgressIndicator(strokeWidth: 2),
                          )
                        : const Icon(Icons.bolt),
                    label: const Text('Fire all three'),
                    onPressed: _running || _selected == null ? null : _run,
                  ),
                ],
              ),
            ),
            const Divider(height: 1),
            Expanded(
              child: SingleChildScrollView(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    IntrinsicHeight(
                      child: Row(
                        crossAxisAlignment: CrossAxisAlignment.stretch,
                        children: [
                          Expanded(child: _column('REST', _rest)),
                          const VerticalDivider(width: 1),
                          Expanded(child: _column('MCP', _mcp)),
                          const VerticalDivider(width: 1),
                          Expanded(child: _column('A2A', _a2a)),
                        ],
                      ),
                    ),
                    const Divider(height: 1),
                    const _McpDetailSection(),
                    const Divider(height: 1),
                    const _ErrorTaxonomySection(),
                  ],
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _column(String label, InvocationResult? r) => Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(
              children: [
                Text(label, style: Theme.of(context).textTheme.titleMedium),
                const Spacer(),
                if (r != null)
                  Chip(
                    label: Text('${r.statusCode} • ${r.elapsed.inMilliseconds}ms'),
                    visualDensity: VisualDensity.compact,
                    backgroundColor: r.ok
                        ? Theme.of(context).colorScheme.secondaryContainer
                        : Theme.of(context).colorScheme.errorContainer,
                  ),
              ],
            ),
            const SizedBox(height: 8),
            if (r == null)
              const Text('—')
            else ...[
              if (r.traceId != null)
                SelectableText('trace ${r.traceId}',
                    style: Theme.of(context).textTheme.bodySmall),
              // A2A is the only protocol that carries a task lifecycle
              // (FR-A-7): its in-process store echoes the terminal
              // state on success. REST/MCP leave taskState null.
              if (r.taskState != null) ...[
                const SizedBox(height: 4),
                Align(
                  alignment: Alignment.centerLeft,
                  child: Chip(
                    avatar: const Icon(Icons.task_alt, size: 16),
                    label: Text('task: ${r.taskState}'),
                    visualDensity: VisualDensity.compact,
                  ),
                ),
              ],
              const SizedBox(height: 8),
              if (r.error != null)
                Card(
                  color: Theme.of(context).colorScheme.errorContainer,
                  child: Padding(
                    padding: const EdgeInsets.all(12),
                    child: Text(r.error!),
                  ),
                ),
              JsonViewer(r.raw),
            ],
          ],
        ),
      );
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
          Text('initialize',
              style: Theme.of(context).textTheme.titleSmall),
          const SizedBox(height: 8),
          _kv(context, 'protocolVersion', _info!.protocolVersion),
          _kv(context, 'serverInfo',
              '${_info!.serverName} ${_info!.serverVersion}'),
          _kv(context, 'capabilities', _info!.capabilities.keys.join(', ')),
          const SizedBox(height: 16),
          Text('resources/read  ·  $_runtimeUri',
              style: Theme.of(context).textTheme.titleSmall),
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
              child: Text(k,
                  style: Theme.of(context).textTheme.bodySmall?.copyWith(
                        color: Theme.of(context).colorScheme.onSurfaceVariant,
                      )),
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

class _ErrorTaxonomySectionState
    extends ConsumerState<_ErrorTaxonomySection> {
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
      return _ScenarioRow(
          name, detail, results[0], results[1], results[2]);
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
    final cls = (r.raw['error'] as String?) ??
        (r.raw['metadata']?['error'] as String?);
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
                  DataRow(cells: [
                    DataCell(Tooltip(
                      message: row.detail,
                      child: Text(row.name),
                    )),
                    DataCell(Text(_cell(row.rest, mcp: false))),
                    DataCell(Text(_cell(row.mcp, mcp: true))),
                    DataCell(Text(_cell(row.a2a, mcp: false))),
                  ]),
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
