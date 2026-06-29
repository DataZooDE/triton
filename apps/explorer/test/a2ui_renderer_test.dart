// Golden-ish tests for the two A2UI renderers. The fixture envelopes
// match the JSON Triton's builders emit (see
// `crates/triton-core/src/a2ui/v08.rs` and `v09.rs`). When Triton
// adds a new component kind to those builders, both this test and
// the renderer get a matching node.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/widgets/a2ui/a2ui_renderer.dart';
import 'package:triton_explorer/widgets/a2ui/a2ui_v08_renderer.dart';
import 'package:triton_explorer/widgets/a2ui/a2ui_v09_renderer.dart';

void main() {
  group('A2UIv08Renderer', () {
    testWidgets('renders Text, Narration, Button', (tester) async {
      String? firedTool;
      Map<String, dynamic>? firedArgs;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv08Renderer(
            envelope: const {
              'version': '0.8',
              'stream': [
                {
                  'Component': {
                    'Text': {'text': 'hello'},
                  }
                },
                {
                  'Component': {
                    'Narration': {'text': 'thinking...'},
                  }
                },
                {
                  'Component': {
                    'Button': {
                      'label': 'go',
                      'action': {
                        'tool': 'echo',
                        'args': {'msg': 'x'},
                      },
                    },
                  }
                },
              ],
            },
            onAction: (tool, args) {
              firedTool = tool;
              firedArgs = args;
            },
          ),
        ),
      ));
      expect(find.text('hello'), findsOneWidget);
      expect(find.text('thinking...'), findsOneWidget);
      expect(find.widgetWithText(FilledButton, 'go'), findsOneWidget);

      await tester.tap(find.widgetWithText(FilledButton, 'go'));
      await tester.pump();
      expect(firedTool, 'echo');
      expect(firedArgs?['msg'], 'x');
    });

    testWidgets('unknown kind shows a debug card', (tester) async {
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv08Renderer(
            envelope: const {
              'version': '0.8',
              'stream': [
                {
                  'Component': {'Mystery': {}},
                },
              ],
            },
          ),
        ),
      ));
      expect(find.textContaining('unknown v0.8 component kind: Mystery'),
          findsOneWidget);
    });
  });

  group('A2UIv09Renderer', () {
    testWidgets('renders flat lowercase typed nodes', (tester) async {
      String? firedTool;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv09Renderer(
            envelope: const {
              'version': '0.9',
              'stream': [
                {'type': 'text', 'text': 'hello'},
                {'type': 'narration', 'text': 'thinking...'},
                {
                  'type': 'button',
                  'label': 'go',
                  'action': {'tool': 'echo', 'args': {}},
                },
              ],
            },
            onAction: (tool, _) => firedTool = tool,
          ),
        ),
      ));
      expect(find.text('hello'), findsOneWidget);
      expect(find.text('thinking...'), findsOneWidget);
      expect(find.widgetWithText(FilledButton, 'go'), findsOneWidget);
      await tester.tap(find.widgetWithText(FilledButton, 'go'));
      await tester.pump();
      expect(firedTool, 'echo');
    });

    testWidgets('unknown type shows a debug card', (tester) async {
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv09Renderer(
            envelope: const {
              'version': '0.9',
              'stream': [
                {'type': 'mystery'},
              ],
            },
          ),
        ),
      ));
      expect(find.textContaining('unknown v0.9 type: mystery'), findsOneWidget);
    });

    testWidgets('renders report kinds: kpi, table, vega(png)', (tester) async {
      // 1x1 transparent PNG.
      const png =
          'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==';
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv09Renderer(
            envelope: const {
              'version': '0.9',
              'stream': [
                {'type': 'kpi', 'label': 'Total revenue', 'value': '\$1.2M'},
                {
                  'type': 'table',
                  'columns': ['month', 'revenue'],
                  'rows': [
                    ['1997-01', '1000'],
                    ['1997-02', '1200'],
                  ],
                },
                {'type': 'vega', 'title': 'Revenue', 'png_base64': png},
              ],
            },
          ),
        ),
      ));
      expect(find.text('Total revenue'), findsOneWidget);
      expect(find.text('\$1.2M'), findsOneWidget);
      expect(find.byType(DataTable), findsOneWidget);
      expect(find.text('1997-02'), findsOneWidget);
      expect(find.byType(Image), findsOneWidget);
    });

    testWidgets('vega without png shows a placeholder', (tester) async {
      await tester.pumpWidget(const MaterialApp(
        home: Scaffold(
          body: A2UIv09Renderer(
            envelope: {
              'version': '0.9',
              'stream': [
                {'type': 'vega', 'title': 'Revenue'},
              ],
            },
          ),
        ),
      ));
      expect(find.textContaining('chart (open the embedded report'),
          findsOneWidget);
    });
  });

  group('A2UIRenderer dispatch', () {
    testWidgets('routes v0.8 to v0.8 renderer', (tester) async {
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIRenderer(envelope: const {
            'version': '0.8',
            'stream': [
              {
                'Component': {
                  'Text': {'text': 'v8'},
                }
              },
            ],
          }),
        ),
      ));
      expect(find.text('v8'), findsOneWidget);
    });

    testWidgets('routes v0.9 to v0.9 renderer', (tester) async {
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIRenderer(envelope: const {
            'version': '0.9',
            'stream': [
              {'type': 'text', 'text': 'v9'},
            ],
          }),
        ),
      ));
      expect(find.text('v9'), findsOneWidget);
    });

    testWidgets('unknown version surfaces a clear message', (tester) async {
      await tester.pumpWidget(const MaterialApp(
        home: Scaffold(
          body: A2UIRenderer(envelope: {'version': '0.99'}),
        ),
      ));
      expect(find.textContaining('Unknown A2UI version: 0.99'), findsOneWidget);
    });

    testWidgets('unwraps result-nested envelope with explicit version',
        (tester) async {
      // The real wire shape: stream under `result`, no version field.
      await tester.pumpWidget(const MaterialApp(
        home: Scaffold(
          body: A2UIRenderer(
            version: '0.9',
            envelope: {
              'latency_ms': 0,
              'result': {
                'stream': [
                  {'type': 'text', 'text': 'nested-v9'},
                ],
              },
            },
          ),
        ),
      ));
      expect(find.text('nested-v9'), findsOneWidget);
    });

    testWidgets('sniffs v0.8 from Component wrapper when version absent',
        (tester) async {
      await tester.pumpWidget(const MaterialApp(
        home: Scaffold(
          body: A2UIRenderer(
            envelope: {
              'result': {
                'stream': [
                  {
                    'Component': {
                      'Text': {'text': 'sniffed-v8'},
                    }
                  },
                ],
              },
            },
          ),
        ),
      ));
      expect(find.text('sniffed-v8'), findsOneWidget);
    });
  });
}
