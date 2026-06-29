// The chrome-free generic-surface page invokes a tool over REST, renders its
// A2UI v0.9 surface, and routes surface-button taps back through invoke (so a
// cross-tool button dispatches correctly). No trait doubles: a real RestClient
// over a fake Dio adapter, as in console_roundtrip_test.

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/rest_client.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/ui/features/surface/generic_surface_page.dart';

class _SurfaceAdapter implements HttpClientAdapter {
  final List<String> calledTools = [];

  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async {
    final tool = options.path.split('/').last;
    calledTools.add(tool);
    // A narration + a button that dispatches to a *different* tool.
    return ResponseBody.fromString(
      '{"latency_ms":0,"result":{"stream":['
      '{"type":"narration","text":"hello from $tool"},'
      '{"type":"button","label":"Open report",'
      '"action":{"tool":"render_report","args":{"report_id":"r1"}}}'
      ']}}',
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
    final adapter = _SurfaceAdapter();
    dio.httpClientAdapter = adapter;

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          restClientProvider.overrideWith(
            (ref) => RestClient(dio, baseUrl: 'http://t', token: 'dev-token'),
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
