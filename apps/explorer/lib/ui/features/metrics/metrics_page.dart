import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../providers/api_provider.dart';
import '../../../widgets/panel_help.dart';
import '../../../widgets/prometheus_parser.dart';

/// Live Prometheus view backed by `GET /v1/metrics`. The substrate
/// scrapes the unauthenticated tailnet-only `:9090` listener; this
/// page reads the same registry through the REST adapter so
/// browser-side rendering works without bridging tags.
class MetricsPage extends ConsumerWidget {
  const MetricsPage({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final body = ref.watch(metricsProvider);
    return Scaffold(
      appBar: AppBar(
        title: const Text('Metrics'),
        actions: [
          IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: () => ref.invalidate(metricsProvider),
          ),
        ],
      ),
      body: Column(
        children: [
          const PanelHelp(
            what: 'Live Prometheus metrics from /v1/metrics — dispatch '
                'counters by tool/protocol/result and latency histograms.',
            how: 'Press Refresh to re-scrape. Metrics are grouped by family; '
                'the substrate scrapes the same registry on the tailnet-only '
                ':9090 listener.',
          ),
          Expanded(
            child: body.when(
              loading: () =>
                  const Center(child: CircularProgressIndicator()),
              error: (e, _) => Padding(
                padding: const EdgeInsets.all(24),
                child: Card(
                  color: Theme.of(context).colorScheme.errorContainer,
                  child: Padding(
                    padding: const EdgeInsets.all(16),
                    child: Text('Could not load /v1/metrics: $e'),
                  ),
                ),
              ),
              data: (text) {
          final exp = parsePrometheus(text);
          if (exp.families.isEmpty) {
            return const Center(
              child: Padding(
                padding: EdgeInsets.all(24),
                child: Text('Prometheus body parsed empty.'),
              ),
            );
          }
          return ListView.separated(
            padding: const EdgeInsets.all(16),
            itemCount: exp.families.length,
            separatorBuilder: (_, _) => const SizedBox(height: 12),
            itemBuilder: (context, i) => _FamilyCard(family: exp.families[i]),
          );
                },
              ),
            ),
          ],
        ),
      );
  }
}

class _FamilyCard extends StatelessWidget {
  const _FamilyCard({required this.family});
  final PromMetricFamily family;

  @override
  Widget build(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(_iconFor(family.type), size: 18),
                const SizedBox(width: 8),
                Text(family.name,
                    style: const TextStyle(
                        fontFamily: 'monospace', fontSize: 14)),
                const SizedBox(width: 8),
                Chip(
                  label: Text(family.type),
                  visualDensity: VisualDensity.compact,
                ),
              ],
            ),
            if (family.help.isNotEmpty) ...[
              const SizedBox(height: 4),
              Text(
                family.help,
                style: Theme.of(context).textTheme.bodySmall?.copyWith(
                      color: Theme.of(context).colorScheme.onSurfaceVariant,
                    ),
              ),
            ],
            const SizedBox(height: 8),
            _SamplesTable(samples: family.samples),
          ],
        ),
      ),
    );
  }

  IconData _iconFor(String type) {
    switch (type) {
      case 'counter':
        return Icons.trending_up;
      case 'gauge':
        return Icons.speed;
      case 'histogram':
      case 'summary':
        return Icons.bar_chart;
      default:
        return Icons.numbers;
    }
  }
}

class _SamplesTable extends StatelessWidget {
  const _SamplesTable({required this.samples});
  final List<PromSample> samples;

  @override
  Widget build(BuildContext context) {
    if (samples.isEmpty) {
      return Text('— no samples —',
          style: Theme.of(context).textTheme.bodySmall);
    }
    // Sort by labels for stable display.
    final sorted = [...samples]
      ..sort((a, b) => a.labels.toString().compareTo(b.labels.toString()));
    final labelKeys = <String>{
      for (final s in sorted) ...s.labels.keys,
    }.toList()
      ..sort();
    return Table(
      defaultColumnWidth: const IntrinsicColumnWidth(),
      columnWidths: const {
        0: FlexColumnWidth(2),
      },
      border: TableBorder.symmetric(
        inside: BorderSide(
            color: Theme.of(context).colorScheme.outlineVariant, width: 0.5),
      ),
      children: [
        TableRow(children: [
          for (final k in labelKeys) _cell(context, k, header: true),
          _cell(context, 'value', header: true, alignRight: true),
        ]),
        for (final s in sorted)
          TableRow(children: [
            for (final k in labelKeys) _cell(context, s.labels[k] ?? '—'),
            _cell(context, _formatValue(s.value), alignRight: true),
          ]),
      ],
    );
  }

  Widget _cell(BuildContext context, String text,
      {bool header = false, bool alignRight = false}) {
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
      child: Text(
        text,
        textAlign: alignRight ? TextAlign.right : TextAlign.left,
        style: TextStyle(
          fontFamily: 'monospace',
          fontSize: 12,
          fontWeight: header ? FontWeight.w600 : FontWeight.normal,
          color: header
              ? Theme.of(context).colorScheme.onSurfaceVariant
              : null,
        ),
      ),
    );
  }

  String _formatValue(double v) {
    if (v == v.truncateToDouble()) return v.toInt().toString();
    return v.toString();
  }
}
