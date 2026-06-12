// Regression tests for the poisoned-base-URL dead end.
//
// A `triton.baseUrl` persisted with a trailing slash (the shape every
// browser address bar produces on copy-paste) made the SPA request
// `//v1/runtime`, which Triton's router 404s. Because the whole app
// blocks on `/v1/runtime`, the user was then stuck on the login
// screen's error card with no way to reach Settings — only clearing
// site storage (or incognito) recovered. Two fixes under test:
//
//   1. base URLs are normalised (trailing slashes stripped) on save
//      AND on load, so already-poisoned storage heals at next boot;
//   2. the login screen's "Could not reach Triton" card carries an
//      inline base-URL editor + retry, so a bad URL is never a dead
//      end.

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:triton_explorer/auth/login_screen.dart';
import 'package:triton_explorer/providers/runtime_provider.dart';

void main() {
  group('normalizeBaseUrl', () {
    test('strips a single trailing slash', () {
      expect(normalizeBaseUrl('http://127.0.0.1:8081/'),
          'http://127.0.0.1:8081');
    });

    test('strips repeated trailing slashes and whitespace', () {
      expect(normalizeBaseUrl(' http://127.0.0.1:8081// '),
          'http://127.0.0.1:8081');
    });

    test('leaves a clean URL untouched', () {
      expect(normalizeBaseUrl('http://127.0.0.1:8081'),
          'http://127.0.0.1:8081');
    });

    test('keeps a path prefix, only trailing slashes go', () {
      expect(normalizeBaseUrl('https://triton.example/api/'),
          'https://triton.example/api');
    });

    test('empty stays empty', () {
      expect(normalizeBaseUrl(''), '');
    });
  });

  group('loadInitialBaseUrl', () {
    test('heals a persisted trailing-slash URL', () async {
      SharedPreferences.setMockInitialValues(
          {'triton.baseUrl': 'http://127.0.0.1:8081/'});
      final url = await loadInitialBaseUrl('http://fallback.invalid');
      expect(url, 'http://127.0.0.1:8081');
    });
  });

  group('LoginScreen bootstrap-error recovery', () {
    testWidgets('error card offers a base-URL editor + retry',
        (tester) async {
      SharedPreferences.setMockInitialValues({});
      await tester.pumpWidget(
        ProviderScope(
          overrides: [
            tritonBaseUrlProvider
                .overrideWith((ref) => 'http://127.0.0.1:9/'),
            runtimeConfigProvider.overrideWith(
                (ref) => Future.error(Exception('status code of 404'))),
          ],
          child: const MaterialApp(home: LoginScreen()),
        ),
      );
      await tester.pumpAndSettle();

      // The failure is shown...
      expect(find.textContaining('Could not reach Triton'), findsOneWidget);
      // ...but it is NOT a dead end: the base URL can be fixed in place.
      expect(find.byType(TextField), findsOneWidget);
      expect(
        find.widgetWithText(FilledButton, 'Save & retry'),
        findsOneWidget,
      );
    });

    testWidgets('retry saves the normalised URL and refetches',
        (tester) async {
      SharedPreferences.setMockInitialValues({});
      final fetched = <String>[];
      final container = ProviderContainer(overrides: [
        tritonBaseUrlProvider.overrideWith((ref) => 'http://old.invalid/'),
        runtimeConfigProvider.overrideWith((ref) {
          fetched.add(ref.watch(tritonBaseUrlProvider));
          return Future.error(Exception('status code of 404'));
        }),
      ]);
      addTearDown(container.dispose);

      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: container,
          child: const MaterialApp(home: LoginScreen()),
        ),
      );
      await tester.pumpAndSettle();

      await tester.enterText(
          find.byType(TextField), 'http://127.0.0.1:8081/');
      await tester.tap(find.widgetWithText(FilledButton, 'Save & retry'));
      await tester.pumpAndSettle();

      // The provider now holds the normalised URL (no trailing slash),
      // and the runtime fetch ran again against it.
      expect(container.read(tritonBaseUrlProvider), 'http://127.0.0.1:8081');
      expect(fetched.last, 'http://127.0.0.1:8081');
    });
  });
}
