import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../api/friendly_error.dart';
import '../../providers/api_provider.dart';
import '../json_viewer.dart';

/// The per-message **trace sidebar**: everything that happened for one chat
/// turn, opened from the bubble's inspect button. Two sections:
///
///   * **Tools & data touched** — the agent's own per-turn tool trace
///     (`_meta.tool_trace`), already in hand on the [InvocationResult]: which
///     tools ran and which Escurel page/query/account each touched, read or
///     write, in call order. This is the "which function ran, which data was
///     touched per message" view.
///   * **Gateway timeline** — Triton's audit phases for the turn's `trace_id`,
///     fetched from `GET /v1/trace/{id}` (inbound → dispatch → upstream →
///     post), plus captured bodies in dev builds. This is what the standalone
///     Trace tab used to show; it now lives per-message here.
class MessageTraceSidebar extends ConsumerWidget {
  const MessageTraceSidebar({
    super.key,
    required this.traceId,
    required this.toolTrace,
    required this.onClose,
  });

  final String? traceId;
  final List<Map<String, dynamic>>? toolTrace;
  final VoidCallback onClose;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final cs = Theme.of(context).colorScheme;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 14, 8, 8),
          child: Row(
            children: [
              Icon(Icons.travel_explore, size: 18, color: cs.primary),
              const SizedBox(width: 8),
              Text(
                'Message trace',
                style: Theme.of(context).textTheme.titleMedium,
              ),
              const Spacer(),
              IconButton(
                key: const Key('trace-sidebar-close'),
                icon: const Icon(Icons.close, size: 18),
                tooltip: 'Close',
                onPressed: onClose,
              ),
            ],
          ),
        ),
        if (traceId != null)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
            child: SelectableText(
              traceId!,
              style: Theme.of(context).textTheme.bodySmall?.copyWith(
                fontFamily: 'monospace',
                color: cs.onSurfaceVariant,
              ),
            ),
          ),
        const Divider(height: 1),
        Expanded(
          child: ListView(
            padding: const EdgeInsets.all(16),
            children: [
              _sectionTitle(context, 'Tools & data touched'),
              const SizedBox(height: 6),
              ToolTraceList(calls: toolTrace),
              const SizedBox(height: 20),
              _sectionTitle(context, 'Gateway timeline'),
              const SizedBox(height: 6),
              _gateway(context, ref),
            ],
          ),
        ),
      ],
    );
  }

  Widget _sectionTitle(BuildContext context, String text) => Text(
    text,
    style: Theme.of(
      context,
    ).textTheme.titleSmall?.copyWith(fontWeight: FontWeight.w600),
  );

  Widget _gateway(BuildContext context, WidgetRef ref) {
    final id = traceId;
    if (id == null || id.isEmpty) {
      return Text(
        'No trace_id on this reply.',
        style: Theme.of(context).textTheme.bodySmall,
      );
    }
    return FutureBuilder<Map<String, dynamic>>(
      future: ref.read(restClientProvider).trace(id),
      builder: (context, snap) {
        if (snap.connectionState != ConnectionState.done) {
          return const Padding(
            padding: EdgeInsets.all(12),
            child: Center(
              child: SizedBox(
                width: 18,
                height: 18,
                child: CircularProgressIndicator(strokeWidth: 2),
              ),
            ),
          );
        }
        if (snap.hasError) {
          return Text(
            friendlyApiError('Could not load trace', snap.error!),
            style: Theme.of(context).textTheme.bodySmall,
          );
        }
        final data = snap.data ?? const {};
        final entries =
            (data['entries'] as List?)?.cast<Map<String, dynamic>>() ??
            const [];
        final bodies =
            (data['bodies'] as List?)?.cast<Map<String, dynamic>>() ?? const [];
        if (entries.isEmpty) {
          return Text(
            'No audit entries for this trace_id.',
            style: Theme.of(context).textTheme.bodySmall,
          );
        }
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            for (final e in entries) PhaseCard(entry: e),
            if (bodies.isNotEmpty) ...[
              const SizedBox(height: 12),
              Text(
                'Captured bodies (dev)',
                style: Theme.of(context).textTheme.titleSmall,
              ),
              const SizedBox(height: 6),
              for (final b in bodies) BodyTile(body: b),
            ],
          ],
        );
      },
    );
  }
}

/// The agent's per-turn tool trace as a compact list — one row per call:
/// a read/write glyph, the tool name, the Escurel target it touched, and the
/// call's latency. Empty state when the reply carried none.
class ToolTraceList extends StatelessWidget {
  const ToolTraceList({super.key, required this.calls});

  final List<Map<String, dynamic>>? calls;

  @override
  Widget build(BuildContext context) {
    final list = calls;
    if (list == null || list.isEmpty) {
      return Text(
        'No tool calls recorded for this reply.',
        style: Theme.of(context).textTheme.bodySmall,
      );
    }
    return Column(
      key: const Key('tool-trace-list'),
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [for (final c in list) _ToolCallRow(call: c)],
    );
  }
}

class _ToolCallRow extends StatelessWidget {
  const _ToolCallRow({required this.call});
  final Map<String, dynamic> call;

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final tool = (call['tool'] as String?) ?? '?';
    final target = call['target'] as String?;
    final write = call['verb'] == 'write';
    final ms = (call['ms'] as num?)?.toInt();
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 5),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Tooltip(
            message: write ? 'write' : 'read',
            child: Icon(
              write ? Icons.edit_note : Icons.search,
              size: 15,
              color: write ? cs.tertiary : cs.primary,
            ),
          ),
          const SizedBox(width: 8),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  tool,
                  style: const TextStyle(
                    fontFamily: 'monospace',
                    fontWeight: FontWeight.w600,
                    fontSize: 12.5,
                  ),
                ),
                if (target != null && target.isNotEmpty)
                  Text(
                    target,
                    style: Theme.of(context).textTheme.bodySmall?.copyWith(
                      color: cs.onSurfaceVariant,
                    ),
                  ),
              ],
            ),
          ),
          if (ms != null && ms > 0) ...[
            const SizedBox(width: 8),
            Text('${ms}ms', style: Theme.of(context).textTheme.bodySmall),
          ],
        ],
      ),
    );
  }
}

/// One gateway audit phase (inbound / dispatch / upstream / post) — status
/// glyph, phase name, protocol, HTTP status, and a latency bar. Extracted from
/// the retired Trace page so the per-message sidebar reuses it verbatim.
class PhaseCard extends StatelessWidget {
  const PhaseCard({super.key, required this.entry});
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
        padding: const EdgeInsets.all(10),
        child: Row(
          children: [
            Icon(
              ok ? Icons.check_circle : Icons.error,
              size: 16,
              color: ok ? cs.primary : cs.error,
            ),
            const SizedBox(width: 8),
            SizedBox(
              width: 74,
              child: Text(
                phase,
                style: const TextStyle(
                  fontWeight: FontWeight.w600,
                  fontSize: 12.5,
                ),
              ),
            ),
            if (protocol.isNotEmpty)
              Chip(
                label: Text(protocol),
                visualDensity: VisualDensity.compact,
                labelStyle: const TextStyle(fontSize: 11),
              ),
            const SizedBox(width: 6),
            Text('$status', style: Theme.of(context).textTheme.bodySmall),
            const Spacer(),
            Container(
              width: (latency.clamp(1, 120)).toDouble(),
              height: 7,
              decoration: BoxDecoration(
                color: cs.secondary,
                borderRadius: BorderRadius.circular(4),
              ),
            ),
            const SizedBox(width: 6),
            Text('${latency}ms', style: Theme.of(context).textTheme.bodySmall),
          ],
        ),
      ),
    );
  }
}

/// A captured request/response body (dev `capture` builds only), expandable to
/// the pretty-printed JSON. Extracted from the retired Trace page.
class BodyTile extends StatelessWidget {
  const BodyTile({super.key, required this.body});
  final Map<String, dynamic> body;

  @override
  Widget build(BuildContext context) {
    final protocol = (body['protocol'] as String?) ?? '';
    final direction = (body['direction'] as String?) ?? '';
    final payload = body['body'];
    return ExpansionTile(
      title: Text('$protocol · $direction'),
      tilePadding: EdgeInsets.zero,
      childrenPadding: const EdgeInsets.only(bottom: 12),
      expandedCrossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        JsonViewer(
          payload is Map<String, dynamic> ? payload : {'value': payload},
        ),
      ],
    );
  }
}
