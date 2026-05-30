// The Console's editable request envelope: it must seed with the exact
// per-protocol wire frame (so you can *understand* each protocol), reseed
// when you switch protocol, and refuse to send malformed JSON (so an
// injected/corrected message is well-formed before it hits the wire).

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

/// No-op adapter — this test never sends, but Dio needs an adapter.
class _OkAdapter implements HttpClientAdapter {
  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<List<int>>? body,
    Future<void>? cancel,
  ) async => ResponseBody.fromString(
    '{}',
    200,
    headers: {
      Headers.contentTypeHeader: ['application/json'],
    },
  );

  @override
  void close({bool force = false}) {}
}

void main() {
  testWidgets('envelope seeds per protocol and validates JSON', (tester) async {
    await tester.binding.setSurfaceSize(const Size(1400, 1200));
    addTearDown(() => tester.binding.setSurfaceSize(null));

    final dio = Dio()..httpClientAdapter = _OkAdapter();
    final hello = ToolDescriptor(
      name: 'hello',
      inputSchema: const {
        'type': 'object',
        'properties': {
          'subject': {'type': 'string'},
        },
      },
      returnsA2ui: true,
    );

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          toolsProvider.overrideWith((ref) async => [hello]),
          restClientProvider.overrideWith(
            (ref) => RestClient(dio, baseUrl: 'http://t', token: 'dev'),
          ),
          mcpClientProvider.overrideWith(
            (ref) => McpClient(dio, baseUrl: 'http://t', token: 'dev'),
          ),
          a2aClientProvider.overrideWith(
            (ref) => A2aClient(dio, baseUrl: 'http://t', token: 'dev'),
          ),
        ],
        child: const MaterialApp(home: ConsolePage()),
      ),
    );
    await tester.pumpAndSettle();

    String editor() => tester
        .widget<TextField>(find.byKey(const Key('envelope-editor')))
        .controller!
        .text;

    await tester.tap(find.text('hello').first);
    await tester.pumpAndSettle();

    // REST default: the body IS the raw args object (no envelope wrapper).
    expect(editor(), isNot(contains('jsonrpc')));
    expect(editor(), isNot(contains('parts')));

    // MCP → the full JSON-RPC frame.
    await tester.tap(find.text('MCP').first);
    await tester.pumpAndSettle();
    expect(editor(), contains('"method": "tools/call"'));
    expect(editor(), contains('"jsonrpc": "2.0"'));
    expect(editor(), contains('"name": "hello"'));

    // A2A → the Message{parts, ...} envelope.
    await tester.tap(find.text('A2A').first);
    await tester.pumpAndSettle();
    expect(editor(), contains('"parts"'));
    expect(editor(), contains('"tool": "hello"'));

    // Malformed JSON disables Send and surfaces the error.
    await tester.ensureVisible(find.byKey(const Key('envelope-editor')));
    await tester.enterText(
      find.byKey(const Key('envelope-editor')),
      '{ not json',
    );
    await tester.pumpAndSettle();
    expect(find.text('Invalid JSON'), findsOneWidget);
    final send = tester.widget<FilledButton>(
      find.widgetWithText(FilledButton, 'Send'),
    );
    expect(
      send.onPressed,
      isNull,
      reason: 'Send must be disabled while the envelope is invalid JSON',
    );
  });
}
