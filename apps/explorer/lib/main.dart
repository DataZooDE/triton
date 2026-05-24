import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import 'auth/login_screen.dart';
import 'providers/runtime_provider.dart';
import 'theme/app_theme.dart';
import 'ui/shell/app_shell.dart';

void main() {
  runApp(
    ProviderScope(
      overrides: [
        // In prod the SPA is served same-origin behind Triton's CORS
        // layer; `window.location.origin` is the right base. For local
        // dev (`flutter run -d chrome`) we point at the default
        // `triton-bin` REST port so a fresh checkout works without
        // any config tweaking.
        tritonBaseUrlProvider.overrideWithValue(_defaultBaseUrl()),
      ],
      child: const TritonExplorerApp(),
    ),
  );
}

String _defaultBaseUrl() {
  if (kIsWeb) {
    // SAME-ORIGIN path: assumes deployment behind Fabio/Triton's CORS
    // allow-list. Settings page (PR E2) will let the user override.
    return Uri.base.replace(path: '', query: null, fragment: null).toString();
  }
  return 'http://localhost:8003';
}

class TritonExplorerApp extends StatelessWidget {
  const TritonExplorerApp({super.key});

  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Triton Explorer',
        debugShowCheckedModeBanner: false,
        theme: ExplorerTheme.light(),
        // Login → AppShell. PR E2 swaps the placeholder Navigator call
        // for a proper authenticated state guard (and go_router routes
        // for deep-linkable tool URLs).
        initialRoute: '/login',
        routes: {
          '/login': (_) => const LoginScreen(),
          '/': (_) => const AppShell(),
        },
      );
}
