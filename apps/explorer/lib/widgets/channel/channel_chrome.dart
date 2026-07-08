import 'package:flutter/material.dart';

import '../a2ui/markdown_lite.dart';

/// Paints a rendered channel payload as the *product* it targets, so the
/// per-turn preview shows a real rendering experience rather than one generic
/// card. Three shells cover the field:
///   * `bubble` — messaging channels (WhatsApp/Telegram/Signal/Discord): a
///     chat bubble in the platform's colour, plain text (as those clients show
///     it), with a faux timestamp (+ WhatsApp's delivery ticks).
///   * `card` — card channels (MS Teams/Copilot/Google Chat): an
///     Adaptive-Card-style card with a brand accent bar and markdown body.
///   * `answer` — Gemini Enterprise: a clean answer card with a sparkle accent
///     that renders markdown **including tables** (its distinctive output).
///   * `email` — an email-client shell: a From/To/**Subject** header over the
///     rendered body. The richest surface — it renders the message complete
///     (buttons as links, chart inline), and it's the only one with a subject.
class ChannelChrome extends StatelessWidget {
  const ChannelChrome({
    super.key,
    required this.adapter,
    required this.label,
    required this.icon,
    required this.text,
    this.subject,
  });

  final String adapter;
  final String label;
  final IconData icon;
  final String text;

  /// The email subject line — only the `email` skin renders it (the one
  /// channel whose mapper produces a subject). Null on every other channel.
  final String? subject;

  @override
  Widget build(BuildContext context) {
    final skin = _skins[adapter] ?? const _Skin(_Kind.card, Color(0xFF757575));
    final child = switch (skin.kind) {
      _Kind.bubble => _bubble(context, skin),
      _Kind.card => _card(context, skin),
      _Kind.answer => _answer(context, skin),
      _Kind.email => _email(context, skin),
    };
    // Stable per-adapter key so the console (and tests) can tell which shell
    // is on screen.
    return KeyedSubtree(key: ValueKey('channel-chrome-$adapter'), child: child);
  }

  // ── bubble (messaging) ────────────────────────────────────────────────
  Widget _bubble(BuildContext context, _Skin skin) {
    final dark = Theme.of(context).brightness == Brightness.dark;
    final bg = dark ? skin.bubbleDark : skin.bubbleLight;
    final fg = dark
        ? Colors.white.withValues(alpha: 0.92)
        : const Color(0xFF111111);
    return Align(
      alignment: Alignment.centerLeft,
      child: Container(
        constraints: const BoxConstraints(maxWidth: 460),
        margin: const EdgeInsets.only(right: 32, top: 2, bottom: 2),
        padding: const EdgeInsets.fromLTRB(12, 8, 10, 6),
        decoration: BoxDecoration(
          color: bg,
          borderRadius: const BorderRadius.only(
            topLeft: Radius.circular(4),
            topRight: Radius.circular(14),
            bottomLeft: Radius.circular(14),
            bottomRight: Radius.circular(14),
          ),
          boxShadow: [
            BoxShadow(
              color: Colors.black.withValues(alpha: 0.08),
              blurRadius: 2,
              offset: const Offset(0, 1),
            ),
          ],
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min,
          children: [
            SelectableText(
              text,
              style: TextStyle(color: fg, fontSize: 13.5, height: 1.35),
            ),
            const SizedBox(height: 2),
            Row(
              mainAxisSize: MainAxisSize.min,
              mainAxisAlignment: MainAxisAlignment.end,
              children: [
                Text(
                  '09:41',
                  style: TextStyle(
                    color: fg.withValues(alpha: 0.5),
                    fontSize: 10,
                  ),
                ),
                if (adapter == 'whatsapp') ...[
                  const SizedBox(width: 3),
                  const Icon(
                    Icons.done_all,
                    size: 13,
                    color: Color(0xFF53BDEB),
                  ),
                ],
              ],
            ),
          ],
        ),
      ),
    );
  }

  // ── card (Adaptive Cards: Teams / Copilot / Google Chat) ──────────────
  Widget _card(BuildContext context, _Skin skin) {
    final cs = Theme.of(context).colorScheme;
    return Container(
      constraints: const BoxConstraints(maxWidth: 540),
      decoration: BoxDecoration(
        color: cs.surface,
        borderRadius: BorderRadius.circular(8),
        border: Border.all(color: cs.outlineVariant),
        boxShadow: [
          BoxShadow(
            color: Colors.black.withValues(alpha: 0.06),
            blurRadius: 3,
            offset: const Offset(0, 1),
          ),
        ],
      ),
      clipBehavior: Clip.antiAlias,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          Container(height: 4, color: skin.accent),
          Padding(
            padding: const EdgeInsets.all(14),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                _header(skin, cs),
                const SizedBox(height: 8),
                _body(context, cs.onSurface, 13),
              ],
            ),
          ),
        ],
      ),
    );
  }

  // ── answer (Gemini) ───────────────────────────────────────────────────
  Widget _answer(BuildContext context, _Skin skin) {
    final cs = Theme.of(context).colorScheme;
    return Container(
      constraints: const BoxConstraints(maxWidth: 580),
      padding: const EdgeInsets.all(14),
      decoration: BoxDecoration(
        color: cs.surface,
        borderRadius: BorderRadius.circular(12),
        border: Border.all(color: skin.accent.withValues(alpha: 0.35)),
        gradient: LinearGradient(
          begin: Alignment.topLeft,
          end: Alignment.bottomRight,
          colors: [skin.accent.withValues(alpha: 0.07), Colors.transparent],
        ),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          _header(skin, cs),
          const SizedBox(height: 10),
          _body(context, cs.onSurface, 13.5),
        ],
      ),
    );
  }

  // ── email (an email-client shell) ─────────────────────────────────────
  Widget _email(BuildContext context, _Skin skin) {
    final cs = Theme.of(context).colorScheme;
    return Container(
      constraints: const BoxConstraints(maxWidth: 600),
      decoration: BoxDecoration(
        color: cs.surface,
        borderRadius: BorderRadius.circular(8),
        border: Border.all(color: cs.outlineVariant),
        boxShadow: [
          BoxShadow(
            color: Colors.black.withValues(alpha: 0.06),
            blurRadius: 3,
            offset: const Offset(0, 1),
          ),
        ],
      ),
      clipBehavior: Clip.antiAlias,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          // The header band: the channel label, then the envelope meta rows.
          Container(
            width: double.infinity,
            padding: const EdgeInsets.fromLTRB(14, 10, 14, 10),
            color: skin.accent.withValues(alpha: 0.08),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                _header(skin, cs),
                const SizedBox(height: 8),
                _emailMeta(cs, 'From', 'assistant@datazoo.example'),
                _emailMeta(cs, 'To', 'you@company.example'),
                _emailMeta(
                  cs,
                  'Subject',
                  subject?.trim().isNotEmpty == true
                      ? subject!.trim()
                      : '(no subject)',
                  bold: true,
                ),
              ],
            ),
          ),
          Padding(
            padding: const EdgeInsets.all(14),
            child: _body(context, cs.onSurface, 13),
          ),
        ],
      ),
    );
  }

  Widget _emailMeta(ColorScheme cs, String label, String value, {bool bold = false}) =>
      Padding(
        padding: const EdgeInsets.only(top: 2),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            SizedBox(
              width: 56,
              child: Text(
                label,
                style: TextStyle(
                  fontSize: 12,
                  color: cs.onSurfaceVariant,
                  fontWeight: FontWeight.w500,
                ),
              ),
            ),
            Expanded(
              child: Text(
                value,
                style: TextStyle(
                  fontSize: 12.5,
                  color: cs.onSurface,
                  fontWeight: bold ? FontWeight.w700 : FontWeight.normal,
                ),
              ),
            ),
          ],
        ),
      );

  Widget _header(_Skin skin, ColorScheme cs) => Row(
    mainAxisSize: MainAxisSize.min,
    children: [
      Icon(icon, size: 15, color: skin.accent),
      const SizedBox(width: 6),
      Text(
        label,
        style: TextStyle(
          color: skin.accent,
          fontWeight: FontWeight.w600,
          fontSize: 12.5,
        ),
      ),
    ],
  );

  Widget _body(BuildContext context, Color color, double size) =>
      DefaultTextStyle.merge(
        style: TextStyle(fontSize: size, height: 1.45, color: color),
        child: RichMarkdown(text),
      );
}

/// An action button lifted from the answer's A2UI surface — its label plus
/// (for reference) the tool/resource it would invoke. Rendered by
/// [ChannelButtons] as a preview of the Adaptive-Card / rich-card actions the
/// card and answer channels support.
class ChannelButton {
  const ChannelButton({required this.label, this.tool, this.resource});

  final String label;
  final String? tool;
  final String? resource;
}

/// Whether [adapter]'s chrome can render action buttons. Card channels
/// (MS Teams / Copilot / Google Chat) and the Gemini answer card render
/// Adaptive-Card / rich-card actions; the pure-messaging bubble channels
/// (WhatsApp / Telegram / Signal / Discord) can't — their buttons stay a
/// "deferred" footnote chip.
bool channelShowsButtons(String adapter) {
  final skin = _skins[adapter];
  return skin != null &&
      (skin.kind == _Kind.card ||
          skin.kind == _Kind.answer ||
          skin.kind == _Kind.email);
}

/// A row of the answer's action buttons, styled with the channel's brand
/// accent to sit under its chrome (an Adaptive-Card / rich-card action row).
/// Display-only — this is a preview, so the buttons aren't interactive, but
/// they read clearly as buttons.
class ChannelButtons extends StatelessWidget {
  const ChannelButtons({
    super.key,
    required this.adapter,
    required this.buttons,
  });

  final String adapter;
  final List<ChannelButton> buttons;

  @override
  Widget build(BuildContext context) {
    final accent =
        (_skins[adapter] ?? const _Skin(_Kind.card, Color(0xFF757575))).accent;
    return KeyedSubtree(
      key: ValueKey('channel-buttons-$adapter'),
      child: Wrap(
        spacing: 8,
        runSpacing: 8,
        children: [
          for (final b in buttons)
            Container(
              padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
              decoration: BoxDecoration(
                color: accent.withValues(alpha: 0.12),
                borderRadius: BorderRadius.circular(6),
                border: Border.all(color: accent.withValues(alpha: 0.5)),
              ),
              child: Text(
                b.label,
                style: TextStyle(
                  color: accent,
                  fontWeight: FontWeight.w600,
                  fontSize: 12.5,
                ),
              ),
            ),
        ],
      ),
    );
  }
}

enum _Kind { bubble, card, answer, email }

class _Skin {
  const _Skin(
    this.kind,
    this.accent, {
    this.bubbleLight = const Color(0xFFECECEC),
    this.bubbleDark = const Color(0xFF2A2A2E),
  });

  final _Kind kind;
  final Color accent;
  final Color bubbleLight;
  final Color bubbleDark;
}

/// One skin per preview adapter. Bubble channels carry platform-recognisable
/// colours; card/answer channels carry a brand accent.
const _skins = <String, _Skin>{
  'whatsapp': _Skin(
    _Kind.bubble,
    Color(0xFF25D366),
    bubbleLight: Color(0xFFD9FDD3),
    bubbleDark: Color(0xFF005C4B),
  ),
  'telegram': _Skin(
    _Kind.bubble,
    Color(0xFF2AABEE),
    bubbleLight: Color(0xFFFFFFFF),
    bubbleDark: Color(0xFF212D3B),
  ),
  'signal': _Skin(
    _Kind.bubble,
    Color(0xFF3A76F0),
    bubbleLight: Color(0xFFEAEAEA),
    bubbleDark: Color(0xFF2C2C2E),
  ),
  'discord': _Skin(
    _Kind.bubble,
    Color(0xFF5865F2),
    bubbleLight: Color(0xFFF2F3F5),
    bubbleDark: Color(0xFF383A40),
  ),
  'googlechat': _Skin(_Kind.card, Color(0xFF1A73E8)),
  'msteams': _Skin(_Kind.card, Color(0xFF6264A7)),
  'copilot': _Skin(_Kind.card, Color(0xFF8B5CF6)),
  'gemini': _Skin(_Kind.answer, Color(0xFF4285F4)),
  'email': _Skin(_Kind.email, Color(0xFFD93025)),
};

/// Markdown body that also renders pipe tables (`| a | b |`) as a real
/// [Table] — [MarkdownLite] alone leaves the pipes raw. Non-table runs go to
/// MarkdownLite (bold / bullets / links). This is what makes the Gemini answer
/// card render a dashboard-as-table faithfully.
class RichMarkdown extends StatelessWidget {
  const RichMarkdown(this.text, {super.key});

  final String text;

  static final _rowRe = RegExp(r'^\s*\|.*\|\s*$');
  static final _sepRe = RegExp(r'^\s*\|[\s:|-]+\|\s*$');

  @override
  Widget build(BuildContext context) {
    final lines = text.split('\n');
    final blocks = <Widget>[];
    final buf = <String>[];
    void flush() {
      if (buf.isNotEmpty) {
        blocks.add(MarkdownLite(buf.join('\n')));
        buf.clear();
      }
    }

    var i = 0;
    while (i < lines.length) {
      if (_rowRe.hasMatch(lines[i])) {
        final rows = <String>[];
        while (i < lines.length && _rowRe.hasMatch(lines[i])) {
          rows.add(lines[i]);
          i++;
        }
        flush();
        final table = _table(context, rows);
        if (table != null) blocks.add(table);
      } else {
        buf.add(lines[i]);
        i++;
      }
    }
    flush();

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: [
        for (var j = 0; j < blocks.length; j++) ...[
          if (j > 0) const SizedBox(height: 8),
          blocks[j],
        ],
      ],
    );
  }

  Widget? _table(BuildContext context, List<String> rows) {
    final data = rows.where((r) => !_sepRe.hasMatch(r)).map((r) {
      var cells = r.trim();
      if (cells.startsWith('|')) cells = cells.substring(1);
      if (cells.endsWith('|')) cells = cells.substring(0, cells.length - 1);
      return cells.split('|').map((c) => c.trim()).toList();
    }).toList();
    if (data.isEmpty) return null;
    final cs = Theme.of(context).colorScheme;
    final cols = data.map((r) => r.length).reduce((a, b) => a > b ? a : b);

    TableRow rowOf(List<String> cells, bool header) => TableRow(
      decoration: header
          ? BoxDecoration(color: cs.surfaceContainerHighest)
          : null,
      children: [
        for (var c = 0; c < cols; c++)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
            child: Text(
              c < cells.length ? cells[c] : '',
              style: TextStyle(
                fontWeight: header ? FontWeight.w600 : FontWeight.normal,
                fontSize: 12.5,
              ),
            ),
          ),
      ],
    );

    return Align(
      alignment: Alignment.centerLeft,
      child: Container(
        decoration: BoxDecoration(
          border: Border.all(color: cs.outlineVariant),
          borderRadius: BorderRadius.circular(6),
        ),
        clipBehavior: Clip.antiAlias,
        child: IntrinsicWidth(
          child: Table(
            border: TableBorder.symmetric(
              inside: BorderSide(color: cs.outlineVariant),
            ),
            defaultColumnWidth: const IntrinsicColumnWidth(),
            children: [
              rowOf(data.first, true),
              for (final r in data.skip(1)) rowOf(r, false),
            ],
          ),
        ),
      ),
    );
  }
}
