import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../providers/api_provider.dart';
import '../../../widgets/json_viewer.dart';
import '../../../widgets/panel_help.dart';
import '../../../api/friendly_error.dart';

/// The **Trace** view (issue #75): one communication as a timeline,
/// correlated by `trace_id`. Reads `GET /v1/trace/{id}` — the ordered
/// audit phases (inbound → dispatch → upstream → post) and, when Triton
/// was built with the dev `capture` feature, the request/response/surface
/// bodies at each hop. Pre-fills from the Console's last call.
class TracePage extends ConsumerStatefulWidget {
  const TracePage({super.key});

  @override
  ConsumerState<TracePage> createState() => _TracePageState();
}

class _TracePageState extends ConsumerState<TracePage> {
  final _ctrl = TextEditingController();
  Future<Map<String, dynamic>>? _future;
  bool _seeded = false;

  @override
  void dispose() {
    _ctrl.dispose();
    super.dispose();
  }

  void _load() {
    final id = _ctrl.text.trim();
    if (id.isEmpty) return;
    setState(() => _future = ref.read(restClientProvider).trace(id));
  }

  @override
  Widget build(BuildContext context) {
    // Pre-fill from the Console's last invoked trace_id (once).
    final selected = ref.watch(selectedTraceProvider);
    if (!_seeded && selected != null && selected.isNotEmpty) {
      _ctrl.text = selected;
      _seeded = true;
    }

    return Scaffold(
      appBar: AppBar(title: const Text('Trace')),
      body: Column(
        children: [
          const PanelHelp(
            what:
                'One communication as a timeline, correlated by trace_id: '
                'the inbound protocol → dispatch → upstream/tool → post '
                'phases, with latency and status — and, in dev builds, the '
                'request/response/surface bodies at each hop.',
            how:
                "Paste a trace_id (or send a call on the Console — it's "
                'pre-filled here) and press Load. Bodies appear only when '
                'Triton was built with the dev `capture` feature.',
          ),
          Padding(
            padding: const EdgeInsets.all(16),
            child: Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: _ctrl,
                    decoration: const InputDecoration(
                      isDense: true,
                      border: OutlineInputBorder(),
                      labelText: 'trace_id',
                    ),
                    onSubmitted: (_) => _load(),
                  ),
                ),
                const SizedBox(width: 12),
                FilledButton.icon(
                  icon: const Icon(Icons.timeline),
                  label: const Text('Load'),
                  onPressed: _load,
                ),
              ],
            ),
          ),
          const Divider(height: 1),
          Expanded(child: _body(context)),
        ],
      ),
    );
  }

  Widget _body(BuildContext context) {
    final future = _future;
    if (future == null) {
      return const Center(child: Text('Enter a trace_id and press Load.'));
    }
    return FutureBuilder<Map<String, dynamic>>(
      future: future,
      builder: (context, snap) {
        if (snap.connectionState != ConnectionState.done) {
          return const Center(child: CircularProgressIndicator());
        }
        if (snap.hasError) {
          return Padding(
            padding: const EdgeInsets.all(24),
            child: Text(friendlyApiError('Could not load trace', snap.error!)),
          );
        }
        final data = snap.data ?? const {};
        final entries =
            (data['entries'] as List?)?.cast<Map<String, dynamic>>() ??
            const [];
        final bodies =
            (data['bodies'] as List?)?.cast<Map<String, dynamic>>() ?? const [];
        if (entries.isEmpty) {
          return const Center(
            child: Text('No audit entries for this trace_id.'),
          );
        }
        return ListView(
          padding: const EdgeInsets.all(16),
          children: [
            Text('Timeline', style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            for (final e in entries) _PhaseCard(entry: e),
            if (bodies.isNotEmpty) ...[
              const SizedBox(height: 24),
              Text(
                'Captured bodies (dev)',
                style: Theme.of(context).textTheme.titleMedium,
              ),
              const SizedBox(height: 8),
              for (final b in bodies) _BodyTile(body: b),
            ],
          ],
        );
      },
    );
  }
}

class _PhaseCard extends StatelessWidget {
  const _PhaseCard({required this.entry});
  final Map<String, dynamic> entry;

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final phase = (entry['phase'] as String?) ?? '?';
    final protocol = (entry['protocol'] as String?) ?? '';
    final status = entry['status'];
    final latency = (entry['latency_ms'] as num?)?.toInt() ?? 0;
    final result = (entry['result'] as String?) ?? '';
    final ok = result == 'ok';
    return Card(
      margin: const EdgeInsets.only(bottom: 8),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Row(
          children: [
            Icon(
              ok ? Icons.check_circle : Icons.error,
              size: 18,
              color: ok ? cs.primary : cs.error,
            ),
            const SizedBox(width: 10),
            SizedBox(
              width: 96,
              child: Text(
                phase,
                style: const TextStyle(fontWeight: FontWeight.w600),
              ),
            ),
            Chip(label: Text(protocol), visualDensity: VisualDensity.compact),
            const SizedBox(width: 8),
            Text('$status', style: Theme.of(context).textTheme.bodySmall),
            const Spacer(),
            // Simple latency bar (caps at 200ms wide).
            Container(
              width: (latency.clamp(1, 200)).toDouble(),
              height: 8,
              decoration: BoxDecoration(
                color: cs.secondary,
                borderRadius: BorderRadius.circular(4),
              ),
            ),
            const SizedBox(width: 8),
            Text('${latency}ms', style: Theme.of(context).textTheme.bodySmall),
          ],
        ),
      ),
    );
  }
}

class _BodyTile extends StatelessWidget {
  const _BodyTile({required this.body});
  final Map<String, dynamic> body;

  @override
  Widget build(BuildContext context) {
    final protocol = (body['protocol'] as String?) ?? '';
    final direction = (body['direction'] as String?) ?? '';
    final payload = body['body'];
    return ExpansionTile(
      title: Text('$protocol · $direction'),
      childrenPadding: const EdgeInsets.fromLTRB(16, 0, 16, 12),
      expandedCrossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        JsonViewer(
          payload is Map<String, dynamic> ? payload : {'value': payload},
        ),
      ],
    );
  }
}
