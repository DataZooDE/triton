import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/runtime_config.dart';
import '../../../providers/api_provider.dart';
import '../../../providers/runtime_provider.dart';
import '../../../theme/app_theme.dart';

class DashboardPage extends ConsumerWidget {
  const DashboardPage({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final runtime = ref.watch(runtimeConfigProvider);
    final tools = ref.watch(toolsProvider);
    return Scaffold(
      appBar: AppBar(title: const Text('Triton')),
      body: SingleChildScrollView(
        padding: const EdgeInsets.all(24),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              'Welcome to Triton Explorer',
              style: Theme.of(context).textTheme.headlineSmall,
            ),
            const SizedBox(height: 8),
            Text(
              'Pick a feature from the rail. New tools shipped to Triton '
              'appear under Playground without UI work — the explorer reads '
              "Triton's /v1/tools surface directly.",
              style: Theme.of(context).textTheme.bodyMedium?.copyWith(
                    color: ExplorerTheme.onSurfaceVariant,
                  ),
            ),
            const SizedBox(height: 24),
            // Three live cards: runtime metadata, tool inventory,
            // and a static audit explainer that links to the Audit
            // page. Layout adapts to width.
            LayoutBuilder(
              builder: (context, constraints) {
                final wide = constraints.maxWidth >= 720;
                final cards = <Widget>[
                  Expanded(child: _RuntimeCard(runtime)),
                  const SizedBox(width: 16, height: 16),
                  Expanded(child: _ToolsCard(tools)),
                ];
                return wide
                    ? Row(crossAxisAlignment: CrossAxisAlignment.start, children: cards)
                    : Column(
                        crossAxisAlignment: CrossAxisAlignment.stretch,
                        children: cards
                            .whereType<Widget>()
                            .map((w) => w is Expanded ? w.child : w)
                            .toList(),
                      );
              },
            ),
            const SizedBox(height: 16),
            const _AuditPreviewCard(),
          ],
        ),
      ),
    );
  }
}

class _RuntimeCard extends StatelessWidget {
  const _RuntimeCard(this.runtime);
  final AsyncValue<RuntimeConfig> runtime;

  @override
  Widget build(BuildContext context) => Card(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: runtime.when(
            loading: () => const SizedBox(
              height: 120,
              child: Center(child: CircularProgressIndicator()),
            ),
            error: (e, _) => Text(
              'Could not reach Triton: $e\n\nSet the base URL in Settings.',
              style: const TextStyle(color: ExplorerTheme.onErrorContainer),
            ),
            data: (cfg) => Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    Text('Runtime',
                        style: Theme.of(context).textTheme.titleMedium),
                    const Spacer(),
                    _HealthDot(),
                  ],
                ),
                const SizedBox(height: 12),
                _row(context, 'env', cfg.env),
                _row(context, 'image_sha', cfg.imageSha ?? 'unset'),
                _row(context, 'package_version', cfg.packageVersion),
                _row(context, 'binary_sha', cfg.binarySha),
                const SizedBox(height: 12),
                Text('OIDC',
                    style: Theme.of(context).textTheme.titleMedium),
                const SizedBox(height: 8),
                _row(context, 'issuer', cfg.oidcIssuer ?? 'not configured'),
                _row(context, 'audience',
                    cfg.oidcAudience ?? 'not configured'),
                _row(context, 'client_id',
                    cfg.oidcClientId ?? 'not configured'),
              ],
            ),
          ),
        ),
      );
}

class _ToolsCard extends StatelessWidget {
  const _ToolsCard(this.tools);
  final AsyncValue<List> tools;

  @override
  Widget build(BuildContext context) => Card(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: tools.when(
            loading: () => const SizedBox(
              height: 120,
              child: Center(child: CircularProgressIndicator()),
            ),
            error: (e, _) => Text('Could not load /v1/tools: $e'),
            data: (list) => Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text('Tools',
                    style: Theme.of(context).textTheme.titleMedium),
                const SizedBox(height: 12),
                Row(
                  crossAxisAlignment: CrossAxisAlignment.end,
                  children: [
                    Text('${list.length}',
                        style: Theme.of(context).textTheme.displaySmall),
                    const SizedBox(width: 8),
                    Padding(
                      padding: const EdgeInsets.only(bottom: 8),
                      child: Text('registered',
                          style: Theme.of(context).textTheme.bodyMedium),
                    ),
                  ],
                ),
                const SizedBox(height: 8),
                Wrap(
                  spacing: 6,
                  runSpacing: 6,
                  children: [
                    for (final t in list.take(8))
                      Chip(
                        label: Text(t.name as String),
                        visualDensity: VisualDensity.compact,
                      ),
                    if (list.length > 8)
                      Chip(
                        label: Text('+${list.length - 8}'),
                        visualDensity: VisualDensity.compact,
                      ),
                  ],
                ),
              ],
            ),
          ),
        ),
      );
}

class _AuditPreviewCard extends StatelessWidget {
  const _AuditPreviewCard();

  @override
  Widget build(BuildContext context) => Card(
        color: Theme.of(context).colorScheme.surfaceContainer,
        child: ListTile(
          leading: const Icon(Icons.list_alt),
          title: const Text('Audit lines stream to stdout'),
          subtitle: const Text(
            'Two records per invocation, shared trace_id. '
            'Open the Audit page for the JSON shape.',
          ),
          trailing: const Icon(Icons.arrow_forward),
        ),
      );
}

Widget _row(BuildContext context, String label, String value) => Padding(
      padding: const EdgeInsets.symmetric(vertical: 2),
      child: Row(
        children: [
          SizedBox(
            width: 140,
            child: Text(label,
                style: Theme.of(context).textTheme.bodySmall?.copyWith(
                      color: ExplorerTheme.onSurfaceVariant,
                    )),
          ),
          Expanded(
            child: Text(value,
                style: Theme.of(context).textTheme.bodyMedium,
                overflow: TextOverflow.ellipsis),
          ),
        ],
      ),
    );

class _HealthDot extends ConsumerWidget {
  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final rest = ref.watch(restClientProvider);
    return FutureBuilder<Map<String, dynamic>>(
      future: rest.healthz(),
      builder: (context, snap) {
        final ok = snap.connectionState == ConnectionState.done &&
            snap.hasData &&
            snap.data?['status'] == 'ok';
        return Container(
          width: 10,
          height: 10,
          decoration: BoxDecoration(
            shape: BoxShape.circle,
            color: ok
                ? Theme.of(context).colorScheme.secondary
                : Theme.of(context).colorScheme.error,
          ),
        );
      },
    );
  }
}
