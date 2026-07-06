import 'package:flutter/material.dart';

import '../json_viewer.dart';

/// The channels a chat answer can be previewed as: `web` is the canonical
/// A2UI render (the default), the rest are the chat-channel surface mappers
/// exposed by `POST /v1/surface/render` (mirrors `PREVIEW_ADAPTERS` in
/// triton-adapters-http). Adding a server adapter = one entry here.
const kWebChannel = 'web';
const kChatChannels = <(String, String, IconData)>[
  (kWebChannel, 'Web', Icons.language),
  ('gemini', 'Gemini', Icons.auto_awesome),
  ('copilot', 'Copilot', Icons.assistant),
  ('whatsapp', 'WhatsApp', Icons.chat),
  ('telegram', 'Telegram', Icons.send),
  ('discord', 'Discord', Icons.forum),
  ('googlechat', 'Google Chat', Icons.chat_bubble_outline),
  ('msteams', 'MS Teams', Icons.groups),
  ('signal', 'Signal', Icons.lock_outline),
];

/// A compact chip row under an agent bubble: pick which surface to view the
/// answer as. `Web` is the canonical render; the others map the SAME surface
/// through that platform's mapper — the folded-in successor to the old
/// standalone A2UI-diff channel dropdown.
class ChannelChips extends StatelessWidget {
  const ChannelChips({
    super.key,
    required this.selected,
    required this.onSelected,
  });

  final String selected;
  final void Function(String channel) onSelected;

  @override
  Widget build(BuildContext context) {
    return Wrap(
      spacing: 6,
      runSpacing: 4,
      children: [
        for (final c in kChatChannels)
          ChoiceChip(
            avatar: Icon(c.$3, size: 15),
            label: Text(c.$2),
            selected: selected == c.$1,
            visualDensity: VisualDensity.compact,
            materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
            onSelected: (_) => onSelected(c.$1),
          ),
      ],
    );
  }
}

/// Renders a `POST /v1/surface/render` payload as a chat message: the
/// degrade decisions as chips, the mapped text in a chat-bubble container,
/// and the raw platform payload behind a disclosure. This is the same
/// treatment the retired A2UI-diff channel column used, now driven by a
/// live turn instead of an empty-args tool pick.
class ChannelBubble extends StatelessWidget {
  const ChannelBubble({
    super.key,
    required this.adapter,
    required this.payload,
    this.loading = false,
  });

  final String adapter;

  /// The native payload from `/v1/surface/render`, or null while loading /
  /// before the first fetch.
  final Map<String, dynamic>? payload;
  final bool loading;

  @override
  Widget build(BuildContext context) {
    if (loading || payload == null) {
      return const Padding(
        padding: EdgeInsets.symmetric(vertical: 12),
        child: SizedBox(
          height: 18,
          width: 18,
          child: CircularProgressIndicator(strokeWidth: 2),
        ),
      );
    }
    final r = payload!;
    final cs = Theme.of(context).colorScheme;
    if (r['rendered'] == false) {
      return Text(
        'No usable message — $adapter would skip this post.'
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
            style: const TextStyle(fontSize: 13, height: 1.4),
          ),
        ),
        const SizedBox(height: 4),
        Theme(
          data: Theme.of(context).copyWith(dividerColor: Colors.transparent),
          child: ExpansionTile(
            tilePadding: EdgeInsets.zero,
            childrenPadding: const EdgeInsets.only(bottom: 8),
            dense: true,
            title: Text(
              'raw $adapter payload',
              style: Theme.of(context).textTheme.bodySmall,
            ),
            children: [JsonViewer(r)],
          ),
        ),
      ],
    );
  }

  List<Widget> _chips(Map<String, dynamic> r) {
    final chips = <Widget>[];
    void add(String label, {IconData? icon}) => chips.add(
      Chip(
        avatar: icon == null ? null : Icon(icon, size: 14),
        label: Text(label),
        visualDensity: VisualDensity.compact,
        materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
      ),
    );
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
