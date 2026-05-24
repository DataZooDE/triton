import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/models.dart';
import '../../../providers/api_provider.dart';
import '../../../widgets/a2ui/a2ui_renderer.dart';
import '../../../widgets/json_schema_form.dart';
import '../../../widgets/json_viewer.dart';

/// Lists every tool from `GET /v1/tools`. Selecting a tool renders
/// a JSON-schema form for its `input_schema`; pressing Invoke fires
/// `POST /v1/tools/:name` and shows the response.
///
/// The page is the introspection keystone — when the upstream agent
/// ships a new tool to Triton's registry, it appears here on next
/// refresh, no UI work needed.
class PlaygroundPage extends ConsumerStatefulWidget {
  const PlaygroundPage({super.key});

  @override
  ConsumerState<PlaygroundPage> createState() => _PlaygroundPageState();
}

class _PlaygroundPageState extends ConsumerState<PlaygroundPage> {
  ToolDescriptor? _selected;
  Map<String, dynamic> _args = const {};
  InvocationResult? _lastResult;
  bool _invoking = false;

  String? _a2uiVersion;

  Future<void> _invoke() async {
    final tool = _selected;
    if (tool == null) return;
    setState(() {
      _invoking = true;
      _lastResult = null;
    });
    final client = ref.read(restClientProvider);
    final r = await client.invoke(
      tool.name,
      _args,
      a2uiVersion: tool.returnsA2ui ? _a2uiVersion : null,
    );
    if (!mounted) return;
    setState(() {
      _lastResult = r;
      _invoking = false;
    });
  }

  void _onAcceptChanged(String? v) {
    setState(() => _a2uiVersion = v);
  }

  @override
  Widget build(BuildContext context) {
    final tools = ref.watch(toolsProvider);
    return Scaffold(
      appBar: AppBar(
        title: const Text('Playground'),
        actions: [
          IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh tool list',
            onPressed: () => ref.invalidate(toolsProvider),
          ),
        ],
      ),
      body: tools.when(
        data: (list) => _PlaygroundBody(
          tools: list,
          selected: _selected,
          onSelect: (t) => setState(() {
            _selected = t;
            _args = const {};
            _lastResult = null;
          }),
          onArgsChanged: (v) => _args = v,
          onInvoke: _invoke,
          invoking: _invoking,
          result: _lastResult,
          a2uiVersion: _a2uiVersion,
          onAcceptChanged: _onAcceptChanged,
        ),
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
      ),
    );
  }
}

class _PlaygroundBody extends StatelessWidget {
  const _PlaygroundBody({
    required this.tools,
    required this.selected,
    required this.onSelect,
    required this.onArgsChanged,
    required this.onInvoke,
    required this.invoking,
    required this.result,
    required this.a2uiVersion,
    required this.onAcceptChanged,
  });

  final List<ToolDescriptor> tools;
  final ToolDescriptor? selected;
  final ValueChanged<ToolDescriptor> onSelect;
  final ValueChanged<Map<String, dynamic>> onArgsChanged;
  final VoidCallback onInvoke;
  final bool invoking;
  final InvocationResult? result;
  final String? a2uiVersion;
  final ValueChanged<String?> onAcceptChanged;

  @override
  Widget build(BuildContext context) {
    if (tools.isEmpty) {
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
      children: [
        SizedBox(
          width: 260,
          child: ListView.builder(
            itemCount: tools.length,
            itemBuilder: (context, i) {
              final t = tools[i];
              return ListTile(
                title: Text(t.name),
                subtitle: t.returnsA2ui
                    ? const Text('returns A2UI',
                        style: TextStyle(fontSize: 11))
                    : null,
                selected: selected?.name == t.name,
                onTap: () => onSelect(t),
              );
            },
          ),
        ),
        const VerticalDivider(width: 1),
        Expanded(
          child: selected == null
              ? const Center(child: Text('Pick a tool'))
              : _ToolDetail(
                  tool: selected!,
                  onArgsChanged: onArgsChanged,
                  onInvoke: onInvoke,
                  invoking: invoking,
                  result: result,
                  a2uiVersion: a2uiVersion,
                  onAcceptChanged: onAcceptChanged,
                ),
        ),
      ],
    );
  }
}

class _ToolDetail extends StatelessWidget {
  const _ToolDetail({
    required this.tool,
    required this.onArgsChanged,
    required this.onInvoke,
    required this.invoking,
    required this.result,
    required this.a2uiVersion,
    required this.onAcceptChanged,
  });

  final ToolDescriptor tool;
  final ValueChanged<Map<String, dynamic>> onArgsChanged;
  final VoidCallback onInvoke;
  final bool invoking;
  final InvocationResult? result;
  final String? a2uiVersion;
  final ValueChanged<String?> onAcceptChanged;

  @override
  Widget build(BuildContext context) {
    final showA2uiTab =
        tool.returnsA2ui && result != null && a2uiVersion != null;
    return SingleChildScrollView(
      padding: const EdgeInsets.all(24),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(tool.name, style: Theme.of(context).textTheme.headlineSmall),
          const SizedBox(height: 4),
          if (tool.returnsA2ui)
            Chip(
              label: const Text('returns A2UI'),
              visualDensity: VisualDensity.compact,
              avatar: const Icon(Icons.brush, size: 16),
            ),
          const SizedBox(height: 16),
          Text('Arguments', style: Theme.of(context).textTheme.titleMedium),
          const SizedBox(height: 8),
          JsonSchemaForm(
            schema: tool.inputSchema,
            onChanged: onArgsChanged,
          ),
          if (tool.returnsA2ui) ...[
            const SizedBox(height: 12),
            Row(
              children: [
                const Text('Accept:'),
                const SizedBox(width: 12),
                SegmentedButton<String?>(
                  segments: const [
                    ButtonSegment(value: null, label: Text('JSON')),
                    ButtonSegment(value: '0.8', label: Text('A2UI v0.8')),
                    ButtonSegment(value: '0.9', label: Text('A2UI v0.9')),
                  ],
                  selected: {a2uiVersion},
                  onSelectionChanged: (s) => onAcceptChanged(s.first),
                ),
              ],
            ),
          ],
          const SizedBox(height: 16),
          Row(
            children: [
              FilledButton.icon(
                icon: invoking
                    ? const SizedBox(
                        width: 16,
                        height: 16,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      )
                    : const Icon(Icons.send),
                label: const Text('Invoke'),
                onPressed: invoking ? null : onInvoke,
              ),
              const Spacer(),
              if (result?.traceId != null)
                SelectableText('trace ${result!.traceId}',
                    style: Theme.of(context).textTheme.bodySmall),
            ],
          ),
          const SizedBox(height: 24),
          if (result != null) ...[
            Text(
              'Response (${result!.statusCode} • '
              '${result!.elapsed.inMilliseconds}ms)',
              style: Theme.of(context).textTheme.titleMedium,
            ),
            const SizedBox(height: 8),
            if (result!.error != null)
              Card(
                color: Theme.of(context).colorScheme.errorContainer,
                child: Padding(
                  padding: const EdgeInsets.all(12),
                  child: Text(result!.error!),
                ),
              ),
            if (showA2uiTab)
              Card(
                child: Padding(
                  padding: const EdgeInsets.all(12),
                  child: A2UIRenderer(envelope: result!.raw),
                ),
              ),
            const SizedBox(height: 8),
            JsonViewer(result!.raw),
          ],
        ],
      ),
    );
  }
}
