import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../providers/runtime_provider.dart';
import '../../../theme/app_theme.dart';

class DashboardPage extends ConsumerWidget {
  const DashboardPage({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final runtime = ref.watch(runtimeConfigProvider);
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
            runtime.when(
              data: (cfg) => _RuntimeCard(cfg),
              loading: () => const Center(child: CircularProgressIndicator()),
              error: (e, _) => Card(
                color: ExplorerTheme.errorContainer,
                child: Padding(
                  padding: const EdgeInsets.all(16),
                  child: Text(
                    'Could not reach Triton: $e\n\n'
                    'Set the base URL in Settings.',
                    style: const TextStyle(
                      color: ExplorerTheme.onErrorContainer,
                    ),
                  ),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _RuntimeCard extends StatelessWidget {
  const _RuntimeCard(this.cfg);
  final dynamic cfg;

  @override
  Widget build(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Runtime',
                style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 12),
            _row(context, 'env', cfg.env as String),
            _row(context, 'image_sha', (cfg.imageSha as String?) ?? 'unset'),
            _row(context, 'package_version', cfg.packageVersion as String),
            _row(context, 'binary_sha', cfg.binarySha as String),
            const SizedBox(height: 12),
            Text('OIDC',
                style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            _row(context, 'issuer', (cfg.oidcIssuer as String?) ?? 'not configured'),
            _row(context, 'audience', (cfg.oidcAudience as String?) ?? 'not configured'),
            _row(context, 'client_id', (cfg.oidcClientId as String?) ?? 'not configured'),
          ],
        ),
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
}
