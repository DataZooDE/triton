// The Trace page must surface the distinct trace_ids available in the audit
// ring buffer (derived from /v1/audit) as a clickable list, so an operator
// can browse and load a trace without already knowing its id.

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/rest_client.dart';
import 'package:triton_explorer/api/trace_summary.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/ui/features/trace/trace_page.dart';

/// Answers any `/v1/trace/{id}` GET with a one-phase timeline so a tap on a
/// listed trace resolves to the timeline view.
class _TraceAdapter implements HttpClientAdapter {
  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async => ResponseBody.fromString(
    '{"entries":[{"phase":"dispatch","protocol":"rest","tool":"echo",'
    '"status":200,"latency_ms":5,"result":"ok"}],"bodies":[]}',
    200,
    headers: {
      Headers.contentTypeHeader: ['application/json'],
    },
  );

  @override
  void close({bool force = false}) {}
}

void main() {
  testWidgets('lists available trace_ids and loads one on tap', (tester) async {
    await tester.binding.setSurfaceSize(const Size(1200, 1000));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final dio = Dio()..httpClientAdapter = _TraceAdapter();
    const traces = [
      TraceSummary(
        traceId: 'trace-aaa-111',
        latestWhen: '2026-06-17T00:00:02Z',
        tool: 'echo',
        protocol: 'rest',
        count: 2,
      ),
      TraceSummary(
        traceId: 'trace-bbb-222',
        latestWhen: '2026-06-17T00:00:01Z',
        tool: 'narrate',
        protocol: 'mcp',
        count: 1,
      ),
    ];

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          availableTracesProvider.overrideWith((ref) async => traces),
          restClientProvider.overrideWith(
            (ref) => RestClient(dio, baseUrl: 'http://t', token: 'dev'),
          ),
        ],
        child: const MaterialApp(home: TracePage()),
      ),
    );
    await tester.pumpAndSettle();

    // Both distinct trace_ids are listed.
    expect(find.text('trace-aaa-111'), findsOneWidget);
    expect(find.text('trace-bbb-222'), findsOneWidget);

    // Tapping one loads its timeline.
    await tester.tap(find.text('trace-aaa-111'));
    await tester.pumpAndSettle();
    expect(find.text('Timeline'), findsOneWidget);
  });
}
