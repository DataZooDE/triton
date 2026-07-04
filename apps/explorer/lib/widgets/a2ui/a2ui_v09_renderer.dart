import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter/material.dart';

import 'markdown_lite.dart';
import 'sources_row.dart';

/// A2UI v0.9 renderer. **No shared base** with v0.8 per ADR-4. The
/// envelope uses lowercase `type`, no `Component` wrapper, action
/// data inlined. Components: text / narration / button / selection /
/// form / dashboard, plus the report kinds kpi / table / vega (so a
/// report renderer's surface — e.g. Peacock — renders natively).
class A2UIv09Renderer extends StatelessWidget {
  const A2UIv09Renderer({
    super.key,
    required this.envelope,
    this.onAction,
    this.onOpenResource,
  });

  final Map<String, dynamic> envelope;
  final void Function(String tool, Map<String, dynamic> args)? onAction;

  /// A `sources` chip was tapped: open its `ui://` resource inline.
  final void Function(String uri)? onOpenResource;

  @override
  Widget build(BuildContext context) {
    final stream = (envelope['stream'] as List?) ?? const [];
    // A sibling button carrying a ui:// resource is the open affordance
    // (hosts auto-open it) — inline `report` nodes are suppressed next to
    // it to avoid a duplicate control.
    final hasResourceButton = stream.any((c) =>
        c is Map && (c['resource'] as String?)?.startsWith('ui://') == true);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final raw in stream) _node(context, raw, hasResourceButton),
      ],
    );
  }

  Widget _node(BuildContext context, dynamic raw, bool hasResourceButton) {
    if (raw is! Map) return _unknown('not an object');
    final map = raw.cast<String, dynamic>();
    final type = map['type'] as String?;
    switch (type) {
      case 'text':
        // Chat text may carry light portable markdown (the same subset the
        // Google Chat adapter normalises) — render it, don't show raw `**`.
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          child: MarkdownLite((map['text'] as String?) ?? ''),
        );
      case 'narration':
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          child: Text(
            (map['text'] as String?) ?? '',
            style: const TextStyle(fontStyle: FontStyle.italic),
          ),
        );
      case 'button':
        final label = (map['label'] as String?) ?? '';
        final action = (map['action'] as Map?)?.cast<String, dynamic>();
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 6),
          child: Align(
            alignment: Alignment.centerLeft,
            child: FilledButton(
              onPressed: onAction == null || action == null
                  ? null
                  : () => onAction!(
                        action['tool'] as String,
                        ((action['args'] as Map?)?.cast<String, dynamic>()) ??
                            const {},
                      ),
              child: Text(label),
            ),
          ),
        );
      case 'selection':
        final prompt = (map['prompt'] as String?) ?? '';
        final tool = (map['tool'] as String?) ?? '';
        final argsKey = (map['args_key'] as String?) ?? 'value';
        final options = ((map['options'] as List?) ?? const [])
            .cast<Map>()
            .map((o) => _OptionPair(
                  label: (o['label'] as String?) ?? '',
                  value: (o['value'] as String?) ?? '',
                ))
            .toList(growable: false);
        return _Selection(
          prompt: prompt,
          options: options,
          onPick: onAction == null
              ? null
              : (value) => onAction!(tool, {argsKey: value}),
        );
      case 'form':
        final title = (map['title'] as String?) ?? '';
        final submitLabel = (map['submit_label'] as String?) ?? 'Submit';
        final tool = (map['tool'] as String?) ?? '';
        final fields = ((map['fields'] as List?) ?? const [])
            .cast<Map>()
            .map((f) => _FormFieldSpec(
                  name: (f['name'] as String?) ?? '',
                  label: (f['label'] as String?) ?? '',
                  kind: (f['kind'] as String?) ?? 'string',
                  required: (f['required'] as bool?) ?? false,
                ))
            .toList(growable: false);
        return _Form(
          title: title,
          fields: fields,
          submitLabel: submitLabel,
          onSubmit: onAction == null
              ? null
              : (values) => onAction!(tool, values),
        );
      case 'dashboard':
        final title = (map['title'] as String?) ?? '';
        final tiles = ((map['tiles'] as List?) ?? const [])
            .cast<Map>()
            .map((t) => _Tile(
                  label: (t['label'] as String?) ?? '',
                  value: (t['value'] as String?) ?? '',
                  trend: t['trend'] as String?,
                ))
            .toList(growable: false);
        return _Dashboard(title: title, tiles: tiles);
      case 'kpi':
        return _Kpi(
          label: (map['label'] as String?) ?? '',
          value: (map['value'] ?? '').toString(),
          trend: map['trend'] as String?,
        );
      case 'table':
        final columns = ((map['columns'] as List?) ?? const [])
            .map((c) => c.toString())
            .toList(growable: false);
        final rows = ((map['rows'] as List?) ?? const [])
            .map((r) => ((r as List?) ?? const [])
                .map((c) => c?.toString() ?? '')
                .toList(growable: false))
            .toList(growable: false);
        return _DataTableView(columns: columns, rows: rows);
      case 'vega':
        return _Vega(
          title: map['title'] as String?,
          pngBase64: map['png_base64'] as String?,
        );
      case 'report':
        // An inline report reference: image-hosting chat adapters expand it
        // server-side; here the resource button (if any) already auto-opens
        // the richer ui:// app, so only render an open control when the
        // reply carries no other affordance. Peacock renders — this chip
        // only dispatches.
        if (hasResourceButton) return const SizedBox.shrink();
        final reportId = (map['report_id'] as String?) ?? '';
        final rawArgs =
            (map['args'] as Map?)?.cast<String, dynamic>() ?? <String, dynamic>{};
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 6),
          child: Align(
            alignment: Alignment.centerLeft,
            child: FilledButton.tonal(
              onPressed: onAction == null || reportId.isEmpty
                  ? null
                  : () => onAction!(
                        'render_report',
                        {...rawArgs, 'report_id': reportId},
                      ),
              child: Text('Open report: $reportId'),
            ),
          ),
        );
      case 'sources':
        final items = ((map['items'] as List?) ?? const [])
            .whereType<Map>()
            .map((i) => SourceChip(
                  label: (i['label'] as String?) ?? '',
                  resource: (i['resource'] as String?) ?? '',
                ))
            .toList(growable: false);
        return SourcesRow(items: items, onOpen: onOpenResource);
      default:
        return _unknown('unknown v0.9 type: $type');
    }
  }

  Widget _unknown(String message) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 4),
        child: Card(
          color: Colors.amber.shade100,
          child: Padding(
            padding: const EdgeInsets.all(8),
            child: Text(message),
          ),
        ),
      );
}

// ---------------------------------------------------------------
// Rich-component helpers — local to v0.9 per ADR-4. v0.9 uses
// SegmentedButton for selection where v0.8 used ChoiceChip — the
// renderers can diverge without affecting each other.
// ---------------------------------------------------------------

class _OptionPair {
  const _OptionPair({required this.label, required this.value});
  final String label;
  final String value;
}

class _FormFieldSpec {
  const _FormFieldSpec({
    required this.name,
    required this.label,
    required this.kind,
    required this.required,
  });
  final String name;
  final String label;
  final String kind;
  final bool required;
}

class _Tile {
  const _Tile({required this.label, required this.value, this.trend});
  final String label;
  final String value;
  final String? trend;
}

class _Selection extends StatefulWidget {
  const _Selection({
    required this.prompt,
    required this.options,
    required this.onPick,
  });
  final String prompt;
  final List<_OptionPair> options;
  final ValueChanged<String>? onPick;

  @override
  State<_Selection> createState() => _SelectionState();
}

class _SelectionState extends State<_Selection> {
  String? _picked;

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 6),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(widget.prompt),
            const SizedBox(height: 8),
            SegmentedButton<String>(
              segments: [
                for (final o in widget.options)
                  ButtonSegment(value: o.value, label: Text(o.label)),
              ],
              selected: {?_picked},
              emptySelectionAllowed: true,
              showSelectedIcon: false,
              onSelectionChanged: widget.onPick == null
                  ? null
                  : (s) {
                      if (s.isEmpty) return;
                      setState(() => _picked = s.first);
                      widget.onPick!(s.first);
                    },
            ),
          ],
        ),
      );
}

class _Form extends StatefulWidget {
  const _Form({
    required this.title,
    required this.fields,
    required this.submitLabel,
    required this.onSubmit,
  });
  final String title;
  final List<_FormFieldSpec> fields;
  final String submitLabel;
  final ValueChanged<Map<String, dynamic>>? onSubmit;

  @override
  State<_Form> createState() => _FormStateView();
}

class _FormStateView extends State<_Form> {
  final _values = <String, dynamic>{};
  final _controllers = <String, TextEditingController>{};

  @override
  void dispose() {
    for (final c in _controllers.values) {
      c.dispose();
    }
    super.dispose();
  }

  TextEditingController _ctrl(String name) =>
      _controllers.putIfAbsent(name, TextEditingController.new);

  @override
  Widget build(BuildContext context) => Card(
        margin: const EdgeInsets.symmetric(vertical: 6),
        child: Padding(
          padding: const EdgeInsets.all(12),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Text(widget.title,
                  style: Theme.of(context).textTheme.titleMedium),
              const SizedBox(height: 8),
              for (final f in widget.fields) _fieldFor(f),
              const SizedBox(height: 8),
              Align(
                alignment: Alignment.centerRight,
                child: FilledButton(
                  onPressed: widget.onSubmit == null
                      ? null
                      : () => widget.onSubmit!(Map.unmodifiable(_values)),
                  child: Text(widget.submitLabel),
                ),
              ),
            ],
          ),
        ),
      );

  Widget _fieldFor(_FormFieldSpec f) {
    final label = f.required ? '${f.label} *' : f.label;
    if (f.kind == 'boolean') {
      return SwitchListTile(
        title: Text(label),
        value: _values[f.name] as bool? ?? false,
        onChanged: (v) => setState(() => _values[f.name] = v),
      );
    }
    final isInt = f.kind == 'integer';
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 4),
      child: TextField(
        controller: _ctrl(f.name),
        keyboardType: isInt
            ? const TextInputType.numberWithOptions(decimal: false)
            : TextInputType.text,
        decoration: InputDecoration(
          labelText: label,
          border: const OutlineInputBorder(),
        ),
        onChanged: (v) {
          if (v.isEmpty) {
            _values.remove(f.name);
            return;
          }
          _values[f.name] = isInt ? int.tryParse(v) ?? v : v;
        },
      ),
    );
  }
}

/// A single headline metric (a report's `kpi` component).
class _Kpi extends StatelessWidget {
  const _Kpi({required this.label, required this.value, this.trend});
  final String label;
  final String value;
  final String? trend;

  @override
  Widget build(BuildContext context) => Card(
        margin: const EdgeInsets.symmetric(vertical: 6),
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(label,
                  style: Theme.of(context).textTheme.bodySmall?.copyWith(
                        color: Theme.of(context).colorScheme.onSurfaceVariant,
                      )),
              const SizedBox(height: 4),
              Text(value, style: Theme.of(context).textTheme.headlineSmall),
              if (trend != null) ...[
                const SizedBox(height: 2),
                Text(trend!, style: Theme.of(context).textTheme.bodySmall),
              ],
            ],
          ),
        ),
      );
}

/// A report's `table` component → a scrollable `DataTable`.
class _DataTableView extends StatelessWidget {
  const _DataTableView({required this.columns, required this.rows});
  final List<String> columns;
  final List<List<String>> rows;

  @override
  Widget build(BuildContext context) => Card(
        margin: const EdgeInsets.symmetric(vertical: 6),
        child: SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: DataTable(
            columns: [
              for (final c in columns) DataColumn(label: Text(c)),
            ],
            rows: [
              for (final r in rows)
                DataRow(cells: [
                  for (var i = 0; i < columns.length; i++)
                    DataCell(Text(i < r.length ? r[i] : '')),
                ]),
            ],
          ),
        ),
      );
}

/// A report's `vega` chart. A full Vega-Lite renderer is out of scope for
/// the SPA; when the producer ships a rasterised `png_base64` (Peacock does)
/// we show it, otherwise a placeholder pointing at the embedded report.
class _Vega extends StatelessWidget {
  const _Vega({this.title, this.pngBase64});
  final String? title;
  final String? pngBase64;

  @override
  Widget build(BuildContext context) {
    final bytes = _decode(pngBase64);
    return Card(
      margin: const EdgeInsets.symmetric(vertical: 6),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            if (title != null && title!.isNotEmpty) ...[
              Text(title!, style: Theme.of(context).textTheme.titleSmall),
              const SizedBox(height: 8),
            ],
            if (bytes != null)
              Image.memory(bytes, fit: BoxFit.contain)
            else
              Text(
                'chart (open the embedded report to view)',
                style: Theme.of(context).textTheme.bodySmall?.copyWith(
                      fontStyle: FontStyle.italic,
                      color: Theme.of(context).colorScheme.onSurfaceVariant,
                    ),
              ),
          ],
        ),
      ),
    );
  }

  static Uint8List? _decode(String? b64) {
    if (b64 == null || b64.isEmpty) return null;
    final cleaned = b64.contains(',') ? b64.split(',').last : b64;
    try {
      return base64Decode(cleaned);
    } catch (_) {
      return null;
    }
  }
}

class _Dashboard extends StatelessWidget {
  const _Dashboard({required this.title, required this.tiles});
  final String title;
  final List<_Tile> tiles;

  @override
  Widget build(BuildContext context) => Card(
        margin: const EdgeInsets.symmetric(vertical: 6),
        child: Padding(
          padding: const EdgeInsets.all(12),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(title, style: Theme.of(context).textTheme.titleMedium),
              const SizedBox(height: 12),
              Wrap(
                spacing: 12,
                runSpacing: 12,
                children: [
                  for (final t in tiles)
                    Container(
                      constraints: const BoxConstraints(minWidth: 140),
                      padding: const EdgeInsets.all(12),
                      decoration: BoxDecoration(
                        color: Theme.of(context)
                            .colorScheme
                            .surfaceContainerHigh,
                        borderRadius: BorderRadius.circular(8),
                      ),
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(t.label,
                              style: Theme.of(context)
                                  .textTheme
                                  .bodySmall
                                  ?.copyWith(
                                    color: Theme.of(context)
                                        .colorScheme
                                        .onSurfaceVariant,
                                  )),
                          const SizedBox(height: 4),
                          Text(t.value,
                              style:
                                  Theme.of(context).textTheme.titleLarge),
                          if (t.trend != null) ...[
                            const SizedBox(height: 2),
                            Text(t.trend!,
                                style: Theme.of(context).textTheme.bodySmall),
                          ],
                        ],
                      ),
                    ),
                ],
              ),
            ],
          ),
        ),
      );
}
