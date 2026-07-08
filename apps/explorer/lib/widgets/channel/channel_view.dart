import 'package:flutter/material.dart';

import '../../api/models.dart';
import '../a2ui/ui_resource_view.dart';
import '../json_viewer.dart';
import 'channel_chrome.dart';

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
  ('email', 'Email', Icons.email_outlined),
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
    this.result,
    this.loading = false,
  });

  final String adapter;

  /// The native payload from `/v1/surface/render`, or null while loading /
  /// before the first fetch.
  final Map<String, dynamic>? payload;

  /// The turn's canonical result — the same one the Web bubble renders.
  /// Carries the report `uiResourceUri` (a chart that renders on every
  /// channel) and the raw A2UI surface (whose `button` components feed the
  /// card/answer channels' action rows). `/v1/surface/render` can't re-render
  /// the peacock PNG or replay the buttons, so we reuse what the Explorer
  /// already holds. Null when there's nothing extra to carry.
  final InvocationResult? result;
  final bool loading;

  /// Lift the `button` components out of a turn's raw A2UI surface. Buttons
  /// ride two shapes: the raw agent surface carries `components` with
  /// `kind: "button"` (the tool sits directly on the node), while the
  /// negotiated A2UI envelope re-shapes them onto `stream` with
  /// `type: "button"` (the tool moves under `action`). Both carry `label` and
  /// an optional `ui://` `resource`. Bounded, protocol-agnostic scan.
  static List<ChannelButton> buttonsFrom(Map<String, dynamic>? raw) {
    final out = <ChannelButton>[];
    void scan(Object? node, int depth) {
      if (depth > 8) return;
      if (node is Map) {
        for (final key in const ['components', 'stream']) {
          final comps = node[key];
          if (comps is List) {
            for (final c in comps) {
              if (c is! Map) continue;
              if (c['kind'] != 'button' && c['type'] != 'button') continue;
              final label = (c['label'] as String?)?.trim() ?? '';
              if (label.isEmpty) continue;
              final action = (c['action'] as Map?)?.cast<String, dynamic>();
              out.add(ChannelButton(
                label: label,
                tool: (c['tool'] as String?) ?? (action?['tool'] as String?),
                resource: c['resource'] as String?,
              ));
            }
          }
        }
        for (final v in node.values) {
          scan(v, depth + 1);
        }
      } else if (node is List) {
        for (final v in node) {
          scan(v, depth + 1);
        }
      }
    }

    scan(raw, 0);
    return out;
  }

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
    if (r['rendered'] == false) {
      return Text(
        'No usable message — $adapter would skip this post.'
        '${r['reason'] != null ? '\n(${r['reason']})' : ''}',
        style: Theme.of(context).textTheme.bodySmall,
      );
    }
    final meta = kChatChannels.firstWhere(
      (c) => c.$1 == adapter,
      orElse: () => (adapter, adapter, Icons.chat),
    );
    final text = (r['text'] as String?)?.trim().isNotEmpty == true
        ? r['text'] as String
        : '(no text part)';
    // The card/answer channels render Adaptive-Card / rich-card actions, so
    // show the surface's buttons inside the chrome instead of a "deferred"
    // chip. Bubble (messaging) channels can't — they keep the chip.
    final buttons = channelShowsButtons(adapter)
        ? buttonsFrom(result?.raw)
        : const <ChannelButton>[];
    final reportUri = result?.uiResourceUri;
    final chips = _chips(r, suppressDeferredButtons: buttons.isNotEmpty);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        // The message painted as the target product.
        ChannelChrome(
          adapter: adapter,
          label: meta.$2,
          icon: meta.$3,
          text: text,
          // Only the email mapper returns a subject; the email skin renders
          // it in its header, every other skin ignores it.
          subject: r['subject'] as String?,
        ),
        // The answer's action buttons, styled to sit under the card/answer
        // chrome (the platforms that render rich-card actions).
        if (buttons.isNotEmpty) ...[
          const SizedBox(height: 8),
          ChannelButtons(adapter: adapter, buttons: buttons),
        ],
        // The report graphic (a chart) renders on EVERY channel — reuse the
        // exact same embed the Web bubble uses, below the channel chrome.
        if (reportUri != null) ...[
          const SizedBox(height: 10),
          UiResourceView(uri: reportUri, height: 420),
        ],
        // Degrade decisions + raw payload stay as compact "how it mapped"
        // footnotes below the chrome.
        if (chips.isNotEmpty) ...[
          const SizedBox(height: 8),
          Wrap(spacing: 6, runSpacing: 4, children: chips),
        ],
        const SizedBox(height: 2),
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

  List<Widget> _chips(
    Map<String, dynamic> r, {
    bool suppressDeferredButtons = false,
  }) {
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
      // When we now render the buttons inside the chrome, drop the
      // "deferred: N" chip — don't show both.
      if (k == 'deferred_buttons' && suppressDeferredButtons) continue;
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
