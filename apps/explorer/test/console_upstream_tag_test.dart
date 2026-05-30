// Upstream agents now arrive in /v1/tools flagged `upstream: true`
// (Triton folds in Consul agent:* services). The Console must list them,
// tag them, and let you select one straight from the list — no need to
// know to type the name.

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

void main() {
  testWidgets('upstream agents are listed, tagged, and selectable',
      (tester) async {
    await tester.binding.setSurfaceSize(const Size(1400, 1200));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final dio = Dio()..httpClientAdapter = _OkAdapter();
    final tools = [
      ToolDescriptor(name: 'echo', inputSchema: const {}, returnsA2ui: false),
      ToolDescriptor(
          name: 'hello',
          inputSchema: const {},
          returnsA2ui: true,
          upstream: true),
    ];

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

    // hello is listed (no longer hidden) and tagged as an upstream agent.
    expect(find.text('hello'), findsOneWidget);
    expect(find.text('upstream agent'), findsOneWidget);

    // Selecting it from the list works — its guide renders.
    await tester.tap(find.text('hello'));
    await tester.pumpAndSettle();
    expect(find.textContaining('adk-rust hello-world agent'), findsOneWidget);
  });
}
