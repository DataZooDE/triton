import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../api/a2a_client.dart';
import '../api/mcp_client.dart';
import '../api/rest_client.dart';
import 'runtime_provider.dart';

/// Persisted dev-time bearer token. In nonprod with the dev-token
/// feature on, the literal value `dev-token` works. Production reads
/// the token after OIDC PKCE completes (lands in a follow-up PR).
class TokenNotifier extends StateNotifier<String?> {
  TokenNotifier(super.initial);

  static const _key = 'triton.bearer';

  static Future<TokenNotifier> load() async {
    final prefs = await SharedPreferences.getInstance();
    return TokenNotifier(prefs.getString(_key));
  }

  Future<void> set(String? token) async {
    state = token;
    final prefs = await SharedPreferences.getInstance();
    if (token == null || token.isEmpty) {
      await prefs.remove(_key);
    } else {
      await prefs.setString(_key, token);
    }
  }
}

final tokenProvider = StateNotifierProvider<TokenNotifier, String?>((ref) {
  // Synchronous fallback — the SharedPreferences load happens at
  // boot in main(). Tests inject a FakeTokenNotifier via override.
  return TokenNotifier(null);
});

final restClientProvider = Provider<RestClient>((ref) {
  final dio = ref.watch(dioProvider);
  final base = ref.watch(tritonBaseUrlProvider);
  final token = ref.watch(tokenProvider);
  return RestClient(dio, baseUrl: base, token: token);
});

/// MCP and A2A run on different ports; in same-origin prod they go
/// through different Fabio routes. For dev we derive ports from the
/// base URL host.
String _swapPort(String base, int port) {
  final uri = Uri.parse(base);
  return uri.replace(port: port).toString();
}

final mcpClientProvider = Provider<McpClient>((ref) {
  final dio = ref.watch(dioProvider);
  final base = ref.watch(tritonBaseUrlProvider);
  final token = ref.watch(tokenProvider);
  // Heuristic for dev: if base ends with :8003 (REST), MCP is :8001.
  final mcpBase = base.contains(':8003') ? _swapPort(base, 8001) : base;
  return McpClient(dio, baseUrl: mcpBase, token: token);
});

final a2aClientProvider = Provider<A2aClient>((ref) {
  final dio = ref.watch(dioProvider);
  final base = ref.watch(tritonBaseUrlProvider);
  final token = ref.watch(tokenProvider);
  final a2aBase = base.contains(':8003') ? _swapPort(base, 8002) : base;
  return A2aClient(dio, baseUrl: a2aBase, token: token);
});

/// Auto-refreshed list of tools from `/v1/tools`. The Playground page
/// binds to this; refreshing pulls the latest registry without a
/// manual UI step.
final toolsProvider = FutureProvider((ref) async {
  final client = ref.watch(restClientProvider);
  return client.listTools();
});
