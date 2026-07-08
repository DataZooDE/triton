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

  /// Current bearer, readable outside the provider graph (boot-time
  /// seeding in `main()` runs before any `ref` exists).
  String? get value => state;

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

/// Seed the bearer from version.json's optional `dev_token` field —
/// the same deployment-config channel that carries `api_url`. Lets a
/// dev stack (e.g. a local compose/script serving the bundle) hand
/// the explorer a ready-to-use token so no manual Settings step is
/// needed. Never overwrites a token the user pasted themselves, and
/// a deployment that doesn't inject the field gets today's behaviour.
Future<void> seedDevToken(
    TokenNotifier notifier, Map<String, dynamic>? versionJson) async {
  if ((notifier.value ?? '').isNotEmpty) return;
  final token = (versionJson?['dev_token'] as String? ?? '').trim();
  if (token.isNotEmpty) {
    await notifier.set(token);
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

/// Resolve the MCP/A2A base. If `/v1/runtime` advertised an override
/// (`mcp_base`/`a2a_base` — set by the single-port embed host, e.g.
/// `/mcp`), use it (absolute URL as-is, or joined onto the REST origin).
/// Otherwise fall back to the dev port-swap (`:8003 → devPort`).
String _resolveBase(String base, String? override, int devPort) {
  if (override != null && override.isNotEmpty) {
    if (override.startsWith('http')) return override;
    final b = base.endsWith('/') ? base.substring(0, base.length - 1) : base;
    final o = override.startsWith('/') ? override : '/$override';
    return '$b$o';
  }
  return base.contains(':8003') ? _swapPort(base, devPort) : base;
}

final mcpClientProvider = Provider<McpClient>((ref) {
  final dio = ref.watch(dioProvider);
  final base = ref.watch(tritonBaseUrlProvider);
  final token = ref.watch(effectiveTokenProvider);
  final cfg = ref.watch(runtimeConfigProvider).valueOrNull;
  return McpClient(
    dio,
    baseUrl: _resolveBase(base, cfg?.mcpBase, 8001),
    token: token,
  );
});

final a2aClientProvider = Provider<A2aClient>((ref) {
  final dio = ref.watch(dioProvider);
  final base = ref.watch(tritonBaseUrlProvider);
  final token = ref.watch(effectiveTokenProvider);
  final cfg = ref.watch(runtimeConfigProvider).valueOrNull;
  return A2aClient(
    dio,
    baseUrl: _resolveBase(base, cfg?.a2aBase, 8002),
    token: token,
  );
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

/// Raw Prometheus text exposition from `/v1/metrics`. Operator pages
/// parse this client-side rather than depending on a Prom client.
final metricsProvider = FutureProvider<String>((ref) async {
  final client = ref.watch(restClientProvider);
  return client.metrics();
});

/// Family provider for the audit endpoint: cache key is
/// `(limit, traceIdFilter)` so the Audit page can swap filters
/// without losing the most recent fetch for the other key.
final auditTailProvider =
    FutureProvider.family<List<Map<String, dynamic>>, AuditQuery>((
      ref,
      q,
    ) async {
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
