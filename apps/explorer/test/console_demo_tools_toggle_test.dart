// The Console's tool list mixes real upstream agents with Triton's
// built-in demo/debug tools (echo, delay, demo_panel, …) — confusing
// when the gateway fronts a real agent. Demo tools (`upstream: false`
// in /v1/tools) are now HIDDEN by default behind a small "Show demo
// tools" tick; upstream agents are always listed.

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/a2a_client.dart';
import 'package:triton_explorer/api/mcp_client.dart';
import 'package:triton_explorer/api/models.dart';
import 'package:triton_explorer/api/rest_client.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/ui/features/console/console_page.dart';

class _OkAdapter implements HttpClientAdapter {
  @override
  Future<ResponseBody> fetch(RequestOptions options, Stream<List<int>>? body,
          Future<void>? cancel) async =>
      ResponseBody.fromString('{}', 200, headers: {
        Headers.contentTypeHeader: ['application/json'],
      });

  @override
  void close({bool force = false}) {}
}

Future<void> _pumpConsole(WidgetTester tester, List<ToolDescriptor> tools)
    async {
  await tester.binding.setSurfaceSize(const Size(1400, 1200));
  addTearDown(() => tester.binding.setSurfaceSize(null));

  final dio = Dio()..httpClientAdapter = _OkAdapter();
  await tester.pumpWidget(
    ProviderScope(
      overrides: [
        toolsProvider.overrideWith((ref) async => tools),
        restClientProvider.overrideWith(
            (ref) => RestClient(dio, baseUrl: 'http://t', token: 'dev')),
        mcpClientProvider.overrideWith(
            (ref) => McpClient(dio, baseUrl: 'http://t', token: 'dev')),
        a2aClientProvider.overrideWith(
            (ref) => A2aClient(dio, baseUrl: 'http://t', token: 'dev')),
      ],
      child: const MaterialApp(home: ConsolePage()),
    ),
  );
  await tester.pumpAndSettle();
}

final _mixed = [
  ToolDescriptor(
      name: 'carl',
      inputSchema: const {},
      returnsA2ui: true,
      upstream: true),
  ToolDescriptor(name: 'echo', inputSchema: const {}, returnsA2ui: false),
  ToolDescriptor(
      name: 'demo_panel', inputSchema: const {}, returnsA2ui: true),
];

void main() {
  testWidgets('demo tools are hidden by default, upstream agents listed',
      (tester) async {
    await _pumpConsole(tester, _mixed);

    expect(find.text('carl'), findsOneWidget);
    expect(find.text('echo'), findsNothing);
    expect(find.text('demo_panel'), findsNothing);
    // The tick announces what is hidden.
    expect(find.text('Show demo tools'), findsOneWidget);
  });

  testWidgets('ticking "Show demo tools" reveals them, unticking hides',
      (tester) async {
    await _pumpConsole(tester, _mixed);

    await tester.tap(find.text('Show demo tools'));
    await tester.pumpAndSettle();
    expect(find.text('echo'), findsOneWidget);
    expect(find.text('demo_panel'), findsOneWidget);

    await tester.tap(find.text('Show demo tools'));
    await tester.pumpAndSettle();
    expect(find.text('echo'), findsNothing);
  });

  testWidgets('no demo tools → no tick, nothing to toggle', (tester) async {
    await _pumpConsole(tester, [
      ToolDescriptor(
          name: 'carl',
          inputSchema: const {},
          returnsA2ui: true,
          upstream: true),
    ]);

    expect(find.text('carl'), findsOneWidget);
    expect(find.text('Show demo tools'), findsNothing);
  });

  testWidgets('only demo tools → list is not silently empty', (tester) async {
    await _pumpConsole(tester, [
      ToolDescriptor(name: 'echo', inputSchema: const {}, returnsA2ui: false),
    ]);

    // Hidden by default, but the tick is there to reveal them.
    expect(find.text('echo'), findsNothing);
    expect(find.text('Show demo tools'), findsOneWidget);
    await tester.tap(find.text('Show demo tools'));
    await tester.pumpAndSettle();
    expect(find.text('echo'), findsOneWidget);
  });
}
