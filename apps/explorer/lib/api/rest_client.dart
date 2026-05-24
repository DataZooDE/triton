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

  Options _opts({String? accept}) {
    final headers = <String, String>{};
    if (token != null && token!.isNotEmpty) {
      headers['Authorization'] = 'Bearer $token';
    }
    if (accept != null) headers['Accept'] = accept;
    return Options(headers: headers);
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

  /// `POST /v1/tools/:name`. Optional `a2uiVersion` opts the response
  /// into A2UI v0.8 or v0.9 via the `Accept` header.
  Future<InvocationResult> invoke(
    String name,
    Map<String, dynamic> args, {
    String? a2uiVersion,
  }) async {
    final sw = Stopwatch()..start();
    String? accept;
    if (a2uiVersion != null) {
      accept = 'application/json+a2ui; version=$a2uiVersion';
    }
    try {
      final r = await _dio.post<Map<String, dynamic>>(
        '$baseUrl/v1/tools/$name',
        data: args,
        options: _opts(accept: accept).copyWith(responseType: ResponseType.json),
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
}
