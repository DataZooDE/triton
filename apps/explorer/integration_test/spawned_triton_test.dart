// Integration test that spawns the real `triton-bin` (no mocks) and
// drives the explorer's API client against it end-to-end. Per
// CLAUDE.md §1, a task is "done" only once an integration test
// against a running process passes — this file is the Flutter-side
// counterpart to crates/triton-tests/tests/explorer_contract.rs.
//
// Run with:
//   cd apps/explorer
//   flutter test integration_test/spawned_triton_test.dart \
//       -d linux                                              \
//       --dart-define=TRITON_BIN_PATH=$PWD/../../target/debug/triton
//
// Requires `cargo build -p triton-bin` to have been run at least
// once. The Cargo bin target is named `triton` (the package is
// `triton-bin`); dev-token is on by default.

import 'dart:async';
import 'dart:io';

import 'package:dio/dio.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

import 'package:triton_explorer/api/rest_client.dart';

const _kBinaryPathEnv = 'TRITON_BIN_PATH';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  group('spawned triton-bin', () {
    late Process child;
    late int restPort;

    setUpAll(() async {
      const binaryPath =
          String.fromEnvironment(_kBinaryPathEnv, defaultValue: '');
      if (binaryPath.isEmpty || !File(binaryPath).existsSync()) {
        fail(
          'Set --dart-define=$_kBinaryPathEnv=<absolute path> to the '
          'triton-bin executable. Run `cargo build -p triton-bin '
          '--features dev-token` first if it doesn\'t exist yet.',
        );
      }
      restPort = await _freePort();
      final mcpPort = await _freePort();
      final a2aPort = await _freePort();
      final metricsPort = await _freePort();

      child = await Process.start(binaryPath, const [], environment: {
        'TRITON_HOST': '127.0.0.1',
        'TRITON_REST_PORT': '$restPort',
        'TRITON_MCP_PORT': '$mcpPort',
        'TRITON_A2A_PORT': '$a2aPort',
        'TRITON_METRICS_HOST': '127.0.0.1',
        'TRITON_METRICS_PORT': '$metricsPort',
        'TRITON_DRAIN_DEADLINE_SECS': '3',
        'RUST_LOG': 'warn',
      });
      // Drain stdout/stderr so a misbehaving child doesn't block on a
      // full pipe buffer.
      unawaited(child.stdout.drain<void>());
      unawaited(child.stderr.drain<void>());

      // Wait until /healthz responds 200 (up to 5s).
      final deadline = DateTime.now().add(const Duration(seconds: 5));
      final dio = Dio();
      while (DateTime.now().isBefore(deadline)) {
        try {
          final r = await dio.get<dynamic>('http://127.0.0.1:$restPort/healthz');
          if (r.statusCode == 200) return;
        } catch (_) {}
        await Future<void>.delayed(const Duration(milliseconds: 50));
      }
      fail('triton-bin did not respond to /healthz within 5s');
    });

    tearDownAll(() async {
      child.kill(ProcessSignal.sigterm);
      await child.exitCode.timeout(const Duration(seconds: 5),
          onTimeout: () {
        child.kill(ProcessSignal.sigkill);
        return -1;
      });
    });

    test('/v1/tools roundtrips through the Dart RestClient', () async {
      final client = RestClient(
        Dio(),
        baseUrl: 'http://127.0.0.1:$restPort',
        token: 'dev-token',
      );
      final tools = await client.listTools();
      expect(tools, isNotEmpty,
          reason: 'in-process registry should contain echo at least');
      final echo =
          tools.where((t) => t.name == 'echo').toList(growable: false);
      expect(echo, hasLength(1), reason: 'echo tool registered');
      expect(echo.first.inputSchema, isNotEmpty);
    });

    test('invoke echo returns trace_id and a result', () async {
      final client = RestClient(
        Dio(),
        baseUrl: 'http://127.0.0.1:$restPort',
        token: 'dev-token',
      );
      final r = await client.invoke('echo', {'message': 'hi'});
      expect(r.ok, isTrue, reason: 'echo should succeed: ${r.error}');
      expect(r.statusCode, 200);
      expect(r.traceId, isNotNull);
    });
  });
}

Future<int> _freePort() async {
  final s = await ServerSocket.bind('127.0.0.1', 0);
  final p = s.port;
  await s.close();
  return p;
}
