import 'package:flutter/material.dart';

/// Renders the PORTABLE markdown subset agents are told to emit in `text`
/// components — exactly what the Google Chat adapter normalises to its own
/// syntax (`to_google_chat`): `#` headings, `- `/`* ` bullets, inline
/// `**bold**`, `*italic*`/`_italic_`, `` `code` ``, `[text](url)`.
/// Everything else renders verbatim. Dependency-free by design (no
/// flutter_markdown / url_launcher): links are styled, with the url in a
/// tooltip, not launched — the Explorer is a console, not a browser chrome.
///
/// Plus, when [pills] is non-null, `[[skill::id]]` wikilinks — the v0.9
/// `text.pills` vocabulary. See that field for why absence is the mechanism.
class MarkdownLite extends StatelessWidget {
  const MarkdownLite(this.text, {super.key, this.pills});

  final String text;

  /// Display names for the `[[skill::id]]` wikilinks in [text], keyed by id.
  ///
  /// Three states, all meaningful:
  ///
  ///  * **null** — this caller's contract has no wikilinks (v0.8). Any `[[…]]`
  ///    in the text is content and renders verbatim, exactly as before.
  ///  * **a map with the id** — render the display name.
  ///  * **a map without the id, or an empty map** — render the *bare id*. This
  ///    is the D27 degrade, not an error: the label can only come from a read
  ///    the caller is entitled to make, so an instance they may not see simply
  ///    has no entry. Absence is the whole mechanism, which is why the empty
  ///    map (every id denied — the wire omits the field entirely) must behave
  ///    like a partial one and not fall back to leaking raw syntax.
  final Map<String, String>? pills;

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

  /// Inline formatting: bold / italic / code / link / wikilink spans. One
  /// pass, no nesting (the portable subset agents emit is flat).
  List<InlineSpan> _inline(BuildContext context, String s, TextStyle base) {
    final spans = <InlineSpan>[];
    final pattern = RegExp(
        r'\*\*(.+?)\*\*|(?<!\*)\*([^*\s][^*]*)\*(?!\*)|_([^_\s][^_]*)_|`([^`]+)`|\[([^\]]+)\]\(([^)\s]+)\)|(?<wiki>\[\[[A-Za-z0-9_]+::[A-Za-z0-9_.:-]+\]\])');
    var last = 0;
    for (final m in pattern.allMatches(s)) {
      if (m.start > last) {
        spans.add(TextSpan(text: s.substring(last, m.start)));
      }
      final wiki = m.namedGroup('wiki');
      if (wiki != null) {
        final resolved = pills;
        if (resolved == null) {
          // No wikilink contract here (v0.8): the brackets are content.
          spans.add(TextSpan(text: wiki));
        } else {
          final id = wiki.substring(2, wiki.length - 2).split('::').last;
          spans.add(WidgetSpan(
            alignment: PlaceholderAlignment.middle,
            child: _Pill(resolved[id] ?? id),
          ));
        }
      } else if (m.group(1) != null) {
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

/// A resolved (or degraded) wikilink, inline. Deliberately plain: the shape is
/// a chip, the colours come from the host's scheme, and nothing here encodes
/// whether the label was resolved — a reader must not be able to tell an
/// unlabelled reference from a labelled one by its styling, or the D27 degrade
/// becomes a side channel announcing "there is something here you may not see".
class _Pill extends StatelessWidget {
  const _Pill(this.label);
  final String label;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 1),
      decoration: BoxDecoration(
        color: scheme.secondaryContainer,
        borderRadius: BorderRadius.circular(6),
      ),
      child: Text(
        label,
        style: TextStyle(
          fontSize: (DefaultTextStyle.of(context).style.fontSize ?? 14) - 1,
          color: scheme.onSecondaryContainer,
        ),
      ),
    );
  }
}
