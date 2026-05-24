import 'package:dio/dio.dart';

import 'models.dart';

/// Hand-rolled JSON-RPC 2.0 client for Triton's MCP adapter
/// (port 8001 in defaults). Triton's MCP layer is intentionally
/// minimal (initialize / tools/list / tools/call / resources/read),
/// so a hand-rolled client beats pulling in a heavier JSON-RPC
/// package.
class McpClient {
  McpClient(this._dio, {required this.baseUrl, this.token});

  final Dio _dio;
  final String baseUrl;
  final String? token;
  int _id = 0;

  int _nextId() => ++_id;

  Options _opts() => Options(
        responseType: ResponseType.json,
        headers: {
          if (token != null && token!.isNotEmpty) 'Authorization': 'Bearer $token',
          'Content-Type': 'application/json',
        },
      );

  Future<Map<String, dynamic>> _rpc(String method, Map<String, dynamic> params) async {
    final body = {
      'jsonrpc': '2.0',
      'id': _nextId(),
      'method': method,
      'params': params,
    };
    final r = await _dio.post<Map<String, dynamic>>(
      '$baseUrl/',
      data: body,
      options: _opts(),
    );
    return r.data!;
  }

  /// `tools/list` — MCP returns tools in camelCase shape. We
  /// translate to the same `ToolDescriptor` the REST client emits so
  /// the playground code is wire-agnostic.
  Future<List<ToolDescriptor>> listTools() async {
    final resp = await _rpc('tools/list', {});
    final tools = (resp['result']?['tools'] as List? ?? const [])
        .cast<Map<String, dynamic>>();
    return tools
        .map((t) => ToolDescriptor(
              name: t['name'] as String,
              inputSchema:
                  (t['inputSchema'] as Map?)?.cast<String, dynamic>() ??
                      <String, dynamic>{},
              returnsA2ui: (t['x-triton']?['returns_a2ui'] as bool?) ?? false,
            ))
        .toList(growable: false);
  }

  /// `tools/call` — same surface as REST `POST /v1/tools/:name`.
  Future<InvocationResult> invoke(
    String name,
    Map<String, dynamic> args, {
    String? a2uiVersion,
  }) async {
    final sw = Stopwatch()..start();
    try {
      final params = <String, dynamic>{
        'name': name,
        'arguments': args,
      };
      if (a2uiVersion != null) {
        params['_meta'] = {'a2ui_version': 'v$a2uiVersion'};
      }
      final resp = await _rpc('tools/call', params);
      sw.stop();
      final result = (resp['result'] as Map?)?.cast<String, dynamic>() ??
          <String, dynamic>{};
      final structured = (result['structuredContent'] as Map?)
              ?.cast<String, dynamic>() ??
          <String, dynamic>{};
      final traceId = (result['_meta']?['trace_id'] as String?);
      return InvocationResult(
        raw: structured,
        statusCode: 200,
        elapsed: sw.elapsed,
        traceId: traceId,
        error: (resp['error'] as Map?)?['message'] as String?,
      );
    } on DioException catch (e) {
      sw.stop();
      return InvocationResult(
        raw: const {},
        statusCode: e.response?.statusCode ?? 0,
        elapsed: sw.elapsed,
        error: e.message ?? e.toString(),
      );
    }
  }
}
