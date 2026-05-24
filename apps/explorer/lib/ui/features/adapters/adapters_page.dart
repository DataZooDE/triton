import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/models.dart';
import '../../../providers/api_provider.dart';
import '../../../widgets/json_viewer.dart';

/// Side-by-side comparison: fires the same {tool, args} against
/// REST, MCP, and A2A in parallel via Future.wait, then shows each
/// response in its own column with status, latency, and trace_id.
/// The user can verify Triton's three-way parity (ACC-1) live.
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
          ],
        ),
      ),
    );
  }

  Widget _column(String label, InvocationResult? r) => SingleChildScrollView(
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
