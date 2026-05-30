import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../providers/api_provider.dart';
import 'json_viewer.dart';

/// Shows how a tool's A2UI surface degrades onto each chat platform —
/// the same `POST /v1/surface/render` mappers the live couriers use.
/// One card per channel (Telegram, WhatsApp, Discord, Google Chat, MS
/// Teams, Signal) with the rendered text, the degrade decisions as
/// chips, and the raw platform payload.
///
/// It fetches the *canonical* surface itself by invoking the tool with
/// no A2UI Accept (the plain result carries `result.surface`), so it
/// doesn't depend on which A2UI version the caller is viewing.
class ChatChannelPreviews extends ConsumerStatefulWidget {
  const ChatChannelPreviews({
    super.key,
    required this.toolName,
    required this.args,
  });

  final String toolName;
  final Map<String, dynamic> args;

  @override
  ConsumerState<ChatChannelPreviews> createState() =>
      _ChatChannelPreviewsState();
}

const _channels = <(String, String, IconData)>[
  ('telegram', 'Telegram', Icons.send),
  ('whatsapp', 'WhatsApp', Icons.chat),
  ('discord', 'Discord', Icons.forum),
  ('googlechat', 'Google Chat', Icons.chat_bubble_outline),
  ('msteams', 'MS Teams', Icons.groups),
  ('signal', 'Signal', Icons.lock_outline),
];

class _ChatChannelPreviewsState extends ConsumerState<ChatChannelPreviews> {
  bool _loading = false;
  String? _error;
  bool _noSurface = false;
  final _results = <String, Map<String, dynamic>>{};

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    setState(() {
      _loading = true;
      _error = null;
      _noSurface = false;
      _results.clear();
    });
    final client = ref.read(restClientProvider);
    try {
      // Plain invoke → the canonical surface lives at result.surface.
      final r = await client.invoke(widget.toolName, widget.args);
      final result = (r.raw['result'] as Map?)?.cast<String, dynamic>();
      final surface = (result?['surface'] as Map?)?.cast<String, dynamic>();
      if (surface == null) {
        if (mounted) {
          setState(() {
            _noSurface = true;
            _loading = false;
          });
        }
        return;
      }
      final entries = await Future.wait(_channels.map((c) async {
        try {
          final res = await client
              .renderSurface(adapter: c.$1, result: {'surface': surface});
          return MapEntry(c.$1, res);
        } catch (e) {
          return MapEntry(c.$1, <String, dynamic>{
            'rendered': false,
            'reason': e.toString(),
          });
        }
      }));
      if (!mounted) return;
      setState(() {
        _results.addEntries(entries);
        _loading = false;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.toString();
        _loading = false;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            Text('Chat-channel previews',
                style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(width: 8),
            Tooltip(
              message: 'Same surface, mapped per platform via '
                  'POST /v1/surface/render',
              child: Icon(Icons.info_outline,
                  size: 16,
                  color: Theme.of(context).colorScheme.onSurfaceVariant),
            ),
            const Spacer(),
            if (_loading)
              const SizedBox(
                width: 16,
                height: 16,
                child: CircularProgressIndicator(strokeWidth: 2),
              )
            else
              IconButton(
                icon: const Icon(Icons.refresh, size: 18),
                tooltip: 'Re-render',
                onPressed: _load,
              ),
          ],
        ),
        const SizedBox(height: 8),
        if (_error != null)
          Card(
            color: Theme.of(context).colorScheme.errorContainer,
            child: Padding(
              padding: const EdgeInsets.all(12),
              child: Text('Could not render channels: $_error'),
            ),
          )
        else if (_noSurface)
          Text(
            'This tool returned no canonical surface to map.',
            style: Theme.of(context).textTheme.bodySmall,
          )
        else
          LayoutBuilder(
            builder: (context, constraints) {
              final cols = constraints.maxWidth >= 1100
                  ? 3
                  : constraints.maxWidth >= 720
                      ? 2
                      : 1;
              final gap = 12.0;
              final width =
                  (constraints.maxWidth - gap * (cols - 1)) / cols;
              return Wrap(
                spacing: gap,
                runSpacing: gap,
                children: [
                  for (final c in _channels)
                    SizedBox(
                      width: width,
                      child: _ChannelCard(
                        label: c.$2,
                        icon: c.$3,
                        result: _results[c.$1],
                      ),
                    ),
                ],
              );
            },
          ),
      ],
    );
  }
}

class _ChannelCard extends StatelessWidget {
  const _ChannelCard({
    required this.label,
    required this.icon,
    required this.result,
  });

  final String label;
  final IconData icon;
  final Map<String, dynamic>? result;

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final r = result;
    return Card(
      clipBehavior: Clip.antiAlias,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Container(
            color: cs.surfaceContainerHigh,
            padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
            child: Row(
              children: [
                Icon(icon, size: 16, color: cs.primary),
                const SizedBox(width: 8),
                Text(label,
                    style: Theme.of(context).textTheme.titleSmall),
              ],
            ),
          ),
          Padding(
            padding: const EdgeInsets.all(12),
            child: r == null
                ? const SizedBox(
                    height: 40,
                    child: Center(child: Text('—')),
                  )
                : _body(context, r),
          ),
        ],
      ),
    );
  }

  Widget _body(BuildContext context, Map<String, dynamic> r) {
    final cs = Theme.of(context).colorScheme;
    if (r['rendered'] == false) {
      return Text(
        'No usable message — the platform would skip this post.'
        '${r['reason'] != null ? '\n(${r['reason']})' : ''}',
        style: Theme.of(context).textTheme.bodySmall,
      );
    }
    final chips = _chips(r);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        if (chips.isNotEmpty) ...[
          Wrap(spacing: 6, runSpacing: 4, children: chips),
          const SizedBox(height: 8),
        ],
        // A chat-bubble-ish container for the rendered message text.
        Container(
          width: double.infinity,
          padding: const EdgeInsets.all(10),
          decoration: BoxDecoration(
            color: cs.surfaceContainer,
            borderRadius: const BorderRadius.only(
              topRight: Radius.circular(12),
              bottomLeft: Radius.circular(12),
              bottomRight: Radius.circular(12),
            ),
          ),
          child: SelectableText(
            (r['text'] as String?)?.trim().isNotEmpty == true
                ? r['text'] as String
                : '(no text part)',
            style: const TextStyle(fontSize: 12.5, height: 1.4),
          ),
        ),
        const SizedBox(height: 4),
        Theme(
          data: Theme.of(context)
              .copyWith(dividerColor: Colors.transparent),
          child: ExpansionTile(
            tilePadding: EdgeInsets.zero,
            childrenPadding: const EdgeInsets.only(bottom: 8),
            dense: true,
            title: Text('raw payload',
                style: Theme.of(context).textTheme.bodySmall),
            children: [JsonViewer(r)],
          ),
        ),
      ],
    );
  }

  List<Widget> _chips(Map<String, dynamic> r) {
    final chips = <Widget>[];
    void add(String label, {IconData? icon}) => chips.add(Chip(
          avatar: icon == null ? null : Icon(icon, size: 14),
          label: Text(label),
          visualDensity: VisualDensity.compact,
          materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
        ));
    if (r['parse_mode'] != null) add('parse_mode: ${r['parse_mode']}');
    if (r['components'] != null) add('components ✓');
    for (final k in const [
      'deferred_buttons',
      'deferred_selections',
      'deferred_forms',
      'deferred_dashboards',
    ]) {
      final v = r[k];
      if (v is int && v > 0) add('${k.replaceFirst('deferred_', '')}: $v');
    }
    if (r['has_dashboard_raster'] == true) {
      add('dashboard → PNG', icon: Icons.image_outlined);
    }
    if (r['truncated'] == true) add('truncated', icon: Icons.warning_amber);
    return chips;
  }
}
