// Per-channel visual chrome: a rendered channel preview should look like the
// product, not a generic card. Messaging channels get chat bubbles; card
// channels (Teams/Copilot/Google Chat) get an Adaptive-Card-style card with
// an accent bar + markdown; Gemini gets an answer card that renders markdown
// tables. Each chrome carries a stable `channel-chrome-<adapter>` key so the
// console can tell which shell is on screen.

import 'dart:convert';

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/mcp_client.dart';
import 'package:triton_explorer/api/models.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/widgets/a2ui/markdown_lite.dart';
import 'package:triton_explorer/widgets/a2ui/ui_resource_view.dart';
import 'package:triton_explorer/widgets/channel/channel_view.dart';

Widget _host(String adapter, Map<String, dynamic> payload) => MaterialApp(
  home: Scaffold(
    body: ChannelBubble(adapter: adapter, payload: payload),
  ),
);

Map<String, dynamic> _msg(String text) => {
  'rendered': true,
  'text': text,
  'deferred_buttons': 0,
  'truncated': false,
};

/// Fake MCP adapter answering `resources/read` with canned HTML, so a
/// [UiResourceView] embed resolves to its source (the non-web fallback).
class _ResourceAdapter implements HttpClientAdapter {
  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async {
    final req = (options.data as Map).cast<String, dynamic>();
    return ResponseBody.fromString(
      jsonEncode({
        'jsonrpc': '2.0',
        'id': req['id'],
        'result': {
          'contents': [
            {
              'uri': 'ui://peacock/r1',
              'mimeType': 'text/html',
              'text': '<h1>REPORT-CHART</h1>',
            },
          ],
        },
      }),
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
  testWidgets('WhatsApp renders as a chat bubble showing the message', (
    tester,
  ) async {
    await tester.pumpWidget(_host('whatsapp', _msg('Initech leads.')));
    await tester.pumpAndSettle();
    expect(
      find.byKey(const ValueKey('channel-chrome-whatsapp')),
      findsOneWidget,
    );
    expect(find.textContaining('Initech leads.'), findsOneWidget);
  });

  testWidgets('Copilot renders as an Adaptive-Card-style card', (tester) async {
    await tester.pumpWidget(_host('copilot', _msg('**Bold** answer.')));
    await tester.pumpAndSettle();
    expect(
      find.byKey(const ValueKey('channel-chrome-copilot')),
      findsOneWidget,
    );
    // Card channels render markdown (bold survives, no raw `**`).
    expect(find.byType(MarkdownLite), findsWidgets);
    expect(find.textContaining('**'), findsNothing);
  });

  testWidgets('Gemini renders a markdown table as a real Table', (
    tester,
  ) async {
    const md =
        'Here is the quarter.\n\n**Q3**\n\n| Metric | Value |\n| --- | --- |\n| Revenue | €48,250 |\n| Open orders | 3 |';
    await tester.pumpWidget(_host('gemini', _msg(md)));
    await tester.pumpAndSettle();
    expect(find.byKey(const ValueKey('channel-chrome-gemini')), findsOneWidget);
    // The pipe table becomes an actual Table widget, not raw `|` text.
    expect(find.byType(Table), findsOneWidget);
    expect(find.text('Revenue'), findsOneWidget);
    expect(find.text('€48,250'), findsOneWidget);
  });

  // The surface's button components (negotiated `stream` shape: `type:button`
  // + `action.tool`).
  InvocationResult resultWithButtons({String? uiResourceUri}) =>
      InvocationResult(
        raw: {
          'stream': [
            {'type': 'narration', 'text': 'Recorded.'},
            {
              'type': 'button',
              'label': 'Open report',
              'action': {'tool': 'render_report', 'args': {}},
              'resource': 'ui://peacock/r1',
            },
            {
              'type': 'button',
              'label': 'Mark reviewed',
              'action': {'tool': 'mark_reviewed', 'args': {}},
            },
          ],
        },
        statusCode: 200,
        elapsed: Duration.zero,
        uiResourceUri: uiResourceUri,
      );

  testWidgets('card channel renders the surface buttons, not a deferred chip', (
    tester,
  ) async {
    final payload = {
      'rendered': true,
      'text': 'Recorded.',
      'deferred_buttons': 2,
      'truncated': false,
    };
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: ChannelBubble(
            adapter: 'copilot',
            payload: payload,
            result: resultWithButtons(),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    // The buttons render as real button chrome (both labels), keyed per
    // adapter.
    expect(find.byKey(const ValueKey('channel-buttons-copilot')), findsOneWidget);
    expect(find.text('Open report'), findsOneWidget);
    expect(find.text('Mark reviewed'), findsOneWidget);
    // …and the "buttons: N" deferred chip is dropped — no double reporting.
    expect(find.textContaining('buttons: 2'), findsNothing);
  });

  testWidgets('messaging channel keeps the deferred-buttons chip', (
    tester,
  ) async {
    final payload = {
      'rendered': true,
      'text': 'Recorded.',
      'deferred_buttons': 2,
      'truncated': false,
    };
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: ChannelBubble(
            adapter: 'whatsapp',
            payload: payload,
            result: resultWithButtons(),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    // Pure-messaging channels can't render actions — no button chrome, the
    // deferred chip stays.
    expect(find.byKey(const ValueKey('channel-buttons-whatsapp')), findsNothing);
    expect(find.textContaining('buttons: 2'), findsOneWidget);
  });

  testWidgets('channel preview renders the report graphic on a non-web channel', (
    tester,
  ) async {
    await tester.binding.setSurfaceSize(const Size(900, 1200));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final dio = Dio()..httpClientAdapter = _ResourceAdapter();
    final payload = {
      'rendered': true,
      'text': 'Here is the quarter.',
      'deferred_dashboards': 1,
      'truncated': false,
    };
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          mcpClientProvider.overrideWith(
            (ref) => McpClient(dio, baseUrl: 'http://t', token: 'dev-token'),
          ),
        ],
        child: MaterialApp(
          home: Scaffold(
            body: SingleChildScrollView(
              child: ChannelBubble(
                adapter: 'gemini',
                payload: payload,
                result: resultWithButtons(uiResourceUri: 'ui://peacock/r1'),
              ),
            ),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    // The same UiResourceView embed the Web bubble uses — resolved to its
    // HTML source by the non-web fallback.
    expect(find.byType(UiResourceView), findsOneWidget);
    expect(find.textContaining('REPORT-CHART'), findsOneWidget);
  });

  testWidgets('email renders an email shell with subject header + link buttons', (
    tester,
  ) async {
    // The email mapper's preview payload carries a `subject` + `html` no other
    // channel does; the email skin renders a From/To/Subject header, the
    // surface buttons render inline, and the deferred chip is dropped.
    final payload = {
      'rendered': true,
      'subject': 'Initech renewal at risk',
      'html': '<p>body</p>',
      'text': 'Recorded.',
      'deferred_buttons': 0,
      'deferred_forms': 0,
      'truncated': false,
    };
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: ChannelBubble(
            adapter: 'email',
            payload: payload,
            result: resultWithButtons(),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    // The email shell is on screen.
    expect(find.byKey(const ValueKey('channel-chrome-email')), findsOneWidget);
    // The subject line and the envelope labels render.
    expect(find.text('Subject'), findsOneWidget);
    expect(find.text('Initech renewal at risk'), findsOneWidget);
    expect(find.text('From'), findsOneWidget);
    // Email renders the surface buttons (it's button-capable), keyed per adapter.
    expect(find.byKey(const ValueKey('channel-buttons-email')), findsOneWidget);
    expect(find.text('Open report'), findsOneWidget);
    expect(find.text('Mark reviewed'), findsOneWidget);
  });

  testWidgets('email with no subject shows a placeholder', (tester) async {
    await tester.pumpWidget(_host('email', _msg('Body only.')));
    await tester.pumpAndSettle();
    expect(find.byKey(const ValueKey('channel-chrome-email')), findsOneWidget);
    expect(find.text('(no subject)'), findsOneWidget);
  });

  test('buttonsFrom lifts both surface shapes and skips empty labels', () {
    // Raw agent surface: `components` + `kind:button`, tool on the node.
    final raw = ChannelBubble.buttonsFrom({
      'surface': {
        'components': [
          {'kind': 'narration', 'text': 'hi'},
          {
            'kind': 'button',
            'label': 'Open report',
            'tool': 'render_report',
            'resource': 'ui://peacock/r1',
          },
          {'kind': 'button', 'label': '  '}, // empty → skipped
        ],
      },
    });
    expect(raw.map((b) => b.label), ['Open report']);
    expect(raw.single.tool, 'render_report');
    expect(raw.single.resource, 'ui://peacock/r1');

    // Negotiated envelope: `stream` + `type:button`, tool under `action`.
    final stream = ChannelBubble.buttonsFrom({
      'stream': [
        {
          'type': 'button',
          'label': 'Mark done',
          'action': {'tool': 'mark_done', 'args': {}},
        },
      ],
    });
    expect(stream.single.label, 'Mark done');
    expect(stream.single.tool, 'mark_done');

    expect(ChannelBubble.buttonsFrom(null), isEmpty);
  });
}
