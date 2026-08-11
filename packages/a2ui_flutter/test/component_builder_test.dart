// The seam that makes this a package rather than a moved folder.
//
// An embedding host has two needs the Explorer never had: kinds the wire
// vocabulary does not define (Heron's `diff` on an approval, Peacock's
// `vega`-with-a-live-chart in its iframe runtime), and the occasional
// built-in it wants to draw its own way. Both are the same hook: a builder
// consulted before the built-in switch, where returning null means "I have no
// opinion, carry on".
//
// Without it, a host with one extra kind has to fork the renderer, and a fork
// is exactly the silent drift this package exists to prevent.

import 'package:a2ui_flutter/a2ui_flutter.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

Future<void> pump(
  WidgetTester tester,
  Map<String, dynamic> envelope, {
  A2uiComponentBuilder? componentBuilder,
}) async {
  await tester.pumpWidget(MaterialApp(
    home: Scaffold(
      body: A2UIRenderer(
        envelope: envelope,
        componentBuilder: componentBuilder,
      ),
    ),
  ));
  await tester.pump();
}

/// A v0.9 stream carrying a kind the vocabulary does not define.
Map<String, dynamic> get _hostKindV09 => {
      'version': '0.9',
      'stream': [
        {'type': 'diff', 'summary': 'three lines changed'},
      ],
    };

/// The same, in the v0.8 envelope shape.
Map<String, dynamic> get _hostKindV08 => {
      'version': '0.8',
      'stream': [
        {
          'Component': {
            'Diff': {'summary': 'three lines changed'},
          },
        },
      ],
    };

void main() {
  group('componentBuilder — v0.9', () {
    testWidgets('renders a host-supplied kind, and is handed the node data',
        (tester) async {
      // The node's own fields must reach the host: a hook that only says
      // "something unknown was here" cannot render anything useful, which is
      // the limitation a host working around it would fork over.
      await pump(
        tester,
        _hostKindV09,
        componentBuilder: (context, node) => node['type'] == 'diff'
            ? Text('diff: ${node['summary']}')
            : null,
      );
      expect(find.text('diff: three lines changed'), findsOneWidget);
    });

    testWidgets('without the builder the same node degrades to unknown-kind',
        (tester) async {
      // The control for the test above: proves the kind really is unknown to
      // the package, so the builder is what rendered it and not a built-in.
      await pump(tester, _hostKindV09);
      expect(find.textContaining('unknown v0.9 type: diff'), findsOneWidget);
      expect(find.textContaining('three lines changed'), findsNothing);
    });

    testWidgets('overrides a built-in kind', (tester) async {
      const env = {
        'version': '0.9',
        'stream': [
          {'type': 'text', 'text': 'the built-in rendering'},
        ],
      };

      await pump(
        tester,
        env,
        componentBuilder: (context, node) =>
            node['type'] == 'text' ? const Text('the host rendering') : null,
      );
      expect(find.text('the host rendering'), findsOneWidget);
      expect(find.text('the built-in rendering'), findsNothing);

      // Control: the same envelope with no builder does render the built-in,
      // so the absence above is the override and not a broken fixture.
      await pump(tester, env);
      expect(find.text('the built-in rendering'), findsOneWidget);
    });

    testWidgets('returning null falls through to the built-in', (tester) async {
      // A host opts in per node. One opinion must not cost it the rest of the
      // vocabulary.
      await pump(
        tester,
        const {
          'version': '0.9',
          'stream': [
            {'type': 'text', 'text': 'still built-in'},
            {'type': 'narration', 'text': 'claimed by the host'},
          ],
        },
        componentBuilder: (context, node) => node['type'] == 'narration'
            ? const Text('host narration')
            : null,
      );
      expect(find.text('still built-in'), findsOneWidget);
      expect(find.text('host narration'), findsOneWidget);
      expect(find.text('claimed by the host'), findsNothing);
    });
  });

  group('componentBuilder — v0.8', () {
    testWidgets('the seam exists in both version trees', (tester) async {
      // ADR-4 keeps the trees isolated, which makes it easy to add an
      // affordance to one and forget the other. A host that works against
      // v0.9 and silently loses its components on v0.8 is worse than no hook.
      await pump(
        tester,
        _hostKindV08,
        componentBuilder: (context, node) {
          final inner = (node['Component'] as Map?)?.cast<String, dynamic>();
          final diff = inner?['Diff'] as Map?;
          return diff == null ? null : Text('diff: ${diff['summary']}');
        },
      );
      expect(find.text('diff: three lines changed'), findsOneWidget);
    });

    testWidgets('without the builder the v0.8 node degrades to unknown-kind',
        (tester) async {
      await pump(tester, _hostKindV08);
      expect(find.textContaining('three lines changed'), findsNothing);
      // Positive control: something rendered, and it is the debug card the
      // Explorer relies on to show an operator what is missing.
      expect(find.byType(Card), findsWidgets);
    });
  });
}
