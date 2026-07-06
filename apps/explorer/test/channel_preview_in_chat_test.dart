// Per-turn channel preview folded into the chat (replaces the standalone
// A2UI-diff page): every agent bubble carries a channel chip row — Web +
// the six chat adapters — and picking one re-renders THAT answer through
// the platform's surface mapper via `POST /v1/surface/render`, in place,
// without touching the rest of the transcript. No trait doubles: a real
// McpClient (chat dispatch) and a real RestClient (the render call) over
// fake Dio adapters.

import 'dart:convert';

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/mcp_client.dart';
import 'package:triton_explorer/api/models.dart';
import 'package:triton_explorer/api/rest_client.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/ui/features/console/console_page.dart';

/// Fake MCP adapter: returns a v0.9 surface echoing the question, so the
/// chat has a real bubble to preview.
class _McpAdapter implements HttpClientAdapter {
  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async {
    final req = (options.data as Map).cast<String, dynamic>();
    final params = (req['params'] as Map).cast<String, dynamic>();
    final args = (params['arguments'] as Map?)?.cast<String, dynamic>() ?? {};
    final q = args['question'] ?? '';
    final result = {
      'structuredContent': {
        'result': {
          'stream': [
            {'type': 'narration', 'text': 'you said: $q'},
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

/// Fake REST adapter: answers `POST /v1/surface/render` with a canned
/// native payload tagged by adapter, and records the requested adapter so
/// the test can assert the right mapper was asked for.
class _RenderAdapter implements HttpClientAdapter {
  String? lastAdapter;

  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async {
    if (!options.path.contains('/v1/surface/render')) {
      return ResponseBody.fromString('{}', 404);
    }
    final req = (options.data as Map).cast<String, dynamic>();
    final adapter = req['adapter'] as String;
    lastAdapter = adapter;
    return ResponseBody.fromString(
      jsonEncode({
        'adapter': adapter,
        'rendered': true,
        'text': '[$adapter] you said: hello there',
        'deferred_buttons': 0,
        'truncated': false,
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

Widget _app(Dio mcpDio, Dio restDio, List<ToolDescriptor> tools) =>
    ProviderScope(
      overrides: [
        toolsProvider.overrideWith((ref) async => tools),
        mcpClientProvider.overrideWith(
          (ref) => McpClient(mcpDio, baseUrl: 'http://t', token: 'dev-token'),
        ),
        restClientProvider.overrideWith(
          (ref) => RestClient(restDio, baseUrl: 'http://t', token: 'dev-token'),
        ),
      ],
      child: const MaterialApp(home: ConsolePage()),
    );

void main() {
  testWidgets(
    'agent bubble carries channel chips and previews a channel in place',
    (tester) async {
      await tester.binding.setSurfaceSize(const Size(1200, 900));
      addTearDown(() => tester.binding.setSurfaceSize(null));

      final restDio = Dio()..httpClientAdapter = _RenderAdapter();
      final renderAdapter = restDio.httpClientAdapter as _RenderAdapter;
      final assistant = ToolDescriptor(
        name: 'assistant',
        inputSchema: const {},
        returnsA2ui: true,
        upstream: true,
      );

      await tester.pumpWidget(
        _app(Dio()..httpClientAdapter = _McpAdapter(), restDio, [assistant]),
      );
      await tester.pumpAndSettle(); // auto-selects the upstream agent

      await tester.enterText(
        find.byKey(const Key('chat-composer')),
        'hello there',
      );
      await tester.testTextInput.receiveAction(TextInputAction.send);
      await tester.pumpAndSettle();

      // The canonical (Web) surface is shown by default.
      expect(find.text('you said: hello there'), findsOneWidget);

      // The bubble offers a channel chip row: Web (selected) plus the six
      // chat adapters.
      expect(find.text('Web'), findsOneWidget);
      expect(find.text('WhatsApp'), findsOneWidget);
      expect(find.text('MS Teams'), findsOneWidget);

      // Pick WhatsApp → the bubble re-renders through the WhatsApp mapper,
      // in place. The canonical surface gives way to the channel chrome.
      await tester.tap(find.text('WhatsApp'));
      await tester.pumpAndSettle();

      expect(renderAdapter.lastAdapter, 'whatsapp');
      expect(
        find.textContaining('[whatsapp] you said: hello there'),
        findsOneWidget,
      );
      expect(find.text('you said: hello there'), findsNothing);
    },
  );
}
