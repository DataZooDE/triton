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

  /// The MCP POST target. When `baseUrl` is a bare origin (port-mode:
  /// MCP at root on its own port) the path is `/`. When `/v1/runtime`
  /// advertised a mount path (single-port embed, `mcp_base=/mcp`) the
  /// base already carries it — appending `/` would yield `/mcp/`, which
  /// 404s against the nested router, so use the base verbatim.
  String get _endpoint {
    final p = Uri.parse(baseUrl).path;
    return (p.isEmpty || p == '/') ? '$baseUrl/' : baseUrl;
  }

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
      _endpoint,
      data: body,
      options: _opts(),
    );
    return r.data!;
  }

  /// `initialize` — the MCP handshake. Triton negotiates a protocol
  /// version from its supported set (rejecting unknown ones) and
  /// advertises its `tools`/`resources` capabilities. We pin the
  /// latest spec revision the gateway knows.
  Future<McpServerInfo> initialize({
    String protocolVersion = '2025-06-18',
  }) async {
    final resp = await _rpc('initialize', {'protocolVersion': protocolVersion});
    final result =
        (resp['result'] as Map?)?.cast<String, dynamic>() ?? <String, dynamic>{};
    return McpServerInfo.fromResult(result);
  }

  /// `resources/read` — Triton serves a runtime-discovery stub at
  /// `ui://triton/runtime.html`.
  Future<McpResource> readResource(String uri) async {
    final resp = await _rpc('resources/read', {'uri': uri});
    final result =
        (resp['result'] as Map?)?.cast<String, dynamic>() ?? <String, dynamic>{};
    return McpResource.fromResult(result);
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
              upstream: (t['x-triton']?['upstream'] as bool?) ?? false,
            ))
        .toList(growable: false);
  }

  /// The exact MCP frame for a tool call: a full JSON-RPC 2.0
  /// `tools/call` envelope. The editable body is the whole frame —
  /// `jsonrpc`/`id`/`method`/`params` — so the Console can show and
  /// correct the protocol framing itself, not just the args.
  WireRequest buildRequest(
    String name,
    Map<String, dynamic> args, {
    String? a2uiVersion,
  }) {
    final params = <String, dynamic>{
      'name': name,
      'arguments': args,
    };
    if (a2uiVersion != null) {
      params['_meta'] = {'a2ui_version': 'v$a2uiVersion'};
    }
    return WireRequest(
      protocol: 'MCP',
      method: 'POST',
      url: _endpoint,
      headers: const {'Content-Type': 'application/json'},
      body: {
        'jsonrpc': '2.0',
        'id': _nextId(),
        'method': 'tools/call',
        'params': params,
      },
    );
  }

  /// Send a (possibly hand-edited) JSON-RPC frame exactly as given and
  /// unwrap the `structuredContent` result.
  Future<InvocationResult> sendRaw(WireRequest req) async {
    final sw = Stopwatch()..start();
    try {
      final r = await _dio.post<Map<String, dynamic>>(
        req.url,
        data: req.body,
        options: _opts(),
      );
      sw.stop();
      final resp = r.data!;
      final result = (resp['result'] as Map?)?.cast<String, dynamic>() ??
          <String, dynamic>{};
      final structured = (result['structuredContent'] as Map?)
              ?.cast<String, dynamic>() ??
          <String, dynamic>{};
      final traceId = (result['_meta']?['trace_id'] as String?);
      final err = (resp['error'] as Map?)?.cast<String, dynamic>();
      return InvocationResult(
        raw: structured,
        statusCode: 200,
        elapsed: sw.elapsed,
        traceId: traceId,
        error: err?['message'] as String?,
        errorCode: err?['code'] as int?,
        uiResourceUri: InvocationResult.uiFrom(result),
        toolTrace: InvocationResult.toolTraceFrom(result),
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

  /// `tools/call` — same surface as REST `POST /v1/tools/:name`.
  Future<InvocationResult> invoke(
    String name,
    Map<String, dynamic> args, {
    String? a2uiVersion,
  }) =>
      sendRaw(buildRequest(name, args, a2uiVersion: a2uiVersion));
}
