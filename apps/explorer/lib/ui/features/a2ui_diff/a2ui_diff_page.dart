import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/models.dart';
import '../../../providers/api_provider.dart';
import '../../../widgets/a2ui/a2ui_renderer.dart';
import '../../../widgets/json_viewer.dart';

/// Side-by-side comparison of A2UI v0.8 vs v0.9 envelopes for the
/// same tool + args. Fires two REST invocations in parallel with the
/// version pinned in the `Accept` header, then renders each through
/// its own native Flutter tree. The raw envelopes show below the
/// renderers so the operator can read the schema delta directly.
class A2uiDiffPage extends ConsumerStatefulWidget {
  const A2uiDiffPage({super.key});

  @override
  ConsumerState<A2uiDiffPage> createState() => _A2uiDiffPageState();
}

class _A2uiDiffPageState extends ConsumerState<A2uiDiffPage> {
  ToolDescriptor? _selected;
  InvocationResult? _v08;
  InvocationResult? _v09;
  bool _running = false;

  Future<void> _run() async {
    final tool = _selected;
    if (tool == null) return;
    setState(() {
      _running = true;
      _v08 = null;
      _v09 = null;
    });
    final client = ref.read(restClientProvider);
    final results = await Future.wait([
      client.invoke(tool.name, const {}, a2uiVersion: '0.8'),
      client.invoke(tool.name, const {}, a2uiVersion: '0.9'),
    ]);
    if (!mounted) return;
    setState(() {
      _v08 = results[0];
      _v09 = results[1];
      _running = false;
    });
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
                      label: const Text('Invoke both'),
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
                    Expanded(child: _column('v0.8', _v08)),
                    const VerticalDivider(width: 1),
                    Expanded(child: _column('v0.9', _v09)),
                  ],
                ),
              ),
            ],
          );
        },
      ),
    );
  }

  Widget _column(String label, InvocationResult? result) =>
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
}
