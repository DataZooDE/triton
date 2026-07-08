// The per-message trace sidebar's tool-trace section: the agent's
// `_meta.tool_trace` (which tools ran + the Escurel data each touched) renders
// as one row per call — tool name, target, a read/write glyph, latency — in
// call order, with a clear empty state.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/models.dart';
import 'package:triton_explorer/widgets/trace/trace_view.dart';

Widget _host(Widget child) => MaterialApp(home: Scaffold(body: child));

void main() {
  test('toolTraceFrom lifts MCP _meta and REST/A2A sibling shapes', () {
    // MCP: under the result's `_meta.tool_trace`.
    final mcp = InvocationResult.toolTraceFrom({
      '_meta': {
        'tool_trace': [
          {'tool': 'run_query', 'target': 'top_customers', 'verb': 'read'},
        ],
      },
    });
    expect(mcp, isNotNull);
    expect(mcp!.single['target'], 'top_customers');

    // REST/A2A: a top-level `tool_trace` sibling.
    final rest = InvocationResult.toolTraceFrom({
      'tool_trace': [
        {'tool': 'file_email', 'target': 'email-1', 'verb': 'write'},
      ],
    });
    expect(rest!.single['tool'], 'file_email');

    // Nothing / empty → null (no section to render).
    expect(InvocationResult.toolTraceFrom({}), isNull);
    expect(InvocationResult.toolTraceFrom({'tool_trace': []}), isNull);
    // A scalar blob is ignored, never coerced into rows.
    expect(InvocationResult.toolTraceFrom({'tool_trace': 'gotcha'}), isNull);
  });

  testWidgets('ToolTraceList renders a row per call with tool + target', (
    tester,
  ) async {
    await tester.pumpWidget(
      _host(
        const ToolTraceList(
          calls: [
            {
              'tool': 'search_knowledge',
              'target': 'renewal',
              'verb': 'read',
              'ms': 12,
            },
            {
              'tool': 'flag_follow_up',
              'target': 'initech-corp',
              'verb': 'write',
              'ms': 30,
            },
          ],
        ),
      ),
    );
    await tester.pumpAndSettle();

    expect(find.byKey(const Key('tool-trace-list')), findsOneWidget);
    // Each tool + its Escurel target is shown.
    expect(find.text('search_knowledge'), findsOneWidget);
    expect(find.text('renewal'), findsOneWidget);
    expect(find.text('flag_follow_up'), findsOneWidget);
    expect(find.text('initech-corp'), findsOneWidget);
    // Read vs write render distinct glyphs.
    expect(find.byIcon(Icons.search), findsOneWidget);
    expect(find.byIcon(Icons.edit_note), findsOneWidget);
  });

  testWidgets('ToolTraceList shows an empty state when no calls ran', (
    tester,
  ) async {
    await tester.pumpWidget(_host(const ToolTraceList(calls: null)));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('tool-trace-list')), findsNothing);
    expect(find.textContaining('No tool calls'), findsOneWidget);
  });
}
