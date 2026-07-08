// Golden tests for the rich A2UI components — Selection / Form /
// Dashboard — in both v0.8 and v0.9. Each fixture envelope mirrors
// the JSON Triton emits today (see crates/triton-core/src/a2ui/).

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/widgets/a2ui/a2ui_v08_renderer.dart';
import 'package:triton_explorer/widgets/a2ui/a2ui_v09_renderer.dart';

const _v08Selection = {
  'version': '0.8',
  'stream': [
    {
      'Component': {
        'Selection': {
          'prompt': 'Pick one',
          'options': [
            {'label': 'Apple', 'value': 'a'},
            {'label': 'Banana', 'value': 'b'},
          ],
          'action': {'tool': 'narrate', 'args_key': 'subject'},
        },
      },
    },
  ],
};

const _v09Selection = {
  'version': '0.9',
  'stream': [
    {
      'type': 'selection',
      'prompt': 'Pick one',
      'options': [
        {'label': 'Apple', 'value': 'a'},
        {'label': 'Banana', 'value': 'b'},
      ],
      'tool': 'narrate',
      'args_key': 'subject',
    },
  ],
};

const _v08Form = {
  'version': '0.8',
  'stream': [
    {
      'Component': {
        'Form': {
          'title': 'Feedback',
          'submit_label': 'Send',
          'fields': [
            {
              'name': 'name',
              'label': 'Your name',
              'kind': 'string',
              'required': true,
            },
            {
              'name': 'rating',
              'label': 'Rating',
              'kind': 'integer',
              'required': true,
            },
            {
              'name': 'ok',
              'label': 'OK to follow up?',
              'kind': 'boolean',
              'required': false,
            },
          ],
          'action': {'tool': 'echo'},
        },
      },
    },
  ],
};

const _v09Form = {
  'version': '0.9',
  'stream': [
    {
      'type': 'form',
      'title': 'Feedback',
      'submit_label': 'Send',
      'fields': [
        {
          'name': 'name',
          'label': 'Your name',
          'kind': 'string',
          'required': true,
        },
      ],
      'tool': 'echo',
    },
  ],
};

const _v08Dashboard = {
  'version': '0.8',
  'stream': [
    {
      'Component': {
        'Dashboard': {
          'title': 'Last hour',
          'tiles': [
            {'label': 'invocations', 'value': '1,284'},
            {'label': 'p95 latency', 'value': '84 ms'},
            {'label': 'errors', 'value': '3', 'trend': '-2 vs prior'},
          ],
        },
      },
    },
  ],
};

const _v09Dashboard = {
  'version': '0.9',
  'stream': [
    {
      'type': 'dashboard',
      'title': 'Last hour',
      'tiles': [
        {'label': 'invocations', 'value': '1,284'},
        {'label': 'errors', 'value': '3', 'trend': '-2 vs prior'},
      ],
    },
  ],
};

const _v08Buttons = {
  'version': '0.8',
  'stream': [
    {
      'Component': {
        'Text': {'text': 'Here you go.'},
      },
    },
    {
      'Component': {
        'Button': {
          'label': 'More detail',
          'action': {
            'tool': 'ask',
            'args': {'q': 'detail'},
          },
        },
      },
    },
    {
      'Component': {
        'Button': {
          'label': 'Summarize',
          'action': {
            'tool': 'ask',
            'args': {'q': 'summary'},
          },
        },
      },
    },
    {
      'Component': {
        'Button': {
          'label': 'Chart it',
          'action': {
            'tool': 'ask',
            'args': {'q': 'chart'},
          },
        },
      },
    },
  ],
};

const _v09Buttons = {
  'version': '0.9',
  'stream': [
    {'type': 'text', 'text': 'Here you go.'},
    {
      'type': 'button',
      'label': 'More detail',
      'action': {
        'tool': 'ask',
        'args': {'q': 'detail'},
      },
    },
    {
      'type': 'button',
      'label': 'Summarize',
      'action': {
        'tool': 'ask',
        'args': {'q': 'summary'},
      },
    },
    {
      'type': 'button',
      'label': 'Chart it',
      'action': {
        'tool': 'ask',
        'args': {'q': 'chart'},
      },
    },
  ],
};

void main() {
  group('A2UIv08 rich components', () {
    testWidgets('Selection emits args_key when chip is tapped',
        (tester) async {
      String? tool;
      Map<String, dynamic>? args;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv08Renderer(
            envelope: _v08Selection,
            onAction: (t, a) {
              tool = t;
              args = a;
            },
          ),
        ),
      ));
      expect(find.text('Pick one'), findsOneWidget);
      expect(find.widgetWithText(ChoiceChip, 'Apple'), findsOneWidget);
      await tester.tap(find.widgetWithText(ChoiceChip, 'Banana'));
      await tester.pump();
      expect(tool, 'narrate');
      expect(args, {'subject': 'b'});
    });

    testWidgets('Form submits collected values', (tester) async {
      String? tool;
      Map<String, dynamic>? values;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv08Renderer(
            envelope: _v08Form,
            onAction: (t, v) {
              tool = t;
              values = v;
            },
          ),
        ),
      ));
      expect(find.text('Feedback'), findsOneWidget);
      await tester.enterText(
          find.widgetWithText(TextField, 'Your name *'), 'Alex');
      await tester.enterText(
          find.widgetWithText(TextField, 'Rating *'), '5');
      await tester.tap(find.widgetWithText(SwitchListTile, 'OK to follow up?'));
      await tester.pump();
      await tester.tap(find.widgetWithText(FilledButton, 'Send'));
      await tester.pump();
      expect(tool, 'echo');
      expect(values, {'name': 'Alex', 'rating': 5, 'ok': true});
    });

    testWidgets('Dashboard renders tile labels, values, trends',
        (tester) async {
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv08Renderer(envelope: _v08Dashboard),
        ),
      ));
      expect(find.text('Last hour'), findsOneWidget);
      expect(find.text('invocations'), findsOneWidget);
      expect(find.text('1,284'), findsOneWidget);
      expect(find.text('-2 vs prior'), findsOneWidget);
    });

    testWidgets('follow-up buttons share one compact horizontal Wrap',
        (tester) async {
      String? tool;
      Map<String, dynamic>? args;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv08Renderer(
            envelope: _v08Buttons,
            onAction: (t, a) {
              tool = t;
              args = a;
            },
          ),
        ),
      ));
      // The three follow-ups collapse into a single Wrap (not a Column of
      // full-width buttons), and stay tappable.
      expect(find.byType(Wrap), findsOneWidget);
      expect(
        find.descendant(
          of: find.byType(Wrap),
          matching: find.byType(FilledButton),
        ),
        findsNWidgets(3),
      );
      final style = tester
          .widget<FilledButton>(find.widgetWithText(FilledButton, 'Summarize'))
          .style;
      expect(style?.tapTargetSize, MaterialTapTargetSize.shrinkWrap);
      await tester.tap(find.widgetWithText(FilledButton, 'Summarize'));
      await tester.pump();
      expect(tool, 'ask');
      expect(args, {'q': 'summary'});
    });
  });

  group('A2UIv09 rich components', () {
    testWidgets('Selection emits args_key via SegmentedButton',
        (tester) async {
      String? tool;
      Map<String, dynamic>? args;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv09Renderer(
            envelope: _v09Selection,
            onAction: (t, a) {
              tool = t;
              args = a;
            },
          ),
        ),
      ));
      expect(find.byType(SegmentedButton<String>), findsOneWidget);
      await tester.tap(find.text('Apple'));
      await tester.pump();
      expect(tool, 'narrate');
      expect(args, {'subject': 'a'});
    });

    testWidgets('Form invokes tool with values', (tester) async {
      String? tool;
      Map<String, dynamic>? values;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv09Renderer(
            envelope: _v09Form,
            onAction: (t, v) {
              tool = t;
              values = v;
            },
          ),
        ),
      ));
      await tester.enterText(
          find.widgetWithText(TextField, 'Your name *'), 'Bea');
      await tester.tap(find.widgetWithText(FilledButton, 'Send'));
      await tester.pump();
      expect(tool, 'echo');
      expect(values, {'name': 'Bea'});
    });

    testWidgets('Dashboard renders tiles', (tester) async {
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv09Renderer(envelope: _v09Dashboard),
        ),
      ));
      expect(find.text('invocations'), findsOneWidget);
      expect(find.text('-2 vs prior'), findsOneWidget);
    });

    testWidgets('follow-up buttons share one compact horizontal Wrap',
        (tester) async {
      String? tool;
      Map<String, dynamic>? args;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: A2UIv09Renderer(
            envelope: _v09Buttons,
            onAction: (t, a) {
              tool = t;
              args = a;
            },
          ),
        ),
      ));
      expect(find.byType(Wrap), findsOneWidget);
      expect(
        find.descendant(
          of: find.byType(Wrap),
          matching: find.byType(FilledButton),
        ),
        findsNWidgets(3),
      );
      final style = tester
          .widget<FilledButton>(find.widgetWithText(FilledButton, 'Chart it'))
          .style;
      expect(style?.tapTargetSize, MaterialTapTargetSize.shrinkWrap);
      await tester.tap(find.widgetWithText(FilledButton, 'Chart it'));
      await tester.pump();
      expect(tool, 'ask');
      expect(args, {'q': 'chart'});
    });
  });
}
