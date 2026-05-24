import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/widgets/json_schema_form.dart';

void main() {
  group('JsonSchemaForm', () {
    testWidgets('renders text + integer + boolean + enum primitives',
        (tester) async {
      Map<String, dynamic>? captured;
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: JsonSchemaForm(
            onChanged: (v) => captured = v,
            schema: const {
              'type': 'object',
              'required': ['msg'],
              'properties': {
                'msg': {'type': 'string', 'description': 'message body'},
                'count': {'type': 'integer'},
                'loud': {'type': 'boolean'},
                'mode': {'type': 'string', 'enum': ['fast', 'safe']},
              },
            },
          ),
        ),
      ));
      // Required field marker
      expect(find.text('msg *'), findsOneWidget);
      expect(find.text('count'), findsOneWidget);
      expect(find.text('mode'), findsOneWidget);
      // Boolean renders a switch
      expect(find.byType(SwitchListTile), findsOneWidget);
      // Enum renders a dropdown
      expect(find.byType(DropdownButtonFormField<String>), findsOneWidget);

      await tester.enterText(find.widgetWithText(TextFormField, 'msg *'),
          'hello');
      await tester.pump();
      expect(captured?['msg'], 'hello');
    });

    testWidgets('empty schema shows no-args message', (tester) async {
      await tester.pumpWidget(MaterialApp(
        home: Scaffold(
          body: JsonSchemaForm(
            schema: const {'type': 'object'},
            onChanged: (_) {},
          ),
        ),
      ));
      expect(find.text('Tool takes no arguments.'), findsOneWidget);
    });
  });
}
