// The `diff` component in v0.9 — BR-HIL-1's readable diff of the page an
// approval would write. The fixture mirrors what
// `crates/triton-core/src/a2ui/v09.rs` emits.
//
// The default renderer's job here is modest: show the change, and do not
// silently swallow the folded context. A host with a design system of its own
// (Heron's phone app) overrides the node via `componentBuilder` — this is the
// fallback every other host gets.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:a2ui_flutter/a2ui_flutter.dart';

const _envelope = {
  'version': '0.9',
  'stream': [
    {
      'type': 'diff',
      'lines': [
        {'op': 'ctx', 'text': 'tier: enterprise'},
        {
          'op': 'fold',
          'count': 42,
          'hidden': [
            {'op': 'ctx', 'text': 'buried line'},
          ],
        },
        {'op': 'del', 'text': 'status: prospect'},
        {'op': 'add', 'text': 'status: active'},
      ],
    },
  ],
};

Future<void> pump(WidgetTester tester, Map<String, dynamic> envelope) async {
  await tester.pumpWidget(MaterialApp(
    home: Scaffold(body: A2UIv09Renderer(envelope: envelope)),
  ));
  await tester.pump();
}

void main() {
  testWidgets('a diff renders its changed lines and folds the rest',
      (tester) async {
    await pump(tester, _envelope);

    // The change being approved is visible without any interaction.
    expect(find.textContaining('status: active'), findsOneWidget);
    expect(find.textContaining('status: prospect'), findsOneWidget);
    expect(find.textContaining('tier: enterprise'), findsOneWidget);
    // The fold is announced with its count, and its lines stay hidden.
    expect(find.textContaining('42'), findsOneWidget);
    expect(find.textContaining('buried line'), findsNothing);
    // Positive control for that negative: it is not an unknown-type card.
    expect(find.textContaining('unknown v0.9 type'), findsNothing);
  });

  testWidgets('a host may take the diff node over entirely', (tester) async {
    // The `componentBuilder` seam: Heron renders diffs with its own tokens,
    // and must be able to do so without owning the rest of the vocabulary.
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: A2UIv09Renderer(
          envelope: _envelope,
          componentBuilder: (context, node) =>
              node['type'] == 'diff' ? const Text('HOST DIFF') : null,
        ),
      ),
    ));
    await tester.pump();
    expect(find.text('HOST DIFF'), findsOneWidget);
    // Positive control: the built-in rendering really was displaced.
    expect(find.textContaining('status: active'), findsNothing);
  });
}
