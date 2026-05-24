import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../auth/auth_manager.dart';
import '../../../providers/api_provider.dart';
import '../../../providers/runtime_provider.dart';

class SettingsPage extends ConsumerStatefulWidget {
  const SettingsPage({super.key});

  @override
  ConsumerState<SettingsPage> createState() => _SettingsPageState();
}

class _SettingsPageState extends ConsumerState<SettingsPage> {
  late final TextEditingController _tokenCtrl;
  late final TextEditingController _baseUrlCtrl;

  @override
  void initState() {
    super.initState();
    _tokenCtrl = TextEditingController(text: ref.read(tokenProvider) ?? '');
    _baseUrlCtrl =
        TextEditingController(text: ref.read(tritonBaseUrlProvider));
  }

  @override
  void dispose() {
    _tokenCtrl.dispose();
    _baseUrlCtrl.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final auth = ref.watch(authProvider);
    return Scaffold(
      appBar: AppBar(title: const Text('Settings')),
      body: ListView(
        padding: const EdgeInsets.all(24),
        children: [
          Card(
            child: Padding(
              padding: const EdgeInsets.all(16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('Triton base URL',
                      style: Theme.of(context).textTheme.titleMedium),
                  const SizedBox(height: 4),
                  const Text(
                    'The REST adapter root. Same-origin default; '
                    'override to point at a non-Fabio Triton (e.g. '
                    'http://localhost:8003 in local dev). Persisted.',
                    style: TextStyle(fontSize: 12),
                  ),
                  const SizedBox(height: 12),
                  TextField(
                    controller: _baseUrlCtrl,
                    decoration: const InputDecoration(
                      labelText: 'base URL',
                      border: OutlineInputBorder(),
                    ),
                  ),
                  const SizedBox(height: 12),
                  Row(
                    mainAxisAlignment: MainAxisAlignment.end,
                    children: [
                      FilledButton(
                        onPressed: () async {
                          await setTritonBaseUrl(
                              ref, _baseUrlCtrl.text.trim());
                          ref.invalidate(runtimeConfigProvider);
                          ref.invalidate(toolsProvider);
                          if (context.mounted) {
                            ScaffoldMessenger.of(context).showSnackBar(
                              const SnackBar(
                                content: Text('Base URL saved'),
                                duration: Duration(seconds: 2),
                              ),
                            );
                          }
                        },
                        child: const Text('Save'),
                      ),
                    ],
                  ),
                ],
              ),
            ),
          ),
          const SizedBox(height: 16),
          Card(
            child: Padding(
              padding: const EdgeInsets.all(16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('Bearer token',
                      style: Theme.of(context).textTheme.titleMedium),
                  const SizedBox(height: 4),
                  const Text(
                    "Manual override — used when OIDC PKCE isn't "
                    "configured for this env. Paste 'dev-token' in "
                    'nonprod with --features dev-token, or an OIDC JWT '
                    'obtained out-of-band. The OIDC PKCE access token '
                    'takes precedence when present.',
                    style: TextStyle(fontSize: 12),
                  ),
                  const SizedBox(height: 12),
                  TextField(
                    controller: _tokenCtrl,
                    decoration: const InputDecoration(
                      labelText: 'token',
                      border: OutlineInputBorder(),
                    ),
                  ),
                  const SizedBox(height: 12),
                  Row(
                    mainAxisAlignment: MainAxisAlignment.end,
                    children: [
                      TextButton(
                        onPressed: () {
                          _tokenCtrl.clear();
                          ref.read(tokenProvider.notifier).set(null);
                        },
                        child: const Text('Clear'),
                      ),
                      const SizedBox(width: 8),
                      FilledButton(
                        onPressed: () => ref
                            .read(tokenProvider.notifier)
                            .set(_tokenCtrl.text.trim()),
                        child: const Text('Save'),
                      ),
                    ],
                  ),
                ],
              ),
            ),
          ),
          if (auth.status == AuthStatus.loggedIn) ...[
            const SizedBox(height: 16),
            Card(
              child: ListTile(
                title: Text('OIDC session: ${auth.user?.uid ?? "(unknown)"}'),
                subtitle: const Text('Signed in via PKCE flow.'),
                trailing: TextButton.icon(
                  icon: const Icon(Icons.logout),
                  label: const Text('Sign out'),
                  onPressed: () =>
                      ref.read(authProvider.notifier).logout(),
                ),
              ),
            ),
          ],
        ],
      ),
    );
  }
}
