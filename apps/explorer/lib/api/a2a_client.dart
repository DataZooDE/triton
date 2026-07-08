import 'package:dio/dio.dart';

import 'models.dart';

/// A2A wire shape: `Message{parts: [Part{data: {tool, args}}], metadata}`.
/// Triton's A2A adapter mounts at `POST /message:send` (port 8002 in
/// defaults).
class A2aClient {
  A2aClient(this._dio, {required this.baseUrl, this.token});

  final Dio _dio;
  final String baseUrl;
  final String? token;

  Options _opts() => Options(
        responseType: ResponseType.json,
        headers: {
          if (token != null && token!.isNotEmpty) 'Authorization': 'Bearer $token',
          'Content-Type': 'application/json',
        },
      );

  /// The exact A2A frame: a `Message{parts:[{data:{tool,args}}],
  /// metadata}`. The editable body is the whole Message, so the Console
  /// can show and correct the agent-to-agent envelope itself.
  WireRequest buildRequest(
    String name,
    Map<String, dynamic> args, {
    String? a2uiVersion,
  }) {
    return WireRequest(
      protocol: 'A2A',
      method: 'POST',
      url: '$baseUrl/message:send',
      headers: const {'Content-Type': 'application/json'},
      body: <String, dynamic>{
        'parts': [
          {
            'data': {'tool': name, 'args': args},
          }
        ],
        if (a2uiVersion != null) 'metadata': {'a2ui_version': 'v$a2uiVersion'},
      },
    );
  }

  Future<InvocationResult> invoke(
    String name,
    Map<String, dynamic> args, {
    String? a2uiVersion,
  }) =>
      sendRaw(buildRequest(name, args, a2uiVersion: a2uiVersion));

  /// Send a (possibly hand-edited) A2A Message exactly as given and
  /// unwrap `parts[0].data`.
  Future<InvocationResult> sendRaw(WireRequest req) async {
    final sw = Stopwatch()..start();
    try {
      final r = await _dio.post<Map<String, dynamic>>(
        req.url,
        data: req.body,
        options: _opts(),
      );
      sw.stop();
      final data = r.data!;
      final firstPart = ((data['parts'] as List?)?.first
          as Map?)?.cast<String, dynamic>();
      return InvocationResult(
        raw: firstPart?['data']?.cast<String, dynamic>() ?? data,
        statusCode: r.statusCode ?? 0,
        elapsed: sw.elapsed,
        traceId: (data['metadata']?['trace_id'] as String?),
        // FR-A-7: the A2A task store echoes the terminal state on
        // success. Errors carry `metadata.{error,message}` instead —
        // the failed state stays internal to the store — so this is
        // null on the error path below.
        taskState: (data['metadata']?['task_state'] as String?),
        toolTrace: InvocationResult.toolTraceFrom(
          (data['metadata'] as Map?)?.cast<String, dynamic>(),
        ),
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
        traceId: body['metadata']?['trace_id'] as String? ??
            body['trace_id'] as String?,
        error: e.message ?? e.toString(),
      );
    }
  }
}
