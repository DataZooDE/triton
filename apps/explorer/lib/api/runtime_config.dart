import 'package:dio/dio.dart';

/// Anonymous payload from `GET /v1/runtime` on Triton — see
/// `crates/triton-adapters-http/src/rest.rs::RuntimeDiscovery`. The
/// SPA reads this at boot to learn (a) which OIDC issuer to redirect
/// to for PKCE login and (b) which env/image it's looking at.
class RuntimeConfig {
  RuntimeConfig({
    required this.env,
    required this.packageVersion,
    required this.binarySha,
    this.imageSha,
    this.oidcIssuer,
    this.oidcAudience,
    this.oidcClientId,
    this.mcpBase,
    this.a2aBase,
  });

  final String env;
  final String? imageSha;
  final String packageVersion;
  final String binarySha;
  final String? oidcIssuer;
  final String? oidcAudience;
  final String? oidcClientId;

  /// Base path/URL for the MCP and A2A surfaces when they are NOT on the
  /// dev ports. The single-port embedded host (triton-embed) sets these to
  /// `/mcp` and `/a2a`; absent → the SPA uses its `:8003→:8001/:8002`
  /// port-swap. See `RuntimeDiscovery` in rest.rs.
  final String? mcpBase;
  final String? a2aBase;

  /// When true, the operator hasn't enabled the explorer for this
  /// env — show an "ask an operator to register me" message rather
  /// than failing PKCE opaquely.
  bool get explorerEnabled =>
      oidcIssuer != null && oidcAudience != null && oidcClientId != null;

  factory RuntimeConfig.fromJson(Map<String, dynamic> json) => RuntimeConfig(
    env: json['env'] as String,
    imageSha: json['image_sha'] as String?,
    packageVersion: json['package_version'] as String,
    binarySha: json['binary_sha'] as String,
    oidcIssuer: json['oidc_issuer'] as String?,
    oidcAudience: json['oidc_audience'] as String?,
    oidcClientId: json['oidc_client_id'] as String?,
    mcpBase: json['mcp_base'] as String?,
    a2aBase: json['a2a_base'] as String?,
  );

  /// Fetch from the canonical anonymous endpoint. `baseUrl` is the
  /// REST adapter root (`http://localhost:8003` in dev, the public
  /// Triton URL in nonprod/prod — set via the settings page).
  static Future<RuntimeConfig> fetch(Dio dio, String baseUrl) async {
    final resp = await dio.get<Map<String, dynamic>>(
      '$baseUrl/v1/runtime',
      options: Options(responseType: ResponseType.json),
    );
    return RuntimeConfig.fromJson(resp.data!);
  }
}
