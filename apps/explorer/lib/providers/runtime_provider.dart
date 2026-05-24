import 'package:dio/dio.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../api/runtime_config.dart';

/// Base URL of the Triton REST adapter. The SPA picks this up from
/// `window.location.origin` when served behind Triton's CORS layer,
/// or from the settings page in dev. The dev default points at the
/// out-of-box `triton-bin` listener.
///
/// This provider is `Override`-able from `ProviderScope` so tests
/// and `main()` can inject the value without an env file.
final tritonBaseUrlProvider = Provider<String>((ref) {
  throw UnimplementedError('inject from ProviderScope or main()');
});

final dioProvider = Provider<Dio>((ref) {
  final dio = Dio(BaseOptions(
    connectTimeout: const Duration(seconds: 5),
    receiveTimeout: const Duration(seconds: 10),
  ));
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
