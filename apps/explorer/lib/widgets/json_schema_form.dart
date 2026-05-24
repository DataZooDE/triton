import 'package:flutter/material.dart';

/// JSON Schema renderer for the playground. Handles the shapes tools
/// in the wider DataZoo registry actually emit:
///   - string / integer / number / boolean primitives
///   - enum (string-valued)
///   - nested objects (recurses into `properties`)
///   - arrays (add/remove rows; item schema can itself be any of
///     the supported shapes)
///   - `$ref` to a local `#/definitions/<name>` or `#/$defs/<name>`
///     (cycle-safe — repeated visits short-circuit so a malformed
///     schema can't crash the SPA)
///
/// Out of scope: `oneOf` / `anyOf` / external `$ref` (the SPA only
/// resolves within the schema it was handed).
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
  final Map<String, dynamic> _root = <String, dynamic>{};

  /// In-flight `$ref` chain. Used to short-circuit cycles —
  /// `{ "$ref": "#/$defs/Self", "$defs": { "Self": { "$ref": "#/$defs/Self" } } }`
  /// would otherwise loop the build forever.
  final Set<String> _resolving = <String>{};

  void _notify() {
    widget.onChanged(_deepCopyMap(_root));
  }

  Map<String, dynamic> _deepCopyMap(Map<String, dynamic> m) => <String, dynamic>{
        for (final e in m.entries) e.key: _deepCopy(e.value),
      };

  Object? _deepCopy(Object? v) {
    if (v is Map) {
      return _deepCopyMap(v.cast<String, dynamic>());
    }
    if (v is List) {
      return [for (final x in v) _deepCopy(x)];
    }
    return v;
  }

  Map<String, dynamic>? _resolveRef(String ref) {
    // Only support local refs of the canonical forms.
    const prefixes = ['#/definitions/', '#/\$defs/'];
    for (final p in prefixes) {
      if (!ref.startsWith(p)) continue;
      final key = ref.substring(p.length);
      final defs =
          (widget.schema[p == '#/definitions/' ? 'definitions' : r'$defs']
                  as Map?)
              ?.cast<String, dynamic>();
      final found = defs?[key];
      if (found is Map<String, dynamic>) return found;
      if (found is Map) return found.cast<String, dynamic>();
    }
    return null;
  }

  Map<String, dynamic>? _maybeResolve(Map<String, dynamic> schema) {
    final ref = schema[r'$ref'] as String?;
    if (ref == null) return schema;
    if (_resolving.contains(ref)) return null; // cycle
    _resolving.add(ref);
    try {
      final resolved = _resolveRef(ref);
      if (resolved == null) return null;
      // Recurse one level so chains like A -> B -> C resolve.
      return _maybeResolve(resolved);
    } finally {
      _resolving.remove(ref);
    }
  }

  @override
  Widget build(BuildContext context) {
    final resolved = _maybeResolve(widget.schema);
    if (resolved == null) {
      return _refBroken(context, widget.schema[r'$ref'] as String? ?? '?');
    }
    return _objectFields(context, resolved, _root, depth: 0);
  }

  /// Render an object schema's `properties` against the values map
  /// `valuesMap`. Mutations write straight back into `valuesMap`,
  /// which is part of the `_root` tree, so the top-level
  /// `widget.onChanged` always sees the full latest snapshot.
  Widget _objectFields(
    BuildContext context,
    Map<String, dynamic> schema,
    Map<String, dynamic> valuesMap, {
    required int depth,
  }) {
    final props = (schema['properties'] as Map?)?.cast<String, dynamic>() ??
        const <String, dynamic>{};
    final required =
        ((schema['required'] as List?) ?? const []).cast<String>().toSet();
    if (props.isEmpty) {
      if (depth == 0) {
        return const Padding(
          padding: EdgeInsets.symmetric(vertical: 8),
          child: Text('Tool takes no arguments.'),
        );
      }
      return const SizedBox.shrink();
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final e in props.entries)
          _field(
            context,
            e.key,
            (e.value as Map).cast<String, dynamic>(),
            isRequired: required.contains(e.key),
            valuesMap: valuesMap,
            depth: depth,
          ),
      ],
    );
  }

  Widget _field(
    BuildContext context,
    String name,
    Map<String, dynamic> rawSchema, {
    required bool isRequired,
    required Map<String, dynamic> valuesMap,
    required int depth,
  }) {
    final schema = _maybeResolve(rawSchema);
    if (schema == null) {
      return _refBroken(context, rawSchema[r'$ref'] as String? ?? '?');
    }
    final type = schema['type'] as String?;
    final description = schema['description'] as String?;
    final label = isRequired ? '$name *' : name;
    final hint = description ?? '';
    final enumValues = (schema['enum'] as List?)?.cast<String>();

    if (enumValues != null && enumValues.isNotEmpty) {
      return Padding(
        padding: const EdgeInsets.symmetric(vertical: 6),
        child: DropdownButtonFormField<String>(
          decoration: InputDecoration(labelText: label, helperText: hint),
          initialValue: valuesMap[name] as String?,
          items: [
            for (final v in enumValues)
              DropdownMenuItem(value: v, child: Text(v)),
          ],
          onChanged: (v) {
            setState(() {
              if (v == null) {
                valuesMap.remove(name);
              } else {
                valuesMap[name] = v;
              }
            });
            _notify();
          },
        ),
      );
    }

    switch (type) {
      case 'boolean':
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 6),
          child: SwitchListTile(
            title: Text(label),
            subtitle: hint.isNotEmpty ? Text(hint) : null,
            value: valuesMap[name] as bool? ?? false,
            onChanged: (v) {
              setState(() => valuesMap[name] = v);
              _notify();
            },
          ),
        );
      case 'integer':
      case 'number':
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 6),
          child: TextFormField(
            initialValue: valuesMap[name]?.toString(),
            decoration: InputDecoration(labelText: label, helperText: hint),
            keyboardType: const TextInputType.numberWithOptions(decimal: true),
            onChanged: (v) {
              setState(() {
                if (v.isEmpty) {
                  valuesMap.remove(name);
                } else {
                  final parsed =
                      type == 'integer' ? int.tryParse(v) : num.tryParse(v);
                  if (parsed == null) {
                    valuesMap.remove(name);
                  } else {
                    valuesMap[name] = parsed;
                  }
                }
              });
              _notify();
            },
          ),
        );
      case 'object':
        // Nested object: render its properties inside a card so the
        // depth is visually obvious.
        final nested =
            (valuesMap[name] as Map?)?.cast<String, dynamic>() ??
                <String, dynamic>{};
        valuesMap[name] = nested;
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 8),
          child: Card(
            margin: EdgeInsets.zero,
            color: Theme.of(context).colorScheme.surfaceContainerLow,
            child: Padding(
              padding: const EdgeInsets.all(12),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(label,
                      style: Theme.of(context).textTheme.titleSmall),
                  if (hint.isNotEmpty) ...[
                    const SizedBox(height: 2),
                    Text(hint,
                        style: Theme.of(context).textTheme.bodySmall),
                  ],
                  const SizedBox(height: 6),
                  _objectFields(context, schema, nested, depth: depth + 1),
                ],
              ),
            ),
          ),
        );
      case 'array':
        final list = (valuesMap[name] as List?)?.cast<dynamic>() ?? <dynamic>[];
        valuesMap[name] = list;
        return _ArrayField(
          label: label,
          hint: hint,
          itemSchema: (schema['items'] as Map?)?.cast<String, dynamic>() ??
              const <String, dynamic>{},
          values: list,
          renderItem: (idx, itemSchema, val, onChanged) => _ArrayItemHost(
            schema: itemSchema,
            value: val,
            onChanged: onChanged,
            resolveRef: _maybeResolve,
            refBroken: _refBroken,
            buildField: _field,
            buildObject: _objectFields,
            buildArray: _renderInnerArray,
            depth: depth + 1,
            notify: _notify,
          ),
          notify: _notify,
          rebuild: setState,
        );
      default:
        // Treat anything else (string, or no type given) as a free
        // text field.
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 6),
          child: TextFormField(
            initialValue: valuesMap[name] as String?,
            decoration: InputDecoration(labelText: label, helperText: hint),
            onChanged: (v) {
              setState(() {
                if (v.isEmpty) {
                  valuesMap.remove(name);
                } else {
                  valuesMap[name] = v;
                }
              });
              _notify();
            },
          ),
        );
    }
  }

  /// Recursive array helper used when an array contains arrays.
  Widget _renderInnerArray(
    BuildContext context,
    String label,
    Map<String, dynamic> schema,
    List<dynamic> values, {
    required int depth,
  }) {
    return _ArrayField(
      label: label,
      hint: '',
      itemSchema: (schema['items'] as Map?)?.cast<String, dynamic>() ??
          const <String, dynamic>{},
      values: values,
      renderItem: (idx, itemSchema, val, onChanged) => _ArrayItemHost(
        schema: itemSchema,
        value: val,
        onChanged: onChanged,
        resolveRef: _maybeResolve,
        refBroken: _refBroken,
        buildField: _field,
        buildObject: _objectFields,
        buildArray: _renderInnerArray,
        depth: depth + 1,
        notify: _notify,
      ),
      notify: _notify,
      rebuild: setState,
    );
  }

  Widget _refBroken(BuildContext context, String ref) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 6),
        child: Card(
          color: Theme.of(context).colorScheme.errorContainer,
          child: Padding(
            padding: const EdgeInsets.all(8),
            child: Text(
              'Could not resolve \$ref: $ref',
              style: TextStyle(
                color: Theme.of(context).colorScheme.onErrorContainer,
              ),
            ),
          ),
        ),
      );
}

typedef _FieldBuilder = Widget Function(
  BuildContext context,
  String name,
  Map<String, dynamic> schema, {
  required bool isRequired,
  required Map<String, dynamic> valuesMap,
  required int depth,
});

typedef _ObjectBuilder = Widget Function(
  BuildContext context,
  Map<String, dynamic> schema,
  Map<String, dynamic> valuesMap, {
  required int depth,
});

typedef _ArrayBuilder = Widget Function(
  BuildContext context,
  String label,
  Map<String, dynamic> schema,
  List<dynamic> values, {
  required int depth,
});

typedef _RefResolver = Map<String, dynamic>? Function(
    Map<String, dynamic> schema);

typedef _RefBroken = Widget Function(BuildContext context, String ref);

class _ArrayField extends StatelessWidget {
  const _ArrayField({
    required this.label,
    required this.hint,
    required this.itemSchema,
    required this.values,
    required this.renderItem,
    required this.notify,
    required this.rebuild,
  });

  final String label;
  final String hint;
  final Map<String, dynamic> itemSchema;
  final List<dynamic> values;
  final Widget Function(
    int index,
    Map<String, dynamic> itemSchema,
    dynamic value,
    ValueChanged<dynamic> onChanged,
  ) renderItem;
  final VoidCallback notify;
  final void Function(void Function()) rebuild;

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 8),
        child: Card(
          margin: EdgeInsets.zero,
          color: Theme.of(context).colorScheme.surfaceContainerLow,
          child: Padding(
            padding: const EdgeInsets.all(12),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text('$label  (${values.length})',
                              style:
                                  Theme.of(context).textTheme.titleSmall),
                          if (hint.isNotEmpty) ...[
                            const SizedBox(height: 2),
                            Text(hint,
                                style:
                                    Theme.of(context).textTheme.bodySmall),
                          ],
                        ],
                      ),
                    ),
                    IconButton(
                      tooltip: 'Add item',
                      onPressed: () {
                        rebuild(() => values.add(_emptyForSchema(itemSchema)));
                        notify();
                      },
                      icon: const Icon(Icons.add_circle_outline),
                    ),
                  ],
                ),
                const SizedBox(height: 6),
                for (var i = 0; i < values.length; i++)
                  Padding(
                    padding: const EdgeInsets.symmetric(vertical: 4),
                    child: Row(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Expanded(
                          child: renderItem(
                            i,
                            itemSchema,
                            values[i],
                            (v) {
                              rebuild(() => values[i] = v);
                              notify();
                            },
                          ),
                        ),
                        IconButton(
                          tooltip: 'Remove',
                          onPressed: () {
                            rebuild(() => values.removeAt(i));
                            notify();
                          },
                          icon: const Icon(Icons.remove_circle_outline),
                        ),
                      ],
                    ),
                  ),
              ],
            ),
          ),
        ),
      );
}

dynamic _emptyForSchema(Map<String, dynamic> schema) {
  switch (schema['type']) {
    case 'object':
      return <String, dynamic>{};
    case 'array':
      return <dynamic>[];
    case 'boolean':
      return false;
    case 'integer':
    case 'number':
      return 0;
    default:
      return '';
  }
}

/// Renders a single array item against an item schema. Bridges
/// between the in-list `dynamic value` and the form's
/// `valuesMap`-shaped renderers by wrapping primitive items in a
/// one-key map under `__item`.
class _ArrayItemHost extends StatefulWidget {
  const _ArrayItemHost({
    required this.schema,
    required this.value,
    required this.onChanged,
    required this.resolveRef,
    required this.refBroken,
    required this.buildField,
    required this.buildObject,
    required this.buildArray,
    required this.depth,
    required this.notify,
  });

  final Map<String, dynamic> schema;
  final dynamic value;
  final ValueChanged<dynamic> onChanged;
  final _RefResolver resolveRef;
  final _RefBroken refBroken;
  final _FieldBuilder buildField;
  final _ObjectBuilder buildObject;
  final _ArrayBuilder buildArray;
  final int depth;
  final VoidCallback notify;

  @override
  State<_ArrayItemHost> createState() => _ArrayItemHostState();
}

class _ArrayItemHostState extends State<_ArrayItemHost> {
  late TextEditingController _ctrl;

  @override
  void initState() {
    super.initState();
    _ctrl = TextEditingController(text: widget.value?.toString() ?? '');
  }

  @override
  void dispose() {
    _ctrl.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final schema = widget.resolveRef(widget.schema);
    if (schema == null) {
      return widget
          .refBroken(context, widget.schema[r'$ref'] as String? ?? '?');
    }
    final type = schema['type'] as String?;
    switch (type) {
      case 'object':
        final map = (widget.value as Map?)?.cast<String, dynamic>() ??
            <String, dynamic>{};
        if (widget.value is! Map) {
          // The list was bootstrapped with the wrong shape; rewrite
          // it so future rebuilds see a Map.
          widget.onChanged(map);
        }
        return widget.buildObject(context, schema, map, depth: widget.depth);
      case 'array':
        final list = (widget.value as List?) ?? <dynamic>[];
        if (widget.value is! List) widget.onChanged(list);
        return widget.buildArray(
            context, 'items', schema, list,
            depth: widget.depth);
      case 'boolean':
        return SwitchListTile(
          title: const Text('item'),
          value: (widget.value as bool?) ?? false,
          onChanged: (v) => widget.onChanged(v),
        );
      case 'integer':
      case 'number':
        return TextField(
          controller: _ctrl,
          decoration: const InputDecoration(
              labelText: 'item', isDense: true, border: OutlineInputBorder()),
          keyboardType: const TextInputType.numberWithOptions(decimal: true),
          onChanged: (v) {
            final parsed =
                type == 'integer' ? int.tryParse(v) : num.tryParse(v);
            widget.onChanged(parsed ?? v);
          },
        );
      default:
        return TextField(
          controller: _ctrl,
          decoration: const InputDecoration(
              labelText: 'item', isDense: true, border: OutlineInputBorder()),
          onChanged: (v) => widget.onChanged(v),
        );
    }
  }
}
