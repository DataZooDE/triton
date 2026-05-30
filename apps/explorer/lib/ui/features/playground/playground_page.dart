import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/models.dart';
import '../../../providers/api_provider.dart';
import '../../../widgets/a2ui/a2ui_renderer.dart';
import '../../../widgets/chat_channel_previews.dart';
import '../../../widgets/json_schema_form.dart';
import '../../../widgets/json_viewer.dart';
import '../../../widgets/panel_help.dart';

/// Per-tool "what is this / what to do" copy for the bundled demo
/// tools. Unknown tools fall back to a hint derived from the tool's
/// schema + `returns_a2ui` flag, so a tool shipped later still gets a
/// sensible banner with no UI work.
({String what, String how}) toolGuide(ToolDescriptor t) {
  switch (t.name) {
    case 'echo':
      return (
        what: 'The simplest tool — returns whatever you send it. Proves a '
            'request reaches the dispatcher and an audit line is emitted.',
        how: 'Type a message and press Invoke; you get it back with a '
            'trace_id you can look up on the Audit page.',
      );
    case 'narrate':
      return (
        what: 'Builds a small A2UI surface (a line of text, a narration, and '
            'a Refresh button) from a subject — the A2UI builder + the '
            'interactive round-trip.',
        how: 'Enter a subject, pick A2UI v0.9, and Invoke. Then click the '
            'Refresh button in the rendered surface to re-invoke the tool.',
      );
    case 'demo_panel':
      return (
        what: 'Renders the full A2UI component vocabulary at once — text, '
            'narration, a dashboard, a selection, a form, and a button. The '
            'reference surface.',
        how: 'Pick A2UI v0.8 or v0.9 and Invoke. Click a selection chip or '
            'the button to watch the surface re-invoke a tool (the stateless '
            'round-trip).',
      );
    case 'delay':
      return (
        what: 'Sleeps for the given number of milliseconds, then returns. '
            'Used to exercise timeouts and graceful shutdown (SIGTERM drain).',
        how: 'Enter a value in ms (0–5000) and Invoke; the response comes '
            'back after that delay.',
      );
    case 'form_only_demo':
      return (
        what: 'Returns a single-field A2UI form — models a chat modal (e.g. '
            'Discord). Demonstrates form submission.',
        how: 'Pick A2UI, Invoke, fill the field, and press submit; that '
            'dispatches submitted_form with your values.',
      );
    case 'form_only_demo_multi':
      return (
        what: 'Like form_only_demo but with several fields — models a '
            'multi-field chat form (e.g. Telegram).',
        how: 'Pick A2UI, Invoke, fill the fields, and submit; the values are '
            'dispatched to submitted_form.',
      );
    case 'submitted_form':
      return (
        what: 'The target tool a form submits to — it echoes back the fields '
            'it received.',
        how: 'Usually invoked for you when you submit a form in demo_panel or '
            'form_only_demo. You can also call it directly with arbitrary JSON '
            'args to see the echo.',
      );
    case 'empty_surface':
      return (
        what: 'Returns an A2UI surface with no components — exercises the '
            'empty / fallback render path.',
        how: 'Pick an A2UI version and Invoke to see how the renderer handles '
            'an empty surface.',
      );
    default:
      final hasArgs =
          ((t.inputSchema['properties'] as Map?)?.isNotEmpty) ?? false;
      final how = StringBuffer();
      if (hasArgs) how.write('Fill in the arguments, then ');
      how.write(hasArgs ? 'press Invoke' : 'press Invoke');
      if (t.returnsA2ui) {
        how.write(' (choose an A2UI version to render the surface, or JSON '
            'for the raw result)');
      }
      how.write('; the response shows status, latency, trace_id and raw JSON.');
      return (
        what: 'A tool registered on this Triton gateway, discovered live from '
            '/v1/tools.',
        how: how.toString(),
      );
  }
}

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

  /// Invoked when the user interacts with a rendered A2UI surface —
  /// taps a Button, picks a Selection, or submits a Form. This is the
  /// stateless re-invocation loop (FR-D-3): the action carries the
  /// tool + args to fire next. We keep [_a2uiVersion] so the response
  /// re-renders interactively, letting chained round-trips work (e.g.
  /// narrate's Refresh button → narrate again).
  Future<void> _onA2uiAction(String tool, Map<String, dynamic> args) async {
    setState(() {
      _invoking = true;
      _lastResult = null;
    });
    final client = ref.read(restClientProvider);
    final r = await client.invoke(tool, args, a2uiVersion: _a2uiVersion);
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
          onA2uiAction: _onA2uiAction,
          args: _args,
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
    required this.onA2uiAction,
    required this.args,
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
  final void Function(String tool, Map<String, dynamic> args) onA2uiAction;
  final Map<String, dynamic> args;

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
      // Stretch so the detail pane's scroll view fills the height and
      // top-aligns its content, instead of the default center which
      // floats short tool panels in the vertical middle.
      crossAxisAlignment: CrossAxisAlignment.stretch,
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
                  onA2uiAction: onA2uiAction,
                  args: args,
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
    required this.onA2uiAction,
    required this.args,
  });

  final ToolDescriptor tool;
  final ValueChanged<Map<String, dynamic>> onArgsChanged;
  final VoidCallback onInvoke;
  final bool invoking;
  final InvocationResult? result;
  final String? a2uiVersion;
  final ValueChanged<String?> onAcceptChanged;
  final void Function(String tool, Map<String, dynamic> args) onA2uiAction;
  final Map<String, dynamic> args;

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
          Builder(builder: (context) {
            final guide = toolGuide(tool);
            return PanelHelp(
              what: guide.what,
              how: guide.how,
              margin: const EdgeInsets.only(top: 12),
            );
          }),
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
            if (showA2uiTab) ...[
              Text('Rendered surface — MCP App / web client',
                  style: Theme.of(context).textTheme.titleSmall),
              const SizedBox(height: 6),
              Card(
                child: Padding(
                  padding: const EdgeInsets.all(12),
                  child: A2UIRenderer(
                    envelope: result!.raw,
                    version: a2uiVersion,
                    onAction: onA2uiAction,
                  ),
                ),
              ),
            ],
            // The same surface, mapped onto each chat platform. Only
            // meaningful for tools that emit an A2UI surface. Placed
            // above the raw envelope so it isn't buried under the JSON.
            if (tool.returnsA2ui && result!.ok) ...[
              const SizedBox(height: 24),
              const Divider(),
              const SizedBox(height: 12),
              ChatChannelPreviews(toolName: tool.name, args: args),
            ],
            const SizedBox(height: 24),
            const Divider(),
            const SizedBox(height: 8),
            Text('Raw envelope',
                style: Theme.of(context).textTheme.titleSmall),
            const SizedBox(height: 6),
            JsonViewer(result!.raw),
          ],
        ],
      ),
    );
  }
}
