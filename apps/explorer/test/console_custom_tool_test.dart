// The Console must let you target a tool that ISN'T in /v1/tools — an
// upstream agent Triton dispatches into by Consul tag (the `hello`
// adk-rust agent). This is the "inject a call to any tool" affordance.

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
  testWidgets('a custom tool name (not in /v1/tools) can be targeted',
      (tester) async {
    await tester.binding.setSurfaceSize(const Size(1400, 1200));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final dio = Dio()..httpClientAdapter = _OkAdapter();
    // /v1/tools only knows a built-in tool; `hello` is upstream-only.
    final echo = ToolDescriptor(
        name: 'echo', inputSchema: const {}, returnsA2ui: false);

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          toolsProvider.overrideWith((ref) async => [echo]),
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

    // `hello` is not listed.
    expect(find.text('hello'), findsNothing);

    await tester.enterText(find.byKey(const Key('custom-tool-field')), 'hello');
    await tester.tap(find.widgetWithText(TextButton, 'Use tool'));
    await tester.pumpAndSettle();

    // The hello guide appears and the REST envelope seeds for it.
    expect(find.textContaining('adk-rust hello-world agent'), findsOneWidget);
    final editor = tester
        .widget<TextField>(find.byKey(const Key('envelope-editor')))
        .controller!
        .text;
    expect(editor, isNot(contains('jsonrpc')));
  });
}
