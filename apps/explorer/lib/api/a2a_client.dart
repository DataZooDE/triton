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

  Future<InvocationResult> invoke(
    String name,
    Map<String, dynamic> args, {
    String? a2uiVersion,
  }) async {
    final sw = Stopwatch()..start();
    try {
      final body = <String, dynamic>{
        'parts': [
          {
            'data': {'tool': name, 'args': args},
          }
        ],
        if (a2uiVersion != null) 'metadata': {'a2ui_version': 'v$a2uiVersion'},
      };
      final r = await _dio.post<Map<String, dynamic>>(
        '$baseUrl/message:send',
        data: body,
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
