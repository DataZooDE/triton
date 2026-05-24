import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/models.dart';
import '../../../providers/api_provider.dart';
import '../../../widgets/a2ui/a2ui_renderer.dart';
import '../../../widgets/json_viewer.dart';

/// Side-by-side comparison: the same tool rendered through the
/// A2UI v0.8 builder, the v0.9 builder, and the Telegram surface
/// mapper. Fires three REST calls in parallel — two `/v1/tools/...`
/// with pinned Accept versions, and one `/v1/surface/render`
/// against the v0.8 result — then renders each in its own column
/// with the raw payload below.
///
/// The Telegram column lets the operator see the L6′ degradation
/// (the buttons→inline-keyboard rung, narration→italic-HTML, etc.)
/// alongside the rich web rendering.
class A2uiDiffPage extends ConsumerStatefulWidget {
  const A2uiDiffPage({super.key});

  @override
  ConsumerState<A2uiDiffPage> createState() => _A2uiDiffPageState();
}

class _A2uiDiffPageState extends ConsumerState<A2uiDiffPage> {
  ToolDescriptor? _selected;
  InvocationResult? _v08;
  InvocationResult? _v09;
  Map<String, dynamic>? _telegram;
  String? _telegramError;
  bool _running = false;

  Future<void> _run() async {
    final tool = _selected;
    if (tool == null) return;
    setState(() {
      _running = true;
      _v08 = null;
      _v09 = null;
      _telegram = null;
      _telegramError = null;
    });
    final client = ref.read(restClientProvider);
    final v08 = await client.invoke(tool.name, const {}, a2uiVersion: '0.8');
    final v09 = await client.invoke(tool.name, const {}, a2uiVersion: '0.9');
    // For the Telegram render we feed the raw tool result (which has
    // a `surface` key when the tool advertises returns_a2ui). The
    // server flips the surface through the same mapper the live
    // Telegram courier uses.
    Map<String, dynamic>? telegram;
    String? error;
    if (v08.ok) {
      try {
        telegram = await client.renderSurface(
          adapter: 'telegram',
          // /v1/tools/... wraps the result; for the render endpoint
          // we want the raw `result` shape with a `surface` field.
          // The dispatcher returns `{result, returns_a2ui, trace_id, ...}`,
          // and when A2UI is negotiated the `result` field already
          // becomes an envelope. We use the v0.9 stream's underlying
          // surface — easier than reconstructing, since the tool
          // emitted it that way originally.
          result: {
            'surface': _streamToSurface(v09.raw),
          },
        );
      } catch (e) {
        error = e.toString();
      }
    }
    if (!mounted) return;
    setState(() {
      _v08 = v08;
      _v09 = v09;
      _telegram = telegram;
      _telegramError = error;
      _running = false;
    });
  }

  /// Reverse-map a v0.9 envelope back into the canonical Surface
  /// shape the mapper expects. v0.9 is structurally close: each
  /// stream entry has `type`, `text`, `label`, etc. We translate
  /// only the rungs the mapper handles today.
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
        // Selection / Form / Dashboard skipped: the Telegram mapper
        // renders them best-effort but the inverse mapping is lossy.
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
                      label: const Text('Invoke all three'),
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
                    Expanded(
                      child: _telegramColumn(
                        result: _telegram,
                        error: _telegramError,
                      ),
                    ),
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

  Widget _telegramColumn({
    required Map<String, dynamic>? result,
    required String? error,
  }) =>
      SingleChildScrollView(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(
              children: [
                Text('telegram',
                    style: Theme.of(context).textTheme.titleMedium),
                const SizedBox(width: 8),
                const Chip(
                  label: Text('L6′'),
                  visualDensity: VisualDensity.compact,
                ),
              ],
            ),
            const SizedBox(height: 8),
            if (error != null)
              Card(
                color: Theme.of(context).colorScheme.errorContainer,
                child: Padding(
                  padding: const EdgeInsets.all(12),
                  child: Text(error),
                ),
              )
            else if (result == null)
              const Text('—')
            else if (result['rendered'] == false)
              Card(
                color: Theme.of(context).colorScheme.surfaceContainerHigh,
                child: Padding(
                  padding: const EdgeInsets.all(12),
                  child: Text(
                    'Mapper produced no usable Telegram message '
                    '(reason: ${result['reason']}). Telegram would skip '
                    'this post entirely.',
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
                        children: [
                          Chip(
                            label: Text(
                                'parse_mode: ${result['parse_mode'] ?? '—'}'),
                            visualDensity: VisualDensity.compact,
                          ),
                          Chip(
                            label: Text(
                                'deferred_buttons: ${result['deferred_buttons']}'),
                            visualDensity: VisualDensity.compact,
                          ),
                          if (result['truncated'] == true)
                            const Chip(
                              avatar: Icon(Icons.warning_amber, size: 16),
                              label: Text('truncated'),
                              visualDensity: VisualDensity.compact,
                            ),
                        ],
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
