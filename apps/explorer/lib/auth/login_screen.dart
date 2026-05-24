import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../providers/runtime_provider.dart';
import '../theme/app_theme.dart';

/// Stage-1 login screen: reads `/v1/runtime` and either tells the
/// operator to register a redirect URI (when the explorer isn't yet
/// operator-enabled) or shows the "Sign in" button that will trigger
/// PKCE.
///
/// The actual `oidc` package wiring lands in PR E2 once the API
/// client trio is in place — for now this screen renders the
/// discovered state so the boot path is exercised end-to-end against
/// a real Triton.
class LoginScreen extends ConsumerWidget {
  const LoginScreen({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final runtime = ref.watch(runtimeConfigProvider);
    return Scaffold(
      body: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 480),
          child: Card(
            child: Padding(
              padding: const EdgeInsets.all(32),
              child: runtime.when(
                data: (cfg) {
                  if (!cfg.explorerEnabled) {
                    return Column(
                      mainAxisSize: MainAxisSize.min,
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(
                          'Explorer not yet registered',
                          style: Theme.of(context).textTheme.headlineSmall,
                        ),
                        const SizedBox(height: 12),
                        const Text(
                          'Triton reports no OIDC client_id for the '
                          'explorer in this env. Ask an operator to set '
                          'TRITON_EXPLORER_CLIENT_ID and register this '
                          "host's /auth/callback as a redirect URI with "
                          "the substrate's IdP.",
                        ),
                        const SizedBox(height: 16),
                        Text('env: ${cfg.env}',
                            style: const TextStyle(
                                color: ExplorerTheme.onSurfaceVariant)),
                      ],
                    );
                  }
                  return Column(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      Text(
                        'Triton Explorer',
                        style: Theme.of(context).textTheme.headlineMedium,
                      ),
                      const SizedBox(height: 8),
                      Text(
                        'env ${cfg.env} • ${cfg.packageVersion}',
                        style: Theme.of(context).textTheme.bodySmall?.copyWith(
                              color: ExplorerTheme.onSurfaceVariant,
                            ),
                      ),
                      const SizedBox(height: 24),
                      FilledButton.icon(
                        icon: const Icon(Icons.login),
                        label: Text('Sign in with ${cfg.oidcIssuer}'),
                        onPressed: () {
                          // PKCE wiring lands in PR E2; for now we
                          // route the dev into the dashboard so
                          // the rest of the shell is exercisable.
                          Navigator.of(context).pushReplacementNamed('/');
                        },
                      ),
                      const SizedBox(height: 16),
                      Text(
                        'PKCE flow lands in PR E2.',
                        style: Theme.of(context).textTheme.bodySmall,
                      ),
                    ],
                  );
                },
                loading: () => const Column(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    CircularProgressIndicator(),
                    SizedBox(height: 12),
                    Text('Discovering Triton...'),
                  ],
                ),
                error: (e, _) => Column(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Icon(Icons.error_outline,
                        color: Theme.of(context).colorScheme.error, size: 40),
                    const SizedBox(height: 12),
                    Text('Could not reach Triton: $e'),
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }
}
