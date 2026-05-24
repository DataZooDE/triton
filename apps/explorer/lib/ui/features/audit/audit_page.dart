import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../providers/api_provider.dart';
import '../../../widgets/json_viewer.dart';

/// Live audit tail. Reads the dispatcher's in-process ring buffer
/// over `GET /v1/audit`. Filter by trace_id; tap any row to expand
/// the full JSON record. Refreshes manually so the operator stays
/// in control of when the data shifts.
class AuditPage extends ConsumerStatefulWidget {
  const AuditPage({super.key});

  @override
  ConsumerState<AuditPage> createState() => _AuditPageState();
}

class _AuditPageState extends ConsumerState<AuditPage> {
  final _filterCtrl = TextEditingController();
  String? _traceFilter;
  int _limit = 50;
  int? _expandedIndex;

  @override
  void dispose() {
    _filterCtrl.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final q = AuditQuery(limit: _limit, traceId: _traceFilter);
    final entries = ref.watch(auditTailProvider(q));
    return Scaffold(
      appBar: AppBar(
        title: const Text('Audit'),
        actions: [
          IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: () => ref.invalidate(auditTailProvider(q)),
          ),
        ],
      ),
      body: Column(
        children: [
          Padding(
            padding: const EdgeInsets.all(16),
            child: Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: _filterCtrl,
                    decoration: const InputDecoration(
                      labelText: 'Filter by trace_id (exact match)',
                      border: OutlineInputBorder(),
                      isDense: true,
                    ),
                    onSubmitted: (v) {
                      setState(() {
                        _traceFilter = v.isEmpty ? null : v;
                        _expandedIndex = null;
                      });
                    },
                  ),
                ),
                const SizedBox(width: 12),
                DropdownButton<int>(
                  value: _limit,
                  onChanged: (v) => setState(() => _limit = v ?? 50),
                  items: const [
                    DropdownMenuItem(value: 20, child: Text('Last 20')),
                    DropdownMenuItem(value: 50, child: Text('Last 50')),
                    DropdownMenuItem(value: 200, child: Text('Last 200')),
                    DropdownMenuItem(value: 500, child: Text('Last 500')),
                  ],
                ),
              ],
            ),
          ),
          const Divider(height: 1),
          Expanded(
            child: entries.when(
              loading: () => const Center(child: CircularProgressIndicator()),
              error: (e, _) => Padding(
                padding: const EdgeInsets.all(24),
                child: Text('Could not load /v1/audit: $e'),
              ),
              data: (rows) {
                if (rows.isEmpty) {
                  return const Center(
                    child: Padding(
                      padding: EdgeInsets.all(24),
                      child: Text(
                        'No audit entries match this filter.\n\n'
                        'The ring buffer is in-process — entries clear on '
                        'restart. The substrate log shipper holds the full '
                        'history.',
                        textAlign: TextAlign.center,
                      ),
                    ),
                  );
                }
                return ListView.separated(
                  itemCount: rows.length,
                  separatorBuilder: (_, _) => const Divider(height: 1),
                  itemBuilder: (context, i) {
                    final e = rows[i];
                    return _AuditTile(
                      entry: e,
                      expanded: _expandedIndex == i,
                      onTap: () => setState(
                        () => _expandedIndex = _expandedIndex == i ? null : i,
                      ),
                    );
                  },
                );
              },
            ),
          ),
        ],
      ),
    );
  }
}

class _AuditTile extends StatelessWidget {
  const _AuditTile({
    required this.entry,
    required this.expanded,
    required this.onTap,
  });

  final Map<String, dynamic> entry;
  final bool expanded;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final phase = (entry['phase'] as String?) ?? '?';
    final tool = (entry['tool'] as String?) ?? '-';
    final protocol = (entry['protocol'] as String?) ?? '-';
    final result = (entry['result'] as String?) ?? '?';
    final latency = entry['latency_ms'] ?? '-';
    final traceId = (entry['trace_id'] as String?) ?? '-';
    final when = (entry['when'] as String?) ?? '-';
    final color = _phaseColor(context, phase);
    return InkWell(
      onTap: onTap,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding:
                const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
            child: Row(
              children: [
                Container(
                  width: 8,
                  height: 8,
                  decoration: BoxDecoration(
                      color: color, shape: BoxShape.circle),
                ),
                const SizedBox(width: 12),
                SizedBox(
                  width: 72,
                  child: Text(phase,
                      style: Theme.of(context).textTheme.bodySmall),
                ),
                SizedBox(
                  width: 100,
                  child: Text('$protocol → $tool',
                      style: Theme.of(context).textTheme.bodyMedium,
                      overflow: TextOverflow.ellipsis),
                ),
                Expanded(
                  child: Text(
                    traceId,
                    style: const TextStyle(
                      fontFamily: 'monospace',
                      fontSize: 12,
                    ),
                    overflow: TextOverflow.ellipsis,
                  ),
                ),
                const SizedBox(width: 8),
                SizedBox(
                  width: 70,
                  child: Text('${latency}ms',
                      textAlign: TextAlign.right,
                      style: Theme.of(context).textTheme.bodySmall),
                ),
                const SizedBox(width: 8),
                SizedBox(
                  width: 70,
                  child: Text(result,
                      textAlign: TextAlign.right,
                      style: Theme.of(context).textTheme.bodySmall),
                ),
                const SizedBox(width: 8),
                SizedBox(
                  width: 180,
                  child: Text(when,
                      textAlign: TextAlign.right,
                      style: const TextStyle(
                          fontFamily: 'monospace', fontSize: 11),
                      overflow: TextOverflow.ellipsis),
                ),
              ],
            ),
          ),
          if (expanded)
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 0, 16, 16),
              child: JsonViewer(entry),
            ),
        ],
      ),
    );
  }

  Color _phaseColor(BuildContext context, String phase) {
    final cs = Theme.of(context).colorScheme;
    switch (phase) {
      case 'dispatch':
        return cs.secondary;
      case 'rejected':
        return cs.error;
      case 'post':
        return cs.tertiary;
      case 'upstream':
        return cs.primary;
      default:
        return cs.outline;
    }
  }
}
