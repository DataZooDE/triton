// The Console must pass an `onAction` into the A2UI renderer so a
// rendered Button/Selection/Form actually re-invokes the tool (FR-D-3,
// the stateless round-trip), routed over the active protocol.
//
// No trait doubles: we drive a *real* RestClient over a fake Dio HTTP
// adapter (same approach as clients_test.dart), so the production
// send path runs end-to-end in-widget.

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/models.dart';
import 'package:triton_explorer/api/rest_client.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/ui/features/console/console_page.dart';

class _NarrateAdapter implements HttpClientAdapter {
  int narrateCalls = 0;

  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async {
    if (options.path.endsWith('/v1/tools/narrate')) {
      narrateCalls++;
    }
    // Real wire shape: the v0.9 stream is nested under `result`, with
    // NO top-level `version`. A single Button that re-invokes narrate.
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
  testWidgets('A2UI Button in the Console re-invokes the tool', (tester) async {
    await tester.binding.setSurfaceSize(const Size(1400, 1200));
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
        child: const MaterialApp(home: ConsolePage()),
      ),
    );
    await tester.pumpAndSettle();

    // Select the tool, opt into A2UI v0.9 (REST is the default protocol),
    // and send.
    // Demo (non-upstream) tools are hidden by default — reveal first.
    await tester.tap(find.text('Show demo tools'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('narrate').first);
    await tester.pumpAndSettle();
    await tester.tap(find.text('v0.9'));
    await tester.pumpAndSettle();
    await tester.tap(find.widgetWithText(FilledButton, 'Send'));
    await tester.pumpAndSettle();

    // The Console also renders ChatChannelPreviews for A2UI tools, which
    // fetches the canonical surface with its own send — so guard the
    // round-trip by the *delta* a Button tap causes, not an absolute
    // count.
    final beforeTap = adapter.narrateCalls;
    final refresh = find.widgetWithText(FilledButton, 'Refresh');
    expect(refresh, findsOneWidget);
    await tester.ensureVisible(refresh);
    await tester.pumpAndSettle();
    await tester.tap(refresh);
    await tester.pumpAndSettle();

    expect(
      adapter.narrateCalls,
      greaterThan(beforeTap),
      reason: 'tapping the A2UI Button should re-invoke narrate',
    );
  });
}
