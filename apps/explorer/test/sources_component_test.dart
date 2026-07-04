// The `sources` component: click-to-open references to the documents a
// reply touched. Chips render for both A2UI versions, NOTHING opens
// without a tap, and a tap hands the item's `ui://` resource to the host
// (the Console mounts a UiResourceView, which issues `resources/read`).

import 'dart:convert';

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/mcp_client.dart';
import 'package:triton_explorer/api/models.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/ui/features/console/console_page.dart';
import 'package:triton_explorer/widgets/a2ui/a2ui_v08_renderer.dart';
import 'package:triton_explorer/widgets/a2ui/a2ui_v09_renderer.dart';

const _docUri = 'ui://peacock/document?skill=account&id=initech-corp';

void main() {
  testWidgets('v0.9 sources render as chips and a tap opens the resource',
      (tester) async {
    final opened = <String>[];
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: A2UIv09Renderer(
          envelope: const {
            'stream': [
              {
                'type': 'sources',
                'items': [
                  {'label': 'account · initech-corp', 'resource': _docUri},
                  {'label': 'email · email-demo-1', 'resource': 'ui://peacock/document?skill=email&id=email-demo-1'},
                ],
              },
            ],
          },
          onOpenResource: opened.add,
        ),
      ),
    ));

    expect(find.text('Sources'), findsOneWidget);
    expect(find.text('account · initech-corp'), findsOneWidget);
    expect(find.text('email · email-demo-1'), findsOneWidget);
    // Nothing opened on render.
    expect(opened, isEmpty);

    await tester.tap(find.text('account · initech-corp'));
    expect(opened, [_docUri]);
  });

  testWidgets('v0.8 sources render as chips and a tap opens the resource',
      (tester) async {
    final opened = <String>[];
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: A2UIv08Renderer(
          envelope: const {
            'stream': [
              {
                'Component': {
                  'Sources': {
                    'items': [
                      {'label': 'account · initech-corp', 'resource': _docUri},
                    ],
                  },
                },
              },
            ],
          },
          onOpenResource: opened.add,
        ),
      ),
    ));

    expect(find.text('account · initech-corp'), findsOneWidget);
    expect(opened, isEmpty);
    await tester.tap(find.text('account · initech-corp'));
    expect(opened, [_docUri]);
  });

  testWidgets(
      'Console: a sources chip mounts the document view (resources/read fires); nothing before the tap',
      (tester) async {
    await tester.binding.setSurfaceSize(const Size(1200, 900));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final adapter = _McpAdapter();
    final dio = Dio()..httpClientAdapter = adapter;
    final assistant = ToolDescriptor(
      name: 'assistant',
      inputSchema: const {},
      returnsA2ui: true,
      upstream: true,
    );

    await tester.pumpWidget(ProviderScope(
      overrides: [
        toolsProvider.overrideWith((ref) async => [assistant]),
        mcpClientProvider.overrideWith(
          (ref) => McpClient(dio, baseUrl: 'http://t', token: 'dev-token'),
        ),
      ],
      child: const MaterialApp(home: ConsolePage()),
    ));
    await tester.pumpAndSettle();

    await tester.enterText(
        find.byKey(const Key('chat-composer')), 'flag initech-corp');
    await tester.testTextInput.receiveAction(TextInputAction.send);
    await tester.pumpAndSettle();

    // The reply's sources chip is there; no resources/read has fired
    // (sources never auto-open).
    expect(find.text('account · initech-corp'), findsOneWidget);
    expect(adapter.resourceReads, isEmpty);

    await tester.tap(find.text('account · initech-corp'));
    await tester.pumpAndSettle();

    // The tap mounted a UiResourceView → exactly one resources/read of the
    // item's ui:// resource.
    expect(adapter.resourceReads, [_docUri]);
  });
}

/// Fake MCP adapter: `tools/call` returns a v0.9 surface whose stream carries
/// a narration + a sources row; `resources/read` returns a stub HTML document
/// and records the URI.
class _McpAdapter implements HttpClientAdapter {
  final List<String> resourceReads = [];

  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async {
    final req = (options.data as Map).cast<String, dynamic>();
    final method = req['method'] as String?;
    final params = (req['params'] as Map?)?.cast<String, dynamic>() ?? {};
    Object? result;
    if (method == 'resources/read') {
      resourceReads.add(params['uri'] as String);
      result = {
        'contents': [
          {
            'uri': params['uri'],
            'mimeType': 'text/html',
            'text': '<!doctype html><p>doc</p>',
          },
        ],
      };
    } else {
      result = {
        'structuredContent': {
          'result': {
            'stream': [
              {'type': 'narration', 'text': 'Flagged.'},
              {
                'type': 'sources',
                'items': [
                  {'label': 'account · initech-corp', 'resource': _docUri},
                ],
              },
            ],
          },
        },
      };
    }
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
