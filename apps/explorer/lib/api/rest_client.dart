import 'package:dio/dio.dart';

import 'models.dart';

/// Talks to Triton's REST adapter (port 8003 in defaults).
/// Per-method timeouts intentionally short — UI feedback should fire
/// fast and let the operator retry rather than hang.
class RestClient {
  RestClient(this._dio, {required this.baseUrl, this.token});

  final Dio _dio;
  final String baseUrl;
  final String? token;

  Options _opts() {
    final headers = <String, String>{};
    if (token != null && token!.isNotEmpty) {
      headers['Authorization'] = 'Bearer $token';
    }
    return Options(headers: headers);
  }

  /// `GET /v1/metrics` — Prometheus text exposition. Auth-gated;
  /// same Bearer as /v1/tools. The unauthenticated tailnet-only
  /// listener on :9090 is the canonical scrape target; this route
  /// is for the explorer SPA.
  Future<String> metrics() async {
    final r = await _dio.get<String>(
      '$baseUrl/v1/metrics',
      options: _opts().copyWith(responseType: ResponseType.plain),
    );
    return r.data ?? '';
  }

  /// `POST /v1/surface/render` — runs an A2UI Surface through the
  /// named chat-channel surface mapper and returns what that
  /// adapter would post. Only `telegram` wired today.
  Future<Map<String, dynamic>> renderSurface({
    required String adapter,
    required Map<String, dynamic> result,
  }) async {
    final r = await _dio.post<Map<String, dynamic>>(
      '$baseUrl/v1/surface/render',
      data: {'adapter': adapter, 'result': result},
      options: _opts().copyWith(responseType: ResponseType.json),
    );
    return r.data!;
  }

  /// `GET /v1/manifest` — current adapter.yaml as JSON, with
  /// credentials redacted by the server. Returns
  /// `{loaded: false}` when no manifest is configured.
  Future<Map<String, dynamic>> manifest() async {
    final r = await _dio.get<Map<String, dynamic>>(
      '$baseUrl/v1/manifest',
      options: _opts().copyWith(responseType: ResponseType.json),
    );
    return r.data!;
  }

  /// `GET /v1/audit` — newest-first slice of the in-process audit
  /// ring buffer. Authenticated like /v1/tools.
  Future<List<Map<String, dynamic>>> auditTail({
    int limit = 50,
    String? traceId,
  }) async {
    final qp = <String, dynamic>{'limit': limit.toString()};
    if (traceId != null && traceId.isNotEmpty) {
      qp['trace_id'] = traceId;
    }
    final r = await _dio.get<Map<String, dynamic>>(
      '$baseUrl/v1/audit',
      queryParameters: qp,
      options: _opts().copyWith(responseType: ResponseType.json),
    );
    return ((r.data!['entries'] as List?) ?? const [])
        .cast<Map<String, dynamic>>();
  }

  /// `GET /v1/trace/{trace_id}` — one communication as a timeline:
  /// `{ trace_id, entries: [audit…], bodies: [captured…] }`. `bodies` is
  /// empty unless Triton was built with the dev `capture` feature.
  Future<Map<String, dynamic>> trace(String traceId) async {
    final r = await _dio.get<Map<String, dynamic>>(
      '$baseUrl/v1/trace/$traceId',
      options: _opts().copyWith(responseType: ResponseType.json),
    );
    return r.data!;
  }

  Future<Map<String, dynamic>> healthz() async {
    final r = await _dio.get<Map<String, dynamic>>(
      '$baseUrl/healthz',
      options: Options(responseType: ResponseType.json),
    );
    return r.data!;
  }

  /// `GET /v1/tools` — requires auth.
  Future<List<ToolDescriptor>> listTools() async {
    final r = await _dio.get<Map<String, dynamic>>(
      '$baseUrl/v1/tools',
      options: _opts().copyWith(responseType: ResponseType.json),
    );
    final tools = (r.data!['tools'] as List).cast<Map<String, dynamic>>();
    return tools.map(ToolDescriptor.fromJson).toList(growable: false);
  }

  /// The exact REST frame for a tool call: `POST /v1/tools/:name` with
  /// the args as the JSON body and an optional A2UI `Accept` header. The
  /// editable body IS the args object — REST has no envelope wrapper.
  WireRequest buildRequest(
    String name,
    Map<String, dynamic> args, {
    String? a2uiVersion,
  }) {
    final headers = <String, String>{'Content-Type': 'application/json'};
    if (a2uiVersion != null) {
      headers['Accept'] = 'application/json+a2ui; version=$a2uiVersion';
    }
    return WireRequest(
      protocol: 'REST',
      method: 'POST',
      url: '$baseUrl/v1/tools/$name',
      headers: headers,
      body: args,
    );
  }

  /// Send a (possibly hand-edited) REST frame exactly as given.
  Future<InvocationResult> sendRaw(WireRequest req) async {
    final sw = Stopwatch()..start();
    try {
      final r = await _dio.post<Map<String, dynamic>>(
        req.url,
        data: req.body,
        options: Options(
          responseType: ResponseType.json,
          headers: {
            ...req.headers,
            if (token != null && token!.isNotEmpty)
              'Authorization': 'Bearer $token',
          },
        ),
      );
      sw.stop();
      return InvocationResult(
        raw: r.data!,
        statusCode: r.statusCode ?? 0,
        elapsed: sw.elapsed,
        traceId: r.data?['trace_id'] as String?,
      );
    } on DioException catch (e) {
      sw.stop();
      final body = (e.response?.data is Map)
          ? (e.response!.data as Map).cast<String, dynamic>()
          : <String, dynamic>{};
      return InvocationResult(
        raw: body,
        statusCode: e.response?.statusCode ?? 0,
        elapsed: sw.elapsed,
        traceId: body['trace_id'] as String?,
        error: e.message ?? e.toString(),
      );
    }
  }

  /// `POST /v1/tools/:name`. Optional `a2uiVersion` opts the response
  /// into A2UI v0.8 or v0.9 via the `Accept` header.
  Future<InvocationResult> invoke(
    String name,
    Map<String, dynamic> args, {
    String? a2uiVersion,
  }) => sendRaw(buildRequest(name, args, a2uiVersion: a2uiVersion));
}
