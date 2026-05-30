// The Playground must pass an `onAction` into the A2UI renderer so a
// rendered Button/Selection/Form actually re-invokes the tool (FR-D-3,
// the stateless round-trip). This regression-guards the one place that
// previously dropped the callback, leaving interactive surfaces dead.
//
// No trait doubles: we drive a *real* RestClient over a fake Dio HTTP
// adapter (same approach as clients_test.dart), so the production
// invoke path runs end-to-end in-widget.

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/models.dart';
import 'package:triton_explorer/api/rest_client.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/ui/features/playground/playground_page.dart';

class _NarrateAdapter implements HttpClientAdapter {
  int narrateCalls = 0;

  @override
  Future<ResponseBody> fetch(
      RequestOptions options, Stream<List<int>>? body, Future<void>? cancel) async {
    if (options.path.endsWith('/v1/tools/narrate')) {
      narrateCalls++;
    }
    // Real wire shape: the v0.9 stream is nested under `result`, with
    // NO top-level `version` (the version is implicit in the Accept
    // the client sent). A single Button that re-invokes narrate.
    return ResponseBody.fromString(
      '{"latency_ms":0,"result":{"stream":[{"type":"button",'
      '"label":"Refresh","action":{"tool":"narrate",'
      '"args":{"subject":"again"}}}]}}',
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
  testWidgets('A2UI Button in Playground re-invokes the tool', (tester) async {
    // Wide surface so the Accept segmented-button row lays out without
    // overflow in the headless test viewport (default is 800px).
    await tester.binding.setSurfaceSize(const Size(1400, 1000));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final dio = Dio();
    final adapter = _NarrateAdapter();
    dio.httpClientAdapter = adapter;

    final narrate = ToolDescriptor(
      name: 'narrate',
      inputSchema: const {'type': 'object'},
      returnsA2ui: true,
    );

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          toolsProvider.overrideWith((ref) async => [narrate]),
          restClientProvider.overrideWith(
            (ref) => RestClient(dio, baseUrl: 'http://t', token: 'dev-token'),
          ),
        ],
        child: const MaterialApp(home: PlaygroundPage()),
      ),
    );
    await tester.pumpAndSettle();

    // Select the tool, opt into A2UI v0.9, and invoke.
    await tester.tap(find.text('narrate').first);
    await tester.pumpAndSettle();
    await tester.tap(find.text('A2UI v0.9'));
    await tester.pumpAndSettle();
    await tester.tap(find.widgetWithText(FilledButton, 'Invoke'));
    await tester.pumpAndSettle();

    // The Playground also renders ChatChannelPreviews for A2UI tools,
    // which fetches the canonical surface with its own plain invoke — so
    // the invoke count after rendering is not just the one Invoke. Guard
    // the round-trip by the *delta* a Button tap causes, not an absolute
    // count, so the test stays honest about what it proves: tapping the
    // rendered Button re-invokes the tool (FR-D-3).
    final beforeTap = adapter.narrateCalls;
    final refresh = find.widgetWithText(FilledButton, 'Refresh');
    expect(refresh, findsOneWidget);
    await tester.tap(refresh);
    await tester.pumpAndSettle();

    expect(adapter.narrateCalls, greaterThan(beforeTap),
        reason: 'tapping the A2UI Button should re-invoke narrate');
  });
}
