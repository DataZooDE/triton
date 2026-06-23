// Unit + client tests for the SSE streaming consumer (issue #132). The
// parser must reassemble frames split across byte chunks, skip
// keep-alive comments, and map the typed event vocabulary; the REST
// client's invokeStreaming must drive it from a real Dio stream
// response (via a mock adapter that emits raw SSE bytes).

import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:dio/dio.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/rest_client.dart';
import 'package:triton_explorer/api/sse.dart';

/// Mock adapter that streams the supplied SSE chunks back as the
/// response body (status 200, text/event-stream), one chunk per event
/// loop turn so the decoder genuinely sees them arrive separately.
class _SseAdapter implements HttpClientAdapter {
  _SseAdapter(this.chunks, {this.status = 200});
  final List<String> chunks;
  final int status;

  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async {
    final controller = StreamController<Uint8List>();
    () async {
      for (final c in chunks) {
        await Future<void>.delayed(const Duration(milliseconds: 1));
        controller.add(Uint8List.fromList(utf8.encode(c)));
      }
      await controller.close();
    }();
    return ResponseBody(
      controller.stream,
      status,
      headers: {
        Headers.contentTypeHeader: ['text/event-stream'],
      },
    );
  }

  @override
  void close({bool force = false}) {}
}

Stream<List<int>> _bytes(List<String> chunks) async* {
  for (final c in chunks) {
    yield utf8.encode(c);
  }
}

void main() {
  group('decodeSseFrames', () {
    test('reassembles frames split across chunk boundaries', () async {
      final frames = await decodeSseFrames(
        _bytes([
          'event: tool\ndata: {"step":"search"}\n\n',
          'event: token\ndata: {"delta":"Hi"}\n\nevent: do',
          'ne\ndata: {"surface":{}}\n\n',
        ]),
      ).toList();

      expect(frames.length, 3);
      expect(frames[0].type, StreamEventType.tool);
      expect(frames[1].type, StreamEventType.token);
      expect(frames[1].delta, 'Hi');
      expect(frames[2].type, StreamEventType.done);
      expect(frames[2].isTerminal, isTrue);
    });

    test('skips comment keep-alive frames', () async {
      final frames = await decodeSseFrames(
        _bytes([': keep-alive\n\n', 'event: done\ndata: {}\n\n']),
      ).toList();
      expect(frames.length, 1);
      expect(frames.single.type, StreamEventType.done);
    });

    test('flushes a trailing unterminated frame on close', () async {
      final frames = await decodeSseFrames(
        _bytes(['event: done\ndata: {"ok":true}']),
      ).toList();
      expect(frames.length, 1);
      expect(frames.single.type, StreamEventType.done);
    });

    test('handles CRLF line endings', () async {
      final frames = await decodeSseFrames(
        _bytes(['event: token\r\ndata: {"delta":"x"}\r\n\r\n']),
      ).toList();
      expect(frames.single.delta, 'x');
    });

    test('unknown event name maps to tool', () async {
      final frames = await decodeSseFrames(
        _bytes(['event: retrieval\ndata: {"stage":"expand"}\n\n']),
      ).toList();
      expect(frames.single.type, StreamEventType.tool);
    });

    test('caps an unbounded frame with a terminal error', () async {
      final huge = 'event: tool\ndata: ${'x' * (1024 * 1024 + 1024)}';
      final frames = await decodeSseFrames(_bytes([huge])).toList();
      expect(frames.length, 1);
      expect(frames.single.type, StreamEventType.error);
    });
  });

  group('RestClient.invokeStreaming', () {
    test('yields tool/token/done frames from a streamed response', () async {
      final dio = Dio()
        ..httpClientAdapter = _SseAdapter([
          'event: tool\ndata: {"step":"search","hits":30}\n\n',
          'event: token\ndata: {"delta":"Hello "}\n\n',
          'event: token\ndata: {"delta":"world"}\n\n',
          'event: done\ndata: {"version":"0.9"}\n\n',
        ]);
      final client = RestClient(dio, baseUrl: 'http://t', token: 'dev');

      final frames = await client.invokeStreaming('grounded', {'q': 'hi'}).toList();

      expect(frames.map((f) => f.type).toList(), [
        StreamEventType.tool,
        StreamEventType.token,
        StreamEventType.token,
        StreamEventType.done,
      ]);
      final tokens = frames
          .where((f) => f.type == StreamEventType.token)
          .map((f) => f.delta)
          .join();
      expect(tokens, 'Hello world');
      expect((frames.last.data as Map)['version'], '0.9');
    });

    test('surfaces a pre-first-byte HTTP error as one error frame', () async {
      // Non-2xx makes Dio throw; invokeStreaming must convert it to a
      // single terminal error frame rather than propagate the exception.
      final dio = Dio()
        ..httpClientAdapter = _SseAdapter(
          ['{"error":"validation","message":"unknown tool: nope"}'],
          status: 400,
        );
      final client = RestClient(dio, baseUrl: 'http://t', token: 'dev');

      final frames = await client.invokeStreaming('nope', {}).toList();
      expect(frames.length, 1);
      expect(frames.single.type, StreamEventType.error);
    });
  });
}
