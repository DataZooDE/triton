import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter/material.dart';

import 'component_builder.dart';
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
    this.onResourceAction,
    this.componentBuilder,
    this.unknownComponentBuilder,
  });

  final Map<String, dynamic> envelope;
  final void Function(String tool, Map<String, dynamic> args)? onAction;

  /// A `sources` chip was tapped: open its `ui://` resource inline.
  final void Function(String uri)? onOpenResource;

  /// A `button` carrying a `resource` was tapped. Falls back to [onAction]
  /// when the host has not supplied it, which is what keeps a resource button
  /// behaving exactly as it did for hosts that predate the callback.
  /// See [A2uiResourceActionCallback].
  final A2uiResourceActionCallback? onResourceAction;

  /// A host's per-node override, consulted before every rule below.
  /// See [A2uiComponentBuilder].
  final A2uiComponentBuilder? componentBuilder;

  /// What to draw for a `type` this tree does not know; null keeps the amber
  /// debug card. See [A2uiUnknownComponentBuilder].
  final A2uiUnknownComponentBuilder? unknownComponentBuilder;

  @override
  Widget build(BuildContext context) {
    final stream = (envelope['stream'] as List?) ?? const [];
    // A sibling button carrying a ui:// resource is the open affordance
    // (hosts auto-open it) — inline `report` nodes are suppressed next to
    // it to avoid a duplicate control.
    final hasResourceButton = stream.any((c) =>
        c is Map && (c['resource'] as String?)?.startsWith('ui://') == true);
    // A run of consecutive action buttons (the model's proposed follow-ups,
    // plus an optional "Open report") collapses into one compact horizontal
    // `Wrap` — mirroring the channel-chip row — instead of a tall stack of
    // full-width buttons.
    final children = <Widget>[];
    final actions = <Widget>[];
    void flushActions() {
      if (actions.isEmpty) return;
      children.add(Padding(
        padding: const EdgeInsets.symmetric(vertical: 6),
        child: Wrap(
          spacing: 8,
          runSpacing: 4,
          children: List.of(actions),
        ),
      ));
      actions.clear();
    }

    for (final raw in stream) {
      // The host first, and before the placement rules below — see
      // [A2uiComponentBuilder] for why its opinion wins outright.
      final hosted = componentBuilder == null || raw is! Map
          ? null
          : componentBuilder!(context, raw.cast<String, dynamic>());
      if (hosted != null) {
        flushActions();
        children.add(hosted);
        continue;
      }
      if (_isSuppressedReport(raw, hasResourceButton)) continue;
      final action = _actionButton(context, raw);
      if (action != null) {
        actions.add(action);
        continue;
      }
      flushActions();
      children.add(_node(context, raw));
    }
    flushActions();
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: children,
    );
  }

  /// An inline `report` next to a resource button is opened by that sibling —
  /// drop it here to avoid a duplicate control.
  bool _isSuppressedReport(dynamic raw, bool hasResourceButton) =>
      hasResourceButton && raw is Map && raw['type'] == 'report';

  /// A compact follow-up button for an actionable node (`button` or an inline
  /// `report` open-control), else null. Rendered into the horizontal `Wrap`.
  Widget? _actionButton(BuildContext context, dynamic raw) {
    if (raw is! Map) return null;
    final map = raw.cast<String, dynamic>();
    switch (map['type']) {
      case 'button':
        final label = (map['label'] as String?) ?? '';
        final action = (map['action'] as Map?)?.cast<String, dynamic>();
        return _followUp(
          context,
          label,
          action == null ? null : _dispatch(action, map['resource'] as String?),
          primary: map['primary'] == true,
        );
      case 'report':
        // A report that inlines a preview is a block, not a chip: it renders
        // through `_node` (which draws the preview AND, when the host can
        // dispatch, the same open control). Only a dispatch-only report — the
        // pre-#206 shape — collapses into the follow-up row, unchanged.
        if (_hasPreview(map)) return null;
        final reportId = (map['report_id'] as String?) ?? '';
        final rawArgs = (map['args'] as Map?)?.cast<String, dynamic>() ??
            <String, dynamic>{};
        return _followUp(
          context,
          'Open report: $reportId',
          onAction == null || reportId.isEmpty
              ? null
              : () => onAction!(
                    'render_report',
                    {...rawArgs, 'report_id': reportId},
                  ),
        );
      default:
        return null;
    }
  }

  /// Where a button's tap goes.
  ///
  /// A `resource` reaches [onResourceAction] **whole** — tool, args and uri
  /// together, because a resource button is an action that also names a view.
  /// A host that has not supplied that callback gets the `(tool, args)`
  /// dispatch it has always had: the resource is unreachable for it, exactly
  /// as before, rather than the tap silently changing meaning.
  VoidCallback? _dispatch(Map<String, dynamic> action, String? resource) {
    final tool = (action['tool'] as String?) ?? '';
    final args =
        ((action['args'] as Map?)?.cast<String, dynamic>()) ?? const {};
    final resourceSink = onResourceAction;
    if (resource != null && resource.isNotEmpty && resourceSink != null) {
      return () => resourceSink(tool, args, resource);
    }
    final sink = onAction;
    return sink == null ? null : () => sink(tool, args);
  }

  /// Whether a `report` node carries the inline preview #206 added beside
  /// `report_id` — the thing a host that cannot dispatch `render_report`
  /// draws instead.
  static bool _hasPreview(Map<String, dynamic> map) =>
      (map['title'] as String?)?.isNotEmpty == true ||
      (map['series'] as List?)?.isNotEmpty == true ||
      (map['labels'] as List?)?.isNotEmpty == true;

  /// A compact, tertiary-accented action button — visually distinct from the
  /// neutral channel chips, small enough that several fit on one `Wrap` line.
  ///
  /// [primary] weights the one action the surface is actually asking for. A
  /// contrast, not a restyle: its siblings keep the tonal weighting every
  /// button had before, or "primary" would mean nothing.
  Widget _followUp(
    BuildContext context,
    String label,
    VoidCallback? onPressed, {
    bool primary = false,
  }) {
    final scheme = Theme.of(context).colorScheme;
    return FilledButton(
      onPressed: onPressed,
      style: FilledButton.styleFrom(
        backgroundColor:
            primary ? scheme.primary : scheme.tertiaryContainer,
        foregroundColor:
            primary ? scheme.onPrimary : scheme.onTertiaryContainer,
        visualDensity: VisualDensity.compact,
        tapTargetSize: MaterialTapTargetSize.shrinkWrap,
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
        textStyle: Theme.of(context).textTheme.labelLarge,
      ),
      child: Text(label),
    );
  }

  Widget _node(BuildContext context, dynamic raw) {
    if (raw is! Map) {
      return _unknown(context, 'not an object', const <String, dynamic>{});
    }
    final map = raw.cast<String, dynamic>();
    final type = map['type'] as String?;
    switch (type) {
      case 'text':
        // Chat text may carry light portable markdown (the same subset the
        // Google Chat adapter normalises) — render it, don't show raw `**`.
        //
        // The pills map is passed even when the node omits it (an empty map,
        // never null): omission means "no label was resolvable", which is the
        // D27 degrade — every wikilink shows its bare id. Passing null here
        // would mean "this contract has no wikilinks" and would leak `[[…]]`
        // to the reader in exactly the case where the labels were withheld.
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          child: MarkdownLite(
            (map['text'] as String?) ?? '',
            pills: (map['pills'] as Map?)?.map(
                  (k, v) => MapEntry('$k', '$v'),
                ) ??
                const <String, String>{},
          ),
        );
      case 'narration':
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          child: Text(
            (map['text'] as String?) ?? '',
            style: const TextStyle(fontStyle: FontStyle.italic),
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
                  placeholder: f['placeholder'] as String?,
                  // `containsKey`, not `!= null`: a field the agent explicitly
                  // proposed as null is a proposal, and collapsing it into
                  // "no default" would be the same silent-substitution bug
                  // this field exists to fix, one level down.
                  hasDefault: f.containsKey('default'),
                  defaultValue: f['default'],
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
      // `button` and a dispatch-only `report` are actionable nodes handled in
      // `build` — they collapse into the compact follow-up `Wrap`, so they
      // never reach here. A `report` carrying an inline preview does.
      case 'report':
        final reportId = (map['report_id'] as String?) ?? '';
        final rawArgs = (map['args'] as Map?)?.cast<String, dynamic>() ??
            <String, dynamic>{};
        return _ReportPreview(
          title: (map['title'] as String?) ?? '',
          series: ((map['series'] as List?) ?? const [])
              .whereType<num>()
              .map((n) => n.toDouble())
              .toList(growable: false),
          labels: ((map['labels'] as List?) ?? const [])
              .map((l) => l.toString())
              .toList(growable: false),
          // Beside the preview, never instead of it: the same envelope has to
          // work in a host that can dispatch `render_report` and one that
          // cannot, and that is the whole degradation contract.
          openLabel: 'Open report: $reportId',
          onOpen: onAction == null || reportId.isEmpty
              ? null
              : () => onAction!(
                    'render_report',
                    {...rawArgs, 'report_id': reportId},
                  ),
        );
      case 'diff':
        return _Diff(
          lines: ((map['lines'] as List?) ?? const [])
              .whereType<Map>()
              .map((l) => l.cast<String, dynamic>())
              .toList(growable: false),
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
        return _unknown(context, 'unknown v0.9 type: $type', map);
    }
  }

  Widget _unknown(
    BuildContext context,
    String message,
    Map<String, dynamic> node,
  ) {
    final hosted = unknownComponentBuilder;
    if (hosted != null) return hosted(context, node);
    return Padding(
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
}

// ---------------------------------------------------------------
// Rich-component helpers — local to v0.9 per ADR-4. v0.9 uses
// SegmentedButton for selection where v0.8 used ChoiceChip — the
// renderers can diverge without affecting each other.
// ---------------------------------------------------------------

/// The `diff` component's default presentation: a monospace block where only
/// added and removed lines are tinted.
///
/// Context is left plain deliberately — if every line is highlighted, none of
/// them is, and the change actually being approved stops standing out.
///
/// A `fold` starts collapsed behind its line count and expands on tap. It is
/// never expanded by default: a diff is mostly unchanged context, and showing
/// all of it buries the change.
class _Diff extends StatefulWidget {
  const _Diff({required this.lines});
  final List<Map<String, dynamic>> lines;

  @override
  State<_Diff> createState() => _DiffState();
}

class _DiffState extends State<_Diff> {
  final _open = <int>{};

  @override
  Widget build(BuildContext context) => Card(
        margin: const EdgeInsets.symmetric(vertical: 6),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            for (var i = 0; i < widget.lines.length; i++)
              _line(context, widget.lines[i], i),
          ],
        ),
      );

  Widget _line(BuildContext context, Map<String, dynamic> l, int index) {
    final scheme = Theme.of(context).colorScheme;
    final op = l['op'] as String? ?? 'ctx';
    if (op == 'fold') {
      final hidden = ((l['hidden'] as List?) ?? const [])
          .whereType<Map>()
          .map((h) => h.cast<String, dynamic>())
          .toList(growable: false);
      final expanded = _open.contains(index);
      final count = l['count'] ?? hidden.length;
      return Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          InkWell(
            onTap: () => setState(
              () => expanded ? _open.remove(index) : _open.add(index),
            ),
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
              child: Text(
                expanded
                    ? 'Hide $count unchanged lines'
                    : 'Show $count unchanged lines',
                style: TextStyle(
                  fontWeight: FontWeight.bold,
                  color: scheme.primary,
                ),
              ),
            ),
          ),
          if (expanded)
            // -1: a hidden line owns no fold state of its own.
            for (final h in hidden) _line(context, h, -1),
        ],
      );
    }
    final background = switch (op) {
      'add' => scheme.primaryContainer,
      'del' => scheme.errorContainer,
      _ => null,
    };
    final marker = switch (op) {
      'add' => '+',
      'del' => '-',
      _ => ' ',
    };
    return Container(
      color: background,
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 2),
      child: Text(
        '$marker ${l['text'] ?? ''}',
        style: const TextStyle(fontFamily: 'monospace', fontSize: 13),
      ),
    );
  }
}

/// The inline preview a `report` may carry beside its `report_id` (#206).
///
/// Deliberately the cheapest chart that is still a chart: a bar per value with
/// its label under it. This is the rung *below* dispatching `render_report`,
/// for a host that cannot dispatch at all — its job is to show the shape of
/// the number, not to compete with the real report. Colours come from the
/// host's scheme; this package is host-agnostic and hosts own their look.
class _ReportPreview extends StatelessWidget {
  const _ReportPreview({
    required this.title,
    required this.series,
    required this.labels,
    required this.openLabel,
    required this.onOpen,
  });

  final String title;
  final List<double> series;
  final List<String> labels;
  final String openLabel;
  final VoidCallback? onOpen;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final max = series.isEmpty
        ? 1.0
        : series.fold<double>(0, (a, b) => b > a ? b : a);
    return Card(
      margin: const EdgeInsets.symmetric(vertical: 6),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            if (title.isNotEmpty)
              Text(title, style: Theme.of(context).textTheme.titleSmall),
            if (series.isNotEmpty) ...[
              const SizedBox(height: 12),
              SizedBox(
                height: 96,
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.end,
                  children: [
                    for (var i = 0; i < series.length; i++)
                      Expanded(
                        child: Padding(
                          padding: const EdgeInsets.symmetric(horizontal: 4),
                          child: Column(
                            mainAxisAlignment: MainAxisAlignment.end,
                            children: [
                              Container(
                                height: max <= 0
                                    ? 4
                                    : 4 + (series[i] / max) * 60,
                                decoration: BoxDecoration(
                                  color: scheme.primary,
                                  borderRadius: BorderRadius.circular(3),
                                ),
                              ),
                              const SizedBox(height: 4),
                              Text(
                                i < labels.length ? labels[i] : '',
                                overflow: TextOverflow.ellipsis,
                                style: Theme.of(context).textTheme.bodySmall,
                              ),
                            ],
                          ),
                        ),
                      ),
                  ],
                ),
              ),
            ],
            if (onOpen != null)
              Align(
                alignment: Alignment.centerLeft,
                child: TextButton(onPressed: onOpen, child: Text(openLabel)),
              ),
          ],
        ),
      ),
    );
  }
}

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
    this.placeholder,
    this.hasDefault = false,
    this.defaultValue,
  });
  final String name;
  final String label;
  final String kind;
  final bool required;

  /// Hint text. Never a value — see [hasDefault].
  final String? placeholder;

  /// Whether the agent proposed a value for this field.
  ///
  /// Separate from [defaultValue] being null because they answer different
  /// questions, and the two must not be conflated: a placeholder that
  /// submitted itself, or a "no proposal" that submitted the empty string,
  /// would put words the agent never proposed into a record a consultant
  /// signed off. When this is false the field contributes **no key at all**
  /// to the submitted map, which is exactly what a pre-#206 envelope did.
  final bool hasDefault;
  final Object? defaultValue;
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
  void initState() {
    super.initState();
    _seedDefaults();
  }

  @override
  void didUpdateWidget(_Form old) {
    super.didUpdateWidget(old);
    // A new surface for the same form slot brings new proposals. Seeding is
    // still idempotent per field name, so a value the user has already touched
    // survives a rebuild that did not change the field.
    _seedDefaults();
  }

  /// Seed the values the agent proposed, so an **untouched** submit carries
  /// them instead of nulls. Only fields that actually carry a `default` are
  /// seeded: a field with none must stay absent from the map, because inventing
  /// an empty string for it would be a value the agent never proposed — the
  /// same class of bug, in the opposite direction.
  void _seedDefaults() {
    for (final f in widget.fields) {
      if (!f.hasDefault || f.name.isEmpty || _values.containsKey(f.name)) {
        continue;
      }
      final value = f.kind == 'integer' && f.defaultValue is String
          ? int.tryParse(f.defaultValue as String) ?? f.defaultValue
          : f.defaultValue;
      _values[f.name] = value;
      if (f.kind != 'boolean') {
        _ctrl(f.name).text = value == null ? '' : '$value';
      }
    }
  }

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
          hintText: f.placeholder,
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
