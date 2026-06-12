import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../api/friendly_error.dart';
import '../providers/runtime_provider.dart';
import '../theme/app_theme.dart';
import 'auth_manager.dart';

/// Login screen with three branches:
///   - explorer not registered in this env → ask the operator to
///     set TRITON_EXPLORER_CLIENT_ID and register the redirect URI;
///   - registered, logged out → "Sign in with $issuer" → PKCE flow;
///   - registered, logged in → AppShell takes over via routing in main().
class LoginScreen extends ConsumerWidget {
  const LoginScreen({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final runtime = ref.watch(runtimeConfigProvider);
    final auth = ref.watch(authProvider);
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
                    return _NotRegistered(env: cfg.env);
                  }
                  return _SignInPanel(
                    issuer: cfg.oidcIssuer!,
                    env: cfg.env,
                    version: cfg.packageVersion,
                    auth: auth,
                    onSignIn: () =>
                        ref.read(authProvider.notifier).login(),
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
                error: (e, _) => _BootstrapErrorPanel(error: e),
              ),
            ),
          ),
        ),
      ),
    );
  }
}

/// Bootstrap failure must never be a dead end: the whole app blocks
/// on `/v1/runtime`, so when the persisted base URL is wrong (stale
/// port, trailing slash pasted from an address bar, …) the user
/// can't reach Settings to fix it. Offer the fix inline instead.
class _BootstrapErrorPanel extends ConsumerStatefulWidget {
  const _BootstrapErrorPanel({required this.error});
  final Object error;

  @override
  ConsumerState<_BootstrapErrorPanel> createState() =>
      _BootstrapErrorPanelState();
}

class _BootstrapErrorPanelState extends ConsumerState<_BootstrapErrorPanel> {
  late final TextEditingController _baseUrlCtrl;

  @override
  void initState() {
    super.initState();
    _baseUrlCtrl = TextEditingController(text: ref.read(tritonBaseUrlProvider));
  }

  @override
  void dispose() {
    _baseUrlCtrl.dispose();
    super.dispose();
  }

  Future<void> _saveAndRetry() async {
    await setTritonBaseUrl(ref, _baseUrlCtrl.text);
    // Reflect the normalised form (trailing slashes stripped) so what
    // the user sees is what will be requested.
    _baseUrlCtrl.text = ref.read(tritonBaseUrlProvider);
    ref.invalidate(runtimeConfigProvider);
  }

  @override
  Widget build(BuildContext context) => Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Center(
            child: Icon(Icons.error_outline,
                color: Theme.of(context).colorScheme.error, size: 40),
          ),
          const SizedBox(height: 12),
          Text(friendlyApiError('Could not reach Triton', widget.error)),
          const SizedBox(height: 16),
          const Text(
            'Check the Triton base URL below — it must be the REST '
            'adapter root (e.g. http://127.0.0.1:8081 in local dev).',
            style: TextStyle(fontSize: 12),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: _baseUrlCtrl,
            decoration: const InputDecoration(
              labelText: 'Triton base URL',
              border: OutlineInputBorder(),
            ),
            onSubmitted: (_) => _saveAndRetry(),
          ),
          const SizedBox(height: 12),
          Align(
            alignment: Alignment.centerRight,
            child: FilledButton.icon(
              icon: const Icon(Icons.refresh),
              label: const Text('Save & retry'),
              onPressed: _saveAndRetry,
            ),
          ),
        ],
      );
}

class _NotRegistered extends StatelessWidget {
  const _NotRegistered({required this.env});
  final String env;

  @override
  Widget build(BuildContext context) => Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Explorer not yet registered',
              style: Theme.of(context).textTheme.headlineSmall),
          const SizedBox(height: 12),
          const Text(
            'Triton reports no OIDC client_id for the explorer in this '
            'env. Ask an operator to set TRITON_EXPLORER_CLIENT_ID and '
            "register this host's /redirect.html as a redirect URI "
            "with the substrate's IdP.",
          ),
          const SizedBox(height: 16),
          Text('env: $env',
              style: const TextStyle(color: ExplorerTheme.onSurfaceVariant)),
        ],
      );
}

class _SignInPanel extends StatelessWidget {
  const _SignInPanel({
    required this.issuer,
    required this.env,
    required this.version,
    required this.auth,
    required this.onSignIn,
  });

  final String issuer;
  final String env;
  final String version;
  final AuthState auth;
  final VoidCallback onSignIn;

  @override
  Widget build(BuildContext context) {
    final busy = auth.status == AuthStatus.discovering;
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text('Triton Explorer',
            style: Theme.of(context).textTheme.headlineMedium),
        const SizedBox(height: 8),
        Text('env $env • $version',
            style: Theme.of(context).textTheme.bodySmall?.copyWith(
                  color: ExplorerTheme.onSurfaceVariant,
                )),
        const SizedBox(height: 24),
        FilledButton.icon(
          icon: busy
              ? const SizedBox(
                  width: 16,
                  height: 16,
                  child: CircularProgressIndicator(strokeWidth: 2),
                )
              : const Icon(Icons.login),
          label: Text('Sign in with $issuer'),
          onPressed: busy ? null : onSignIn,
        ),
        if (auth.error != null) ...[
          const SizedBox(height: 16),
          Card(
            color: Theme.of(context).colorScheme.errorContainer,
            child: Padding(
              padding: const EdgeInsets.all(12),
              child: Text(auth.error!),
            ),
          ),
        ],
        const SizedBox(height: 16),
        Text(
          'Or skip sign-in and paste a token in Settings — handy in '
          'nonprod with --features dev-token.',
          style: Theme.of(context).textTheme.bodySmall,
          textAlign: TextAlign.center,
        ),
      ],
    );
  }
}
