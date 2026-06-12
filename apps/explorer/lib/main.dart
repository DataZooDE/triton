import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import 'auth/auth_manager.dart';
import 'auth/login_screen.dart';
import 'providers/api_provider.dart';
import 'providers/runtime_provider.dart';
import 'theme/app_theme.dart';
import 'ui/shell/app_shell.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  // Read persisted overrides BEFORE the first runApp build so the
  // SPA never paints the "default" URL only to swap immediately.
  // version.json (same origin) is the deployment-config channel:
  // `api_url` discovers the base URL, `dev_token` seeds the bearer
  // on a dev stack so no manual Settings step is needed.
  final versionJson = await fetchVersionJson();
  final baseUrl =
      await loadInitialBaseUrl(_defaultBaseUrl(), versionJson: versionJson);
  final tokenNotifier = await TokenNotifier.load();
  await seedDevToken(tokenNotifier, versionJson);
  runApp(
    ProviderScope(
      overrides: [
        tritonBaseUrlProvider.overrideWith((ref) => baseUrl),
        tokenProvider.overrideWith((ref) => tokenNotifier),
      ],
      child: const TritonExplorerApp(),
    ),
  );
}

String _defaultBaseUrl() {
  if (kIsWeb) {
    // SAME-ORIGIN path: assumes deployment behind Fabio/Triton's CORS
    // allow-list. The Settings page lets the operator override.
    return Uri.base.replace(path: '', query: null, fragment: null).toString();
  }
  return 'http://localhost:8003';
}

class TritonExplorerApp extends ConsumerWidget {
  const TritonExplorerApp({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    // Gate the shell on auth state. When OIDC is configured for this
    // env (cfg.explorerEnabled is true) the user must be `loggedIn`
    // OR have pasted a dev-token in Settings before we hand them the
    // tool playground. When OIDC isn't configured at all, the manual
    // token is the only path and we let the shell render — the
    // individual API calls 401 if the token is bad/missing.
    final auth = ref.watch(authProvider);
    final manualToken = ref.watch(tokenProvider);
    final hasManualToken = manualToken != null && manualToken.isNotEmpty;
    final runtime = ref.watch(runtimeConfigProvider);
    final unlocked = auth.status == AuthStatus.loggedIn ||
        hasManualToken ||
        (runtime.hasValue && runtime.value!.env.isNotEmpty);
    return MaterialApp(
      title: 'Triton Explorer',
      debugShowCheckedModeBanner: false,
      theme: ExplorerTheme.light(),
      home: unlocked ? const AppShell() : const LoginScreen(),
    );
  }
}
