// The chat-first Console: the composer maps to the tool's primary argument,
// the answer renders as its A2UI surface, surface buttons re-invoke, and the
// tool rail lists agents (with a "show all" toggle for demo tools). No trait
// doubles: a real McpClient over a fake Dio adapter (MCP is the default
// protocol).

import 'dart:convert';

import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/mcp_client.dart';
import 'package:triton_explorer/api/models.dart';
import 'package:triton_explorer/providers/api_provider.dart';
import 'package:triton_explorer/ui/features/console/console_page.dart';

/// Fake MCP adapter: records each tool call's name + arguments and returns a
/// v0.9 surface (a narration that echoes the question).
class _McpAdapter implements HttpClientAdapter {
  final List<({String tool, Map args})> calls = [];

  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async {
    final req = (options.data as Map).cast<String, dynamic>();
    final params = (req['params'] as Map).cast<String, dynamic>();
    final args = (params['arguments'] as Map?)?.cast<String, dynamic>() ?? {};
    calls.add((tool: params['name'] as String, args: args));
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

Widget _app(Dio dio, List<ToolDescriptor> tools) => ProviderScope(
      overrides: [
        toolsProvider.overrideWith((ref) async => tools),
        mcpClientProvider.overrideWith(
          (ref) => McpClient(dio, baseUrl: 'http://t', token: 'dev-token'),
        ),
      ],
      child: const MaterialApp(home: ConsolePage()),
    );

void main() {
  testWidgets('composer sends the message as the primary arg and renders the surface',
      (tester) async {
    await tester.binding.setSurfaceSize(const Size(1200, 900));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final dio = Dio()..httpClientAdapter = (_McpAdapter());
    final adapter = dio.httpClientAdapter as _McpAdapter;
    final assistant = ToolDescriptor(
      name: 'assistant',
      inputSchema: const {},
      returnsA2ui: true,
      upstream: true,
    );

    await tester.pumpWidget(_app(dio, [assistant]));
    await tester.pumpAndSettle(); // auto-selects the upstream agent

    await tester.enterText(find.byKey(const Key('chat-composer')), 'hello there');
    await tester.testTextInput.receiveAction(TextInputAction.send);
    await tester.pumpAndSettle();

    // The message went to `assistant` as the `question` arg (schema-less
    // upstream → default primary key).
    expect(adapter.calls.single.tool, 'assistant');
    expect(adapter.calls.single.args['question'], 'hello there');
    // Both the user bubble and the rendered surface are shown.
    expect(find.text('hello there'), findsOneWidget);
    expect(find.text('you said: hello there'), findsOneWidget);
  });

  testWidgets('tool rail lists agents and the toggle reveals demo tools',
      (tester) async {
    await tester.binding.setSurfaceSize(const Size(1200, 900));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final dio = Dio()..httpClientAdapter = (_McpAdapter());
    final assistant = ToolDescriptor(
      name: 'assistant',
      inputSchema: const {},
      returnsA2ui: true,
      upstream: true,
    );
    final echo = ToolDescriptor(
      name: 'echo',
      inputSchema: const {},
      returnsA2ui: false,
    );

    await tester.pumpWidget(_app(dio, [assistant, echo]));
    await tester.pumpAndSettle();

    expect(find.text('assistant'), findsOneWidget);
    expect(find.text('echo'), findsNothing); // demo tool hidden by default

    await tester.tap(find.text('Show all tools'));
    await tester.pumpAndSettle();
    expect(find.text('echo'), findsOneWidget);
  });
}
