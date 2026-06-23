import 'dart:async';
import 'dart:convert';

/// The SSE event vocabulary Triton streams (issue #132). Mirrors the
/// server-side `StreamEvent`: `tool` progress, `token` deltas, and the
/// terminal `done`/`error`.
enum StreamEventType { tool, token, done, error }

/// One decoded SSE frame from a streaming tool invocation.
class StreamFrame {
  StreamFrame(this.type, this.data, {this.delta});

  final StreamEventType type;

  /// The decoded JSON payload of the frame's `data:` line (or the raw
  /// string if it wasn't JSON).
  final dynamic data;

  /// For `token` frames, the extracted delta string (the `delta` key of
  /// `data`, or the raw payload as a fallback).
  final String? delta;

  bool get isTerminal =>
      type == StreamEventType.done || type == StreamEventType.error;
}

/// Cap on the unterminated (partial-frame) buffer, mirroring the Rust
/// decoder: a server that streams bytes without a frame boundary must not
/// grow the client's memory without limit. On overflow we yield a
/// terminal error frame and stop. 1 MiB is generous for a `done` envelope.
const _maxFrameBytes = 1024 * 1024;

/// Decode a raw SSE byte stream (Dio's `ResponseType.stream` body) into a
/// stream of typed [StreamFrame]s. Frames are blank-line separated; a
/// frame split across byte chunks is reassembled, and a trailing
/// unterminated frame is flushed on close. Unknown event names map to
/// [StreamEventType.tool] for forward-compatibility (mirrors the Rust
/// decoder).
Stream<StreamFrame> decodeSseFrames(Stream<List<int>> byteStream) async* {
  // Buffer raw bytes and split on frame boundaries (which are ASCII
  // `\n\n` / `\r\n\r\n`), decoding each complete block as UTF-8. Working
  // in bytes — rather than `.transform(utf8.decoder)` — avoids both the
  // `Stream<Uint8List>` transformer-typing mismatch and a multi-byte
  // char split across chunk boundaries.
  final buffer = <int>[];
  await for (final chunk in byteStream) {
    buffer.addAll(chunk);
    while (true) {
      final b = _findBoundary(buffer);
      if (b == null) break;
      final block = buffer.sublist(0, b.offset);
      buffer.removeRange(0, b.offset + b.sep);
      final frame = _parseBlock(utf8.decode(block, allowMalformed: true));
      if (frame != null) yield frame;
    }
    if (buffer.length > _maxFrameBytes) {
      yield StreamFrame(StreamEventType.error, {
        'message': 'SSE frame exceeded $_maxFrameBytes bytes without a boundary',
      });
      return;
    }
  }
  if (buffer.isNotEmpty) {
    final frame = _parseBlock(utf8.decode(buffer, allowMalformed: true));
    if (frame != null) yield frame;
  }
}

/// Position + separator length of the first SSE frame boundary in `buf`.
class _Boundary {
  _Boundary(this.offset, this.sep);
  final int offset;
  final int sep;
}

/// First `\n\n` (sep 2) or `\r\n\r\n` (sep 4) in the byte buffer.
_Boundary? _findBoundary(List<int> b) {
  for (var i = 0; i + 1 < b.length; i++) {
    if (i + 3 < b.length &&
        b[i] == 13 &&
        b[i + 1] == 10 &&
        b[i + 2] == 13 &&
        b[i + 3] == 10) {
      return _Boundary(i, 4);
    }
    if (b[i] == 10 && b[i + 1] == 10) {
      return _Boundary(i, 2);
    }
  }
  return null;
}

/// Parse one frame body (no trailing blank line). Returns null for a
/// comment/keep-alive frame that carries neither `event:` nor `data:`.
StreamFrame? _parseBlock(String block) {
  String? event;
  final dataLines = <String>[];
  for (final raw in block.split('\n')) {
    final line = raw.endsWith('\r') ? raw.substring(0, raw.length - 1) : raw;
    if (line.isEmpty || line.startsWith(':')) continue; // blank or comment
    final ci = line.indexOf(':');
    final field = ci < 0 ? line : line.substring(0, ci);
    var value = ci < 0 ? '' : line.substring(ci + 1);
    if (value.startsWith(' ')) value = value.substring(1);
    switch (field) {
      case 'event':
        event = value;
      case 'data':
        dataLines.add(value);
      default:
        break; // id, retry, … not part of the Triton vocabulary
    }
  }
  if (event == null && dataLines.isEmpty) return null;

  final dataStr = dataLines.join('\n');
  dynamic data;
  try {
    data = dataStr.isEmpty ? null : jsonDecode(dataStr);
  } catch (_) {
    data = dataStr;
  }

  switch (event) {
    case 'token':
      final delta = (data is Map && data['delta'] is String)
          ? data['delta'] as String
          : dataStr;
      return StreamFrame(StreamEventType.token, data, delta: delta);
    case 'done':
      return StreamFrame(StreamEventType.done, data);
    case 'error':
      return StreamFrame(StreamEventType.error, data);
    default:
      // "tool" and any unrecognised name → opaque progress.
      return StreamFrame(StreamEventType.tool, data);
  }
}
