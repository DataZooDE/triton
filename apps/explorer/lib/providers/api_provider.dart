import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../api/a2a_client.dart';
import '../api/mcp_client.dart';
import '../api/rest_client.dart';
import '../auth/auth_manager.dart';
import 'runtime_provider.dart';

/// Persisted dev-time bearer token. Used as a manual override on the
/// Settings page (paste `dev-token` in nonprod, or paste an OIDC JWT
/// obtained out-of-band). The production path is OIDC PKCE through
/// `authProvider`; when a PKCE session exists, its access token wins
/// over the manual paste via [effectiveTokenProvider].
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

/// Resolves the bearer Triton sees on the wire. PKCE access token
/// wins when present; the manually-pasted token is the dev escape
/// hatch (and the only path before/while PKCE isn't configured).
final effectiveTokenProvider = Provider<String?>((ref) {
  final pkce = ref.watch(authProvider).accessToken;
  if (pkce != null && pkce.isNotEmpty) return pkce;
  return ref.watch(tokenProvider);
});

final restClientProvider = Provider<RestClient>((ref) {
  final dio = ref.watch(dioProvider);
  final base = ref.watch(tritonBaseUrlProvider);
  final token = ref.watch(effectiveTokenProvider);
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
  final token = ref.watch(effectiveTokenProvider);
  // Heuristic for dev: if base ends with :8003 (REST), MCP is :8001.
  final mcpBase = base.contains(':8003') ? _swapPort(base, 8001) : base;
  return McpClient(dio, baseUrl: mcpBase, token: token);
});

final a2aClientProvider = Provider<A2aClient>((ref) {
  final dio = ref.watch(dioProvider);
  final base = ref.watch(tritonBaseUrlProvider);
  final token = ref.watch(effectiveTokenProvider);
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

/// `/v1/manifest` envelope. Either `{loaded: false}` or
/// `{loaded: true, manifest: {...}}`.
final manifestProvider = FutureProvider<Map<String, dynamic>>((ref) async {
  final client = ref.watch(restClientProvider);
  return client.manifest();
});

/// Family provider for the audit endpoint: cache key is
/// `(limit, traceIdFilter)` so the Audit page can swap filters
/// without losing the most recent fetch for the other key.
final auditTailProvider = FutureProvider.family<List<Map<String, dynamic>>,
    AuditQuery>((ref, q) async {
  final client = ref.watch(restClientProvider);
  return client.auditTail(limit: q.limit, traceId: q.traceId);
});

class AuditQuery {
  const AuditQuery({required this.limit, this.traceId});
  final int limit;
  final String? traceId;

  @override
  bool operator ==(Object other) =>
      other is AuditQuery && other.limit == limit && other.traceId == traceId;
  @override
  int get hashCode => Object.hash(limit, traceId);
}
