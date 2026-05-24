/// Minimal Prometheus text-format parser covering what
/// triton-core::Metrics::render emits today: gauge / counter (no
/// histograms or summaries yet). Lives in the explorer because it
/// is purely a display concern.
class PromMetricFamily {
  PromMetricFamily({
    required this.name,
    required this.help,
    required this.type,
    required this.samples,
  });

  final String name;
  final String help;
  final String type; // gauge | counter | histogram | summary | untyped
  final List<PromSample> samples;
}

class PromSample {
  PromSample({required this.labels, required this.value});

  /// Stable map of label name → value. May be empty for unlabeled
  /// samples.
  final Map<String, String> labels;
  final double value;
}

class PromExposition {
  PromExposition(this.families);
  final List<PromMetricFamily> families;
}

/// Parse a Prometheus text exposition body. Forgiving — lines that
/// don't match the expected `# HELP / # TYPE / sample` triplet are
/// silently skipped so a future addition (e.g. histograms) doesn't
/// break the explorer's render until we extend this.
PromExposition parsePrometheus(String body) {
  final familyByName = <String, _PartialFamily>{};
  for (final raw in body.split('\n')) {
    final line = raw.trim();
    if (line.isEmpty) continue;
    if (line.startsWith('# HELP ')) {
      final rest = line.substring(7);
      final sp = rest.indexOf(' ');
      if (sp < 0) continue;
      final name = rest.substring(0, sp);
      final help = rest.substring(sp + 1);
      familyByName.putIfAbsent(name, _PartialFamily.new).help = help;
      continue;
    }
    if (line.startsWith('# TYPE ')) {
      final rest = line.substring(7);
      final sp = rest.indexOf(' ');
      if (sp < 0) continue;
      final name = rest.substring(0, sp);
      final type = rest.substring(sp + 1);
      familyByName.putIfAbsent(name, _PartialFamily.new).type = type;
      continue;
    }
    if (line.startsWith('#')) continue;
    // Sample line: `name[{labels}] value [timestamp]`
    final parsed = _parseSample(line);
    if (parsed == null) continue;
    familyByName
        .putIfAbsent(parsed.name, _PartialFamily.new)
        .samples
        .add(PromSample(labels: parsed.labels, value: parsed.value));
  }
  final families = familyByName.entries
      .map((e) => PromMetricFamily(
            name: e.key,
            help: e.value.help,
            type: e.value.type,
            samples: e.value.samples,
          ))
      .toList(growable: false)
    ..sort((a, b) => a.name.compareTo(b.name));
  return PromExposition(families);
}

class _PartialFamily {
  String help = '';
  String type = 'untyped';
  final List<PromSample> samples = [];
}

class _ParsedSample {
  _ParsedSample(this.name, this.labels, this.value);
  final String name;
  final Map<String, String> labels;
  final double value;
}

_ParsedSample? _parseSample(String line) {
  // Find name end at `{` or whitespace.
  final braceIdx = line.indexOf('{');
  final spaceIdx = line.indexOf(' ');
  final nameEnd = (braceIdx > 0 && (braceIdx < spaceIdx || spaceIdx < 0))
      ? braceIdx
      : spaceIdx;
  if (nameEnd <= 0) return null;
  final name = line.substring(0, nameEnd);
  Map<String, String> labels = const {};
  int valueStart = nameEnd;
  if (braceIdx == nameEnd) {
    final closeIdx = line.indexOf('}', braceIdx);
    if (closeIdx < 0) return null;
    labels = _parseLabels(line.substring(braceIdx + 1, closeIdx));
    valueStart = closeIdx + 1;
  }
  final tail = line.substring(valueStart).trim();
  if (tail.isEmpty) return null;
  // Take the first whitespace-separated token (ignore optional
  // timestamp).
  final tok = tail.split(RegExp(r'\s+')).first;
  final value = double.tryParse(tok);
  if (value == null) return null;
  return _ParsedSample(name, labels, value);
}

Map<String, String> _parseLabels(String inner) {
  // Walk the comma-separated list, respecting that label values are
  // quoted and may contain commas. The metrics we emit don't, but
  // this stays correct for future additions.
  final out = <String, String>{};
  int i = 0;
  while (i < inner.length) {
    // skip whitespace
    while (i < inner.length && inner[i] == ' ') {
      i++;
    }
    final eq = inner.indexOf('=', i);
    if (eq < 0) break;
    final key = inner.substring(i, eq).trim();
    if (eq + 1 >= inner.length || inner[eq + 1] != '"') break;
    int j = eq + 2;
    final buf = StringBuffer();
    while (j < inner.length && inner[j] != '"') {
      if (inner[j] == r'\' && j + 1 < inner.length) {
        // unescape \\ and \"
        buf.write(inner[j + 1]);
        j += 2;
        continue;
      }
      buf.write(inner[j]);
      j++;
    }
    out[key] = buf.toString();
    // skip closing quote and optional comma
    i = j + 1;
    while (i < inner.length && (inner[i] == ',' || inner[i] == ' ')) {
      i++;
    }
  }
  return out;
}
