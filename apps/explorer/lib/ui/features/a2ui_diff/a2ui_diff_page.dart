import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/models.dart';
import '../../../providers/api_provider.dart';
import '../../../widgets/a2ui/a2ui_renderer.dart';
import '../../../widgets/json_viewer.dart';

/// Three-column comparison: the same tool rendered through the A2UI
/// v0.8 builder, the v0.9 builder, and a selectable chat-channel
/// surface mapper. The chat column has an adapter dropdown
/// (telegram / discord / googlechat / msteams / signal / whatsapp)
/// so the operator can see how each platform degrades the same
/// surface (Telegram inline keyboards, Discord components v2, MS
/// Teams Adaptive Cards, …) — all through the SAME mappers the live
/// couriers use, via POST /v1/surface/render.
class A2uiDiffPage extends ConsumerStatefulWidget {
  const A2uiDiffPage({super.key});

  @override
  ConsumerState<A2uiDiffPage> createState() => _A2uiDiffPageState();
}

/// Closed set mirrored from `PREVIEW_ADAPTERS` in
/// `triton-adapters-http`'s rest.rs. If the server gains an adapter,
/// add it here; an unknown value just yields a 400 the column shows.
const _chatAdapters = <String>[
  'telegram',
  'discord',
  'googlechat',
  'msteams',
  'signal',
  'whatsapp',
];

class _A2uiDiffPageState extends ConsumerState<A2uiDiffPage> {
  ToolDescriptor? _selected;
  InvocationResult? _v08;
  InvocationResult? _v09;
  String _chatAdapter = 'telegram';
  Map<String, dynamic>? _chat;
  String? _chatError;
  bool _running = false;
  bool _chatLoading = false;

  Future<void> _run() async {
    final tool = _selected;
    if (tool == null) return;
    setState(() {
      _running = true;
      _v08 = null;
      _v09 = null;
      _chat = null;
      _chatError = null;
    });
    final client = ref.read(restClientProvider);
    final v08 = await client.invoke(tool.name, const {}, a2uiVersion: '0.8');
    final v09 = await client.invoke(tool.name, const {}, a2uiVersion: '0.9');
    if (!mounted) return;
    setState(() {
      _v08 = v08;
      _v09 = v09;
      _running = false;
    });
    await _renderChat();
  }

  /// Render the currently-selected chat adapter against the most
  /// recent v0.9 surface. Called both after `_run()` and when the
  /// adapter dropdown changes (no tool re-invocation needed).
  Future<void> _renderChat() async {
    final v09 = _v09;
    if (v09 == null || !v09.ok) return;
    setState(() {
      _chatLoading = true;
      _chat = null;
      _chatError = null;
    });
    final client = ref.read(restClientProvider);
    try {
      final res = await client.renderSurface(
        adapter: _chatAdapter,
        result: {'surface': _streamToSurface(v09.raw)},
      );
      if (!mounted) return;
      setState(() {
        _chat = res;
        _chatLoading = false;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _chatError = e.toString();
        _chatLoading = false;
      });
    }
  }

  /// Reverse-map a v0.9 envelope back into the canonical Surface
  /// shape the mappers expect. v0.9 is structurally close: each
  /// stream entry has `type`, `text`, `label`, etc.
  Map<String, dynamic> _streamToSurface(Map<String, dynamic> envelope) {
    final stream = (envelope['stream'] as List?) ?? const [];
    final components = <Map<String, dynamic>>[];
    for (final raw in stream) {
      if (raw is! Map) continue;
      final m = raw.cast<String, dynamic>();
      switch (m['type']) {
        case 'text':
          components
              .add({'kind': 'text', 'value': (m['text'] as String?) ?? ''});
          break;
        case 'narration':
          components
              .add({'kind': 'narration', 'text': (m['text'] as String?) ?? ''});
          break;
        case 'button':
          final action = (m['action'] as Map?)?.cast<String, dynamic>() ??
              const <String, dynamic>{};
          components.add({
            'kind': 'button',
            'label': (m['label'] as String?) ?? '',
            'tool': (action['tool'] as String?) ?? '',
            'args': action['args'] ?? <String, dynamic>{},
          });
          break;
        case 'selection':
          components.add({
            'kind': 'selection',
            'prompt': (m['prompt'] as String?) ?? '',
            'options': m['options'] ?? const [],
            'tool': (m['tool'] as String?) ?? '',
            'args_key': (m['args_key'] as String?) ?? 'value',
          });
          break;
        case 'form':
          components.add({
            'kind': 'form',
            'title': (m['title'] as String?) ?? '',
            'fields': m['fields'] ?? const [],
            'submit_label': (m['submit_label'] as String?) ?? 'Submit',
            'tool': (m['tool'] as String?) ?? '',
          });
          break;
        case 'dashboard':
          components.add({
            'kind': 'dashboard',
            'title': (m['title'] as String?) ?? '',
            'tiles': m['tiles'] ?? const [],
          });
          break;
      }
    }
    return {'components': components};
  }

  @override
  Widget build(BuildContext context) {
    final tools = ref.watch(toolsProvider);
    return Scaffold(
      appBar: AppBar(title: const Text('A2UI diff')),
      body: tools.when(
        loading: () => const Center(child: CircularProgressIndicator()),
        error: (e, _) => Padding(
          padding: const EdgeInsets.all(24),
          child: Text('Could not load tools: $e'),
        ),
        data: (list) {
          final a2uiTools = list.where((t) => t.returnsA2ui).toList();
          if (a2uiTools.isEmpty) {
            return const Center(
              child: Padding(
                padding: EdgeInsets.all(24),
                child: Text(
                  'No tool in the registry advertises returns_a2ui.\n\n'
                  'Wire one in triton-bin/src/tools/ first.',
                  textAlign: TextAlign.center,
                ),
              ),
            );
          }
          return Column(
            children: [
              Padding(
                padding: const EdgeInsets.all(16),
                child: Row(
                  children: [
                    DropdownButton<ToolDescriptor>(
                      hint: const Text('Pick an A2UI tool'),
                      value: _selected,
                      onChanged: (t) => setState(() => _selected = t),
                      items: [
                        for (final t in a2uiTools)
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
                          : const Icon(Icons.compare),
                      label: const Text('Invoke'),
                      onPressed: _running || _selected == null ? null : _run,
                    ),
                  ],
                ),
              ),
              const Divider(height: 1),
              Expanded(
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    Expanded(child: _renderColumn('v0.8', _v08)),
                    const VerticalDivider(width: 1),
                    Expanded(child: _renderColumn('v0.9', _v09)),
                    const VerticalDivider(width: 1),
                    Expanded(child: _chatColumn()),
                  ],
                ),
              ),
            ],
          );
        },
      ),
    );
  }

  Widget _renderColumn(String label, InvocationResult? result) =>
      SingleChildScrollView(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(label, style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            if (result == null)
              const Text('—')
            else ...[
              Card(
                child: Padding(
                  padding: const EdgeInsets.all(12),
                  child: A2UIRenderer(envelope: result.raw),
                ),
              ),
              const SizedBox(height: 12),
              Text('raw envelope',
                  style: Theme.of(context).textTheme.bodySmall),
              const SizedBox(height: 4),
              JsonViewer(result.raw),
            ],
          ],
        ),
      );

  Widget _chatColumn() {
    final result = _chat;
    return SingleChildScrollView(
      padding: const EdgeInsets.all(16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              DropdownButton<String>(
                value: _chatAdapter,
                onChanged: (a) {
                  if (a == null) return;
                  setState(() => _chatAdapter = a);
                  _renderChat();
                },
                items: [
                  for (final a in _chatAdapters)
                    DropdownMenuItem(value: a, child: Text(a)),
                ],
              ),
              const SizedBox(width: 8),
              const Chip(
                label: Text('L6′'),
                visualDensity: VisualDensity.compact,
              ),
              if (_chatLoading) ...[
                const SizedBox(width: 8),
                const SizedBox(
                  width: 14,
                  height: 14,
                  child: CircularProgressIndicator(strokeWidth: 2),
                ),
              ],
            ],
          ),
          const SizedBox(height: 8),
          if (_chatError != null)
            Card(
              color: Theme.of(context).colorScheme.errorContainer,
              child: Padding(
                padding: const EdgeInsets.all(12),
                child: Text(_chatError!),
              ),
            )
          else if (result == null)
            const Text('— invoke a tool —')
          else if (result['rendered'] == false)
            Card(
              color: Theme.of(context).colorScheme.surfaceContainerHigh,
              child: Padding(
                padding: const EdgeInsets.all(12),
                child: Text(
                  '$_chatAdapter produced no usable message '
                  '(reason: ${result['reason']}). The platform would '
                  'skip this post entirely.',
                ),
              ),
            )
          else ...[
            Card(
              child: Padding(
                padding: const EdgeInsets.all(12),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Wrap(
                      spacing: 6,
                      runSpacing: 4,
                      children: _chips(result),
                    ),
                    const SizedBox(height: 8),
                    SelectableText(
                      result['text'] as String? ?? '',
                      style: const TextStyle(
                        fontFamily: 'monospace',
                        fontSize: 13,
                        height: 1.4,
                      ),
                    ),
                  ],
                ),
              ),
            ),
            const SizedBox(height: 12),
            Text('raw response',
                style: Theme.of(context).textTheme.bodySmall),
            const SizedBox(height: 4),
            JsonViewer(result),
          ],
        ],
      ),
    );
  }

  /// Build the metadata chips from whatever fields the adapter's
  /// response carried — they differ per platform.
  List<Widget> _chips(Map<String, dynamic> r) {
    final chips = <Widget>[];
    void add(String label) => chips.add(Chip(
          label: Text(label),
          visualDensity: VisualDensity.compact,
        ));
    if (r['parse_mode'] != null) add('parse_mode: ${r['parse_mode']}');
    for (final k in [
      'deferred_buttons',
      'deferred_selections',
      'deferred_forms',
      'deferred_dashboards',
    ]) {
      final v = r[k];
      if (v is int && v > 0) add('$k: $v');
    }
    if (r['components'] != null) add('components ✓');
    if (r['has_dashboard_raster'] == true) add('dashboard → PNG');
    if (r['truncated'] == true) {
      chips.add(const Chip(
        avatar: Icon(Icons.warning_amber, size: 16),
        label: Text('truncated'),
        visualDensity: VisualDensity.compact,
      ));
    }
    if (chips.isEmpty) add('no degradation');
    return chips;
  }
}
