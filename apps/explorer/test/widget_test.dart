// Smoke test: the explorer app boots, the login screen renders, and
// the runtime config provider is loaded from a fake base URL without
// crashing. Per CLAUDE.md §1 we don't mock the dispatcher seam — this
// test stays at the widget layer; the real `flutter test
// integration_test/` against a spawned `triton-bin` lands in PR E2.

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/runtime_config.dart';
import 'package:triton_explorer/auth/login_screen.dart';
import 'package:triton_explorer/providers/runtime_provider.dart';
import 'package:triton_explorer/theme/app_theme.dart';

void main() {
  testWidgets(
      'login screen shows operator-register hint when explorer client unset',
      (tester) async {
    // Inject a synthetic /v1/runtime payload that simulates the
    // "operator hasn't registered the explorer" path. PR E2 swaps
    // this for a real integration_test/ run against spawned triton.
    final cfg = RuntimeConfig(
      env: 'local',
      packageVersion: '0.0.1',
      binarySha: 'abc123',
    );
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          tritonBaseUrlProvider.overrideWith((ref) => 'http://127.0.0.1:1'),
          runtimeConfigProvider.overrideWith((_) async => cfg),
        ],
        child: MaterialApp(
          theme: ExplorerTheme.light(),
          home: const LoginScreen(),
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('Explorer not yet registered'), findsOneWidget);
  });

  testWidgets('login screen shows sign-in button when explorer is configured',
      (tester) async {
    final cfg = RuntimeConfig(
      env: 'nonprod',
      packageVersion: '0.0.1',
      binarySha: 'abc123',
      oidcIssuer: 'https://issuer.example/',
      oidcAudience: 'agents-nonprod',
      oidcClientId: 'triton-explorer',
    );
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          tritonBaseUrlProvider.overrideWith((ref) => 'http://127.0.0.1:1'),
          runtimeConfigProvider.overrideWith((_) async => cfg),
        ],
        child: MaterialApp(
          theme: ExplorerTheme.light(),
          home: const LoginScreen(),
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('Triton Explorer'), findsOneWidget);
    expect(find.byIcon(Icons.login), findsOneWidget);
  });
}
