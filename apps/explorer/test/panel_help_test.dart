// The per-tool guide copy and the shared PanelHelp banner.
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/models.dart';
import 'package:triton_explorer/widgets/panel_help.dart';

ToolDescriptor _tool(String name,
        {bool a2ui = false, Map<String, dynamic>? schema}) =>
    ToolDescriptor(
        name: name,
        inputSchema: schema ?? const {},
        returnsA2ui: a2ui);

void main() {
  group('toolGuide', () {
    test('curated copy for bundled demo tools', () {
      final demo = toolGuide(_tool('demo_panel', a2ui: true));
      expect(demo.what, contains('component vocabulary'));
      expect(demo.how, contains('Send'));

      final echo = toolGuide(_tool('echo'));
      expect(echo.what.toLowerCase(), contains('simplest'));
    });

    test('generic fallback adapts to schema + returns_a2ui', () {
      final unknown = toolGuide(_tool('future_tool',
          a2ui: true,
          schema: {
            'properties': {'x': {}}
          }));
      expect(unknown.what, contains('discovered live'));
      expect(unknown.how, contains('arguments')); // has args
      expect(unknown.how, contains('A2UI')); // returns_a2ui
    });
  });

  testWidgets('PanelHelp shows what + how', (tester) async {
    await tester.pumpWidget(const MaterialApp(
      home: Scaffold(body: PanelHelp(what: 'What this does', how: 'Do this')),
    ));
    expect(find.text('What this does'), findsOneWidget);
    expect(find.textContaining('How to use'), findsOneWidget);
    expect(find.textContaining('Do this'), findsOneWidget);
  });
}
