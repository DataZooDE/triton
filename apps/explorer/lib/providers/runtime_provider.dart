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

/// Canonical form of a Triton base URL: trimmed, no trailing slashes.
/// Every consumer string-concatenates paths onto it
/// (`'$base/v1/runtime'`), so a trailing slash — the shape every
/// browser address bar produces on copy-paste — yields `//v1/...`,
/// which Triton's router 404s. Normalise at BOTH ends (save and load)
/// so a value persisted by an older build heals on next boot.
String normalizeBaseUrl(String url) {
  var out = url.trim();
  while (out.endsWith('/')) {
    out = out.substring(0, out.length - 1);
  }
  return out;
}

/// Persist a new base URL and update the in-memory provider. Returns
/// the new value so callers can update a controller too.
Future<void> setTritonBaseUrl(WidgetRef ref, String url) async {
  final normalized = normalizeBaseUrl(url);
  ref.read(tritonBaseUrlProvider.notifier).state = normalized;
  final prefs = await SharedPreferences.getInstance();
  if (normalized.isEmpty) {
    await prefs.remove(_kPrefsBaseUrlKey);
  } else {
    await prefs.setString(_kPrefsBaseUrlKey, normalized);
  }
}

/// Same-origin version.json, fetched once at boot. It doubles as the
/// SPA's deployment-config channel: the serving layer can inject
/// `api_url` (base URL discovery) and, for dev stacks, `dev_token`
/// (bearer seed — see `seedDevToken`). Returns null off-web or when
/// the fetch fails; every consumer treats null as "no config".
Future<Map<String, dynamic>?> fetchVersionJson() async {
  if (!kIsWeb) return null;
  try {
    final dio = Dio(BaseOptions(
      connectTimeout: const Duration(seconds: 2),
      receiveTimeout: const Duration(seconds: 2),
    ));
    final versionUrl = Uri.base
        .replace(path: 'version.json', query: null, fragment: null)
        .toString();
    final resp = await dio.get<Map<String, dynamic>>(versionUrl);
    if (resp.statusCode == 200) {
      return resp.data;
    }
  } catch (e) {
    debugPrint('Failed to fetch version.json: $e');
  }
  return null;
}

/// Read the persisted base URL, falling back to version.json's
/// `api_url` and then the supplied default. Called once at app start.
Future<String> loadInitialBaseUrl(String fallback,
    {Map<String, dynamic>? versionJson}) async {
  final prefs = await SharedPreferences.getInstance();
  final saved = normalizeBaseUrl(prefs.getString(_kPrefsBaseUrlKey) ?? '');
  if (saved.isNotEmpty) {
    return saved;
  }

  final apiUrl = normalizeBaseUrl(versionJson?['api_url'] as String? ?? '');
  if (apiUrl.isNotEmpty) {
    return apiUrl;
  }

  return normalizeBaseUrl(fallback);
}

final dioProvider = Provider<Dio>((ref) {
  final dio = Dio(BaseOptions(
    connectTimeout: const Duration(seconds: 5),
    // A live upstream agent doing LLM tool-calling can take well over 10s for
    // one turn; keep the read window generous so a slow synthesis doesn't
    // abort as a client timeout.
    receiveTimeout: const Duration(seconds: 90),
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
