import 'package:dio/dio.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../api/runtime_config.dart';

/// Base URL of the Triton REST adapter — the SPA's source-of-truth
/// for who to call. Mutable so the Settings page can repoint without
/// a rebuild; persisted to `shared_preferences` so the override
/// survives reloads.
///
/// Seeded in `main()` via an override against the persisted value
/// (or the `window.location.origin` / dev-port default when no
/// persisted value exists). Tests inject a value via
/// `overrideWith((ref) => '...')`.
final tritonBaseUrlProvider = StateProvider<String>((ref) {
  throw UnimplementedError('seed via override in main()');
});

const _kPrefsBaseUrlKey = 'triton.baseUrl';

/// Persist a new base URL and update the in-memory provider. Returns
/// the new value so callers can update a controller too.
Future<void> setTritonBaseUrl(WidgetRef ref, String url) async {
  ref.read(tritonBaseUrlProvider.notifier).state = url;
  final prefs = await SharedPreferences.getInstance();
  if (url.isEmpty) {
    await prefs.remove(_kPrefsBaseUrlKey);
  } else {
    await prefs.setString(_kPrefsBaseUrlKey, url);
  }
}

/// Read the persisted base URL, falling back to the supplied default
/// when nothing has been saved. Called once at app start.
Future<String> loadInitialBaseUrl(String fallback) async {
  final prefs = await SharedPreferences.getInstance();
  final saved = prefs.getString(_kPrefsBaseUrlKey);
  if (saved != null && saved.isNotEmpty) {
    return saved;
  }

  if (kIsWeb) {
    try {
      final dio = Dio(BaseOptions(
        connectTimeout: const Duration(seconds: 2),
        receiveTimeout: const Duration(seconds: 2),
      ));
      final versionUrl = Uri.base
          .replace(path: 'version.json', query: null, fragment: null)
          .toString();
      final resp = await dio.get<Map<String, dynamic>>(versionUrl);
      if (resp.statusCode == 200 && resp.data != null) {
        final apiUrl = resp.data!['api_url'] as String?;
        if (apiUrl != null && apiUrl.isNotEmpty) {
          return apiUrl;
        }
      }
    } catch (e) {
      debugPrint('Failed to discover baseUrl from version.json: $e');
    }
  }

  return fallback;
}

final dioProvider = Provider<Dio>((ref) {
  final dio = Dio(BaseOptions(
    connectTimeout: const Duration(seconds: 5),
    receiveTimeout: const Duration(seconds: 10),
  ));
  if (kIsWeb) {
    // The deployed shape (apps/explorer/deploy/explorer.nomad.hcl) puts
    // an oauth2-proxy sidecar in front of each FQDN with a shared
    // cookie-domain `.<env>.int.data-zoo.de`. Cross-origin XHRs from
    // the SPA (`triton-explorer.<env>.int.data-zoo.de`) to the API
    // (`triton-api.<env>.int.data-zoo.de`) only carry the session
    // cookie when the request is "credentialed". Dio's web adapter
    // (`BrowserHttpClientAdapter`) gates this via a `withCredentials`
    // field; setting it via a `dynamic` cast keeps this file
    // cross-platform-safe — the integration test in
    // `integration_test/spawned_triton_test.dart` runs on the Dart VM
    // where `kIsWeb` is false, so the dynamic dispatch never executes.
    (dio.httpClientAdapter as dynamic).withCredentials = true;
  }
  return dio;
});

/// Loads `/v1/runtime` once at boot. The whole app blocks on this —
/// without it, the OIDC PKCE config and the dashboard footer can't
/// render. `AsyncValue.error` surfaces in the bootstrap screen so the
/// operator sees a clear "cannot reach Triton" message instead of a
/// blank page.
final runtimeConfigProvider = FutureProvider<RuntimeConfig>((ref) {
  final dio = ref.watch(dioProvider);
  final base = ref.watch(tritonBaseUrlProvider);
  return RuntimeConfig.fetch(dio, base);
});
