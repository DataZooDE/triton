import 'package:flutter/material.dart';

import '../../../widgets/json_viewer.dart';

/// Audit explainer. Triton emits two JSON-lines records per
/// invocation to stdout — the substrate ships them; there's no
/// retrieval endpoint to call yet. This page documents the shape
/// the operator should expect to see in the log shipper, and gets
/// a live data wire once Triton exposes audit retrieval (post-v0.2).
class AuditPage extends StatelessWidget {
  const AuditPage({super.key});

  static const _acceptedSample = {
    'kind': 'audit',
    'phase': 'accepted',
    'tool': 'echo',
    'adapter': 'rest',
    'env': 'nonprod',
    'sub': 'agent-frontdesk',
    'tenant': 't-1',
    'trace_id': '01HXYZ...',
    'ts': '2026-05-24T18:32:11Z',
  };

  static const _completedSample = {
    'kind': 'audit',
    'phase': 'completed',
    'tool': 'echo',
    'adapter': 'rest',
    'env': 'nonprod',
    'sub': 'agent-frontdesk',
    'tenant': 't-1',
    'trace_id': '01HXYZ...',
    'status': 'ok',
    'latency_ms': 17,
    'ts': '2026-05-24T18:32:11Z',
  };

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(title: const Text('Audit')),
        body: ListView(
          padding: const EdgeInsets.all(24),
          children: [
            Text('Two records per invocation',
                style: Theme.of(context).textTheme.titleLarge),
            const SizedBox(height: 8),
            const Text(
              "Triton's dispatcher emits a JSON-lines record at "
              '`accepted` (boundary cleared, identity verified) and a '
              'second at `completed` (tool returned). They share a '
              '`trace_id` so a log shipper can stitch the two ends '
              'of an invocation together — including failures, '
              'circuit-breaker rejections, and upstream timeouts.',
            ),
            const SizedBox(height: 24),
            _section(context, 'accepted (phase=accepted)', _acceptedSample),
            const SizedBox(height: 16),
            _section(context, 'completed (phase=completed)', _completedSample),
            const SizedBox(height: 24),
            Card(
              color: Theme.of(context).colorScheme.surfaceContainerHigh,
              child: const Padding(
                padding: EdgeInsets.all(16),
                child: Text(
                  'Once Triton exposes a tailnet-only audit retrieval '
                  'endpoint, this page renders live records here. Until '
                  'then, look in the substrate log shipper for the '
                  '`kind=audit` JSON-lines stream.',
                ),
              ),
            ),
          ],
        ),
      );

  Widget _section(BuildContext context, String label, Object sample) => Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(label, style: Theme.of(context).textTheme.titleMedium),
          const SizedBox(height: 6),
          JsonViewer(sample),
        ],
      );
}
