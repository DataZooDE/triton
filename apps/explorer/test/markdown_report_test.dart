// The `text` component renders light portable markdown (the same subset the
// Google Chat adapter normalises) instead of raw asterisks; the inline
// `report` component renders an open control ONLY when no sibling resource
// button already provides the (auto-opening) affordance.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/widgets/a2ui/a2ui_v08_renderer.dart';
import 'package:triton_explorer/widgets/a2ui/a2ui_v09_renderer.dart';
import 'package:triton_explorer/widgets/a2ui/markdown_lite.dart';

void main() {
  testWidgets('MarkdownLite renders bold/bullets/links without raw markers',
      (tester) async {
    await tester.pumpWidget(const MaterialApp(
      home: Scaffold(
        body: MarkdownLite(
            'Top: **Initech** leads.\n- widgets: 5751\n- gadgets: 3250\nSee [the report](https://example.com/r).'),
      ),
    ));

    // No raw markdown markers survive.
    expect(find.textContaining('**'), findsNothing);
    expect(find.textContaining(']('), findsNothing);
    // The bullet glyph and the link text render.
    expect(find.textContaining('•'), findsNWidgets(2));
    expect(find.text('the report'), findsOneWidget);
    // The url rides a tooltip, not a launcher.
    expect(
      find.byWidgetPredicate(
          (w) => w is Tooltip && w.message == 'https://example.com/r'),
      findsOneWidget,
    );
  });

  testWidgets('v0.9: a lone report node renders an open control that dispatches',
      (tester) async {
    final calls = <(String, Map<String, dynamic>)>[];
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: A2UIv09Renderer(
          envelope: const {
            'stream': [
              {'type': 'text', 'text': 'Chart below.'},
              {
                'type': 'report',
                'report_id': 'top-categories',
                'args': {
                  'params': {'category': 'widgets'},
                },
              },
            ],
          },
          onAction: (tool, args) => calls.add((tool, args)),
        ),
      ),
    ));

    await tester.tap(find.text('Open report: top-categories'));
    expect(calls.single.$1, 'render_report');
    expect(calls.single.$2['report_id'], 'top-categories');
    expect((calls.single.$2['params'] as Map)['category'], 'widgets');
  });

  testWidgets(
      'v0.9: the report node is suppressed next to a resource-carrying button',
      (tester) async {
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: A2UIv09Renderer(
          envelope: const {
            'stream': [
              {'type': 'report', 'report_id': 'top-categories', 'args': {}},
              {
                'type': 'button',
                'label': 'Open report: top-categories',
                'action': {'tool': 'render_report', 'args': {}},
                'resource': 'ui://peacock/top-categories',
              },
            ],
          },
          onAction: (_, _) {},
        ),
      ),
    ));

    // Exactly ONE open affordance: the resource button (which hosts
    // auto-open); the report node shrank away.
    expect(find.text('Open report: top-categories'), findsOneWidget);
    expect(find.byType(FilledButton), findsOneWidget);
  });

  testWidgets('v0.8: Report node renders and dispatches when alone',
      (tester) async {
    final calls = <(String, Map<String, dynamic>)>[];
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: A2UIv08Renderer(
          envelope: const {
            'stream': [
              {
                'Component': {
                  'Report': {
                    'report_id': 'nba-report',
                    'args': {
                      'params': {'account': 'initech-corp'},
                    },
                  },
                },
              },
            ],
          },
          onAction: (tool, args) => calls.add((tool, args)),
        ),
      ),
    ));

    await tester.tap(find.text('Open report: nba-report'));
    expect(calls.single.$1, 'render_report');
    expect(calls.single.$2['report_id'], 'nba-report');
  });
}
