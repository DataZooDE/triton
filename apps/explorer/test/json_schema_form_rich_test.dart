// Tests for the rich JsonSchemaForm shapes: nested objects, arrays,
// and local $ref resolution. The primitive-only tests live in
// json_schema_form_test.dart and stay green untouched.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/widgets/json_schema_form.dart';

void main() {
  group('JsonSchemaForm — nested objects', () {
    testWidgets('emits {parent: {child: ...}} after editing nested field',
        (tester) async {
      Map<String, dynamic>? captured;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: SingleChildScrollView(
            child: JsonSchemaForm(
              onChanged: (v) => captured = v,
              schema: const {
                'type': 'object',
                'properties': {
                  'user': {
                    'type': 'object',
                    'properties': {
                      'name': {'type': 'string'},
                      'age': {'type': 'integer'},
                    },
                  },
                },
              },
            ),
          ),
        ),
      ));
      // Nested card title.
      expect(find.text('user'), findsOneWidget);
      await tester.enterText(
          find.widgetWithText(TextFormField, 'name'), 'Alex');
      await tester.pump();
      expect(captured?['user'], isA<Map>());
      expect((captured!['user'] as Map)['name'], 'Alex');
    });
  });

  group('JsonSchemaForm — arrays', () {
    testWidgets('Add / remove builds the right value list', (tester) async {
      Map<String, dynamic>? captured;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: SingleChildScrollView(
            child: JsonSchemaForm(
              onChanged: (v) => captured = v,
              schema: const {
                'type': 'object',
                'properties': {
                  'tags': {
                    'type': 'array',
                    'items': {'type': 'string'},
                  },
                },
              },
            ),
          ),
        ),
      ));
      // Add two rows.
      await tester.tap(find.byIcon(Icons.add_circle_outline));
      await tester.pump();
      await tester.tap(find.byIcon(Icons.add_circle_outline));
      await tester.pump();
      final items = find.widgetWithText(TextField, 'item');
      expect(items, findsNWidgets(2));
      await tester.enterText(items.at(0), 'rust');
      await tester.enterText(items.at(1), 'flutter');
      await tester.pump();
      expect(captured?['tags'], ['rust', 'flutter']);

      // Remove first row.
      await tester.tap(find.byIcon(Icons.remove_circle_outline).first);
      await tester.pump();
      expect(captured?['tags'], ['flutter']);
    });
  });

  group('JsonSchemaForm — \$ref', () {
    testWidgets('resolves local #/definitions/<name> ref', (tester) async {
      Map<String, dynamic>? captured;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: SingleChildScrollView(
            child: JsonSchemaForm(
              onChanged: (v) => captured = v,
              schema: const {
                'type': 'object',
                'definitions': {
                  'Address': {
                    'type': 'object',
                    'properties': {
                      'city': {'type': 'string'},
                    },
                  },
                },
                'properties': {
                  'home': {r'$ref': '#/definitions/Address'},
                },
              },
            ),
          ),
        ),
      ));
      expect(find.text('home'), findsOneWidget);
      await tester.enterText(
          find.widgetWithText(TextFormField, 'city'), 'Berlin');
      await tester.pump();
      expect((captured!['home'] as Map)['city'], 'Berlin');
    });

    testWidgets(r'broken $ref surfaces a clear error card', (tester) async {
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: JsonSchemaForm(
            onChanged: (_) {},
            schema: const {
              'type': 'object',
              'properties': {
                'thing': {r'$ref': '#/definitions/DoesNotExist'},
              },
            },
          ),
        ),
      ));
      expect(find.textContaining('Could not resolve'), findsOneWidget);
    });

    testWidgets(r'cyclic $ref does not crash the build', (tester) async {
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: JsonSchemaForm(
            onChanged: (_) {},
            schema: const {
              'type': 'object',
              r'$defs': {
                'Self': {r'$ref': r'#/$defs/Self'},
              },
              'properties': {
                'loop': {r'$ref': r'#/$defs/Self'},
              },
            },
          ),
        ),
      ));
      // Just asserting we get a rendered tree without a stack
      // overflow is the test.
      expect(find.textContaining('Could not resolve'), findsOneWidget);
    });
  });
}
