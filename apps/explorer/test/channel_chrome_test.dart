// Per-channel visual chrome: a rendered channel preview should look like the
// product, not a generic card. Messaging channels get chat bubbles; card
// channels (Teams/Copilot/Google Chat) get an Adaptive-Card-style card with
// an accent bar + markdown; Gemini gets an answer card that renders markdown
// tables. Each chrome carries a stable `channel-chrome-<adapter>` key so the
// console can tell which shell is on screen.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/widgets/a2ui/markdown_lite.dart';
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
}
