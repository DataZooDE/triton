import 'package:flutter/material.dart';

/// Minimal JSON Schema renderer for the playground. Handles the
/// shapes Triton's in-process tools currently emit:
///   - object with required properties
///   - string / integer / number / boolean primitives
///   - enum (string-valued)
///
/// Out of scope for V1: nested objects, arrays, oneOf/anyOf,
/// $ref. When Triton ships a tool that needs them, the missing
/// piece lands as a focused follow-up.
class JsonSchemaForm extends StatefulWidget {
  const JsonSchemaForm({
    super.key,
    required this.schema,
    required this.onChanged,
  });

  final Map<String, dynamic> schema;
  final void Function(Map<String, dynamic>) onChanged;

  @override
  State<JsonSchemaForm> createState() => _JsonSchemaFormState();
}

class _JsonSchemaFormState extends State<JsonSchemaForm> {
  final _values = <String, dynamic>{};

  @override
  Widget build(BuildContext context) {
    final props = (widget.schema['properties'] as Map?)
            ?.cast<String, Map<String, dynamic>>() ??
        <String, Map<String, dynamic>>{};
    final required =
        ((widget.schema['required'] as List?) ?? const []).cast<String>().toSet();
    if (props.isEmpty) {
      return const Padding(
        padding: EdgeInsets.symmetric(vertical: 8),
        child: Text('Tool takes no arguments.'),
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final entry in props.entries)
          _field(context, entry.key, entry.value, required.contains(entry.key)),
      ],
    );
  }

  void _set(String name, dynamic value) {
    setState(() {
      if (value == null || (value is String && value.isEmpty)) {
        _values.remove(name);
      } else {
        _values[name] = value;
      }
    });
    widget.onChanged(Map.unmodifiable(_values));
  }

  Widget _field(BuildContext context, String name, Map<String, dynamic> schema,
      bool isRequired) {
    final type = schema['type'] as String?;
    final description = schema['description'] as String?;
    final label = isRequired ? '$name *' : name;
    final hint = description ?? '';
    final enumValues = (schema['enum'] as List?)?.cast<String>();

    Widget control;
    if (enumValues != null && enumValues.isNotEmpty) {
      control = DropdownButtonFormField<String>(
        decoration: InputDecoration(labelText: label, helperText: hint),
        items: [
          for (final v in enumValues) DropdownMenuItem(value: v, child: Text(v)),
        ],
        onChanged: (v) => _set(name, v),
      );
    } else if (type == 'boolean') {
      control = SwitchListTile(
        title: Text(label),
        subtitle: hint.isNotEmpty ? Text(hint) : null,
        value: _values[name] as bool? ?? false,
        onChanged: (v) => _set(name, v),
      );
    } else if (type == 'integer' || type == 'number') {
      control = TextFormField(
        decoration: InputDecoration(labelText: label, helperText: hint),
        keyboardType: const TextInputType.numberWithOptions(decimal: true),
        onChanged: (v) {
          if (v.isEmpty) {
            _set(name, null);
            return;
          }
          final parsed =
              type == 'integer' ? int.tryParse(v) : num.tryParse(v);
          _set(name, parsed);
        },
      );
    } else {
      // Default: string field.
      control = TextFormField(
        decoration: InputDecoration(labelText: label, helperText: hint),
        onChanged: (v) => _set(name, v),
      );
    }
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: control,
    );
  }
}
