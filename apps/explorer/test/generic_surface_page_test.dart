// The chrome-free generic-surface page invokes a tool over MCP, renders its
// A2UI v0.9 surface, and routes surface-button taps back through invoke (so a
// cross-tool button dispatches correctly). MCP is the surface that carries
// MCP-Apps results. No trait doubles: a real McpClient over a fake Dio
// adapter, as in the other client tests.

import 'dart:convert';

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/mcp_client.dart';
import 'package:triton_explorer/api/runtime_config.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/providers/runtime_provider.dart';
import 'package:triton_explorer/ui/features/surface/generic_surface_page.dart';

class _McpSurfaceAdapter implements HttpClientAdapter {
  final List<String> calledTools = [];

  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async {
    final req = (options.data as Map).cast<String, dynamic>();
    final params = (req['params'] as Map).cast<String, dynamic>();
    final tool = params['name'] as String;
    calledTools.add(tool);
    // MCP tools/call result: structuredContent carries the v0.9 stream nested
    // under `result` (the real wire shape) — a narration plus a button that
    // dispatches to a *different* tool.
    final result = {
      'structuredContent': {
        'result': {
          'stream': [
            {'type': 'narration', 'text': 'hello from $tool'},
            {
              'type': 'button',
              'label': 'Open report',
              'action': {
                'tool': 'render_report',
                'args': {'report_id': 'r1'},
              },
            },
          ],
        },
      },
    };
    return ResponseBody.fromString(
      jsonEncode({'jsonrpc': '2.0', 'id': req['id'], 'result': result}),
      200,
      headers: {
        Headers.contentTypeHeader: ['application/json'],
      },
    );
  }

  @override
  void close({bool force = false}) {}
}

void main() {
  testWidgets('renders a tool surface chrome-free and dispatches button tool',
      (tester) async {
    await tester.binding.setSurfaceSize(const Size(1000, 1000));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final dio = Dio();
    final adapter = _McpSurfaceAdapter();
    dio.httpClientAdapter = adapter;

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          mcpClientProvider.overrideWith(
            (ref) => McpClient(dio, baseUrl: 'http://t', token: 'dev-token'),
          ),
          // The page waits for runtime discovery before its first invoke.
          runtimeConfigProvider.overrideWith(
            (ref) async => RuntimeConfig(
              env: 'test',
              packageVersion: '0',
              binarySha: '0',
              mcpBase: '/mcp',
            ),
          ),
        ],
        child: const MaterialApp(home: GenericSurfacePage(tool: 'assistant')),
      ),
    );
    await tester.pumpAndSettle();

    // It auto-invoked `assistant` and rendered its surface.
    expect(adapter.calledTools.first, 'assistant');
    expect(find.text('hello from assistant'), findsOneWidget);

    // Tapping the button dispatches the button's *own* tool (render_report),
    // not the page's tool.
    await tester.tap(find.widgetWithText(FilledButton, 'Open report'));
    await tester.pumpAndSettle();
    expect(adapter.calledTools.contains('render_report'), isTrue);
    expect(find.text('hello from render_report'), findsOneWidget);
  });
}
