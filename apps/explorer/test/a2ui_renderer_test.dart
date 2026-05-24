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
  });
}
