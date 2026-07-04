import 'package:flutter/material.dart';

/// Renders the PORTABLE markdown subset agents are told to emit in `text`
/// components — exactly what the Google Chat adapter normalises to its own
/// syntax (`to_google_chat`): `#` headings, `- `/`* ` bullets, inline
/// `**bold**`, `*italic*`/`_italic_`, `` `code` ``, `[text](url)`.
/// Everything else renders verbatim. Dependency-free by design (no
/// flutter_markdown / url_launcher): links are styled, with the url in a
/// tooltip, not launched — the Explorer is a console, not a browser chrome.
class MarkdownLite extends StatelessWidget {
  const MarkdownLite(this.text, {super.key});

  final String text;

  @override
  Widget build(BuildContext context) {
    final base = DefaultTextStyle.of(context).style;
    final blocks = <Widget>[];
    for (final raw in text.split('\n')) {
      final line = raw.trimRight();
      if (line.isEmpty) {
        blocks.add(const SizedBox(height: 6));
        continue;
      }
      final heading = RegExp(r'^#{1,6}\s+').firstMatch(line);
      if (heading != null) {
        blocks.add(Padding(
          padding: const EdgeInsets.symmetric(vertical: 2),
          child: Text.rich(
            TextSpan(
                children:
                    _inline(context, line.substring(heading.end), base)),
            style: base.copyWith(
                fontWeight: FontWeight.bold, fontSize: (base.fontSize ?? 14) + 1),
          ),
        ));
        continue;
      }
      final bullet = RegExp(r'^\s*[-*]\s+').firstMatch(line);
      if (bullet != null) {
        blocks.add(Padding(
          padding: const EdgeInsets.only(left: 8, top: 1, bottom: 1),
          child: Text.rich(TextSpan(children: [
            const TextSpan(text: '•  '),
            ..._inline(context, line.substring(bullet.end), base),
          ])),
        ));
        continue;
      }
      blocks.add(Text.rich(TextSpan(children: _inline(context, line, base))));
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: blocks,
    );
  }

  /// Inline formatting: bold / italic / code / link spans. One pass, no
  /// nesting (the portable subset agents emit is flat).
  static List<InlineSpan> _inline(
      BuildContext context, String s, TextStyle base) {
    final spans = <InlineSpan>[];
    final pattern = RegExp(
        r'\*\*(.+?)\*\*|(?<!\*)\*([^*\s][^*]*)\*(?!\*)|_([^_\s][^_]*)_|`([^`]+)`|\[([^\]]+)\]\(([^)\s]+)\)');
    var last = 0;
    for (final m in pattern.allMatches(s)) {
      if (m.start > last) {
        spans.add(TextSpan(text: s.substring(last, m.start)));
      }
      if (m.group(1) != null) {
        spans.add(TextSpan(
            text: m.group(1),
            style: const TextStyle(fontWeight: FontWeight.bold)));
      } else if (m.group(2) != null || m.group(3) != null) {
        spans.add(TextSpan(
            text: m.group(2) ?? m.group(3),
            style: const TextStyle(fontStyle: FontStyle.italic)));
      } else if (m.group(4) != null) {
        spans.add(TextSpan(
            text: m.group(4),
            style: const TextStyle(fontFamily: 'monospace', fontSize: 13)));
      } else {
        // [text](url) — styled, tooltip carries the url (non-launching).
        spans.add(WidgetSpan(
          alignment: PlaceholderAlignment.baseline,
          baseline: TextBaseline.alphabetic,
          child: Tooltip(
            message: m.group(6)!,
            child: Text(
              m.group(5)!,
              style: base.copyWith(
                color: Theme.of(context).colorScheme.primary,
                decoration: TextDecoration.underline,
              ),
            ),
          ),
        ));
      }
      last = m.end;
    }
    if (last < s.length) {
      spans.add(TextSpan(text: s.substring(last)));
    }
    return spans;
  }
}
