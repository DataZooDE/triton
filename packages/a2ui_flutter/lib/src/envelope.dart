/// The A2UI wire envelope, parsed once.
///
/// This is the smallest piece of A2UI that every host has to get right and
/// that none of them can see is wrong: the wire nests the stream under
/// `result` (`{latency_ms, result: {stream: [...]}}`) and carries no
/// `version` — the version is implicit in the `Accept` the caller sent. A
/// host that reads top-level `version`/`stream` gets a plausible-looking
/// empty surface rather than an error, which is exactly how that bug shipped
/// once already.
///
/// Parsing lives here, apart from the widgets, so a host that renders A2UI
/// with its own design system (a phone app, a report runtime) can share the
/// contract without inheriting a look.
///
/// Note what this class deliberately does **not** do: it does not normalise
/// v0.8 and v0.9 into one component model. ADR-4 keeps the two versions in
/// isolated trees so a schema change in one cannot ripple into the other, and
/// a shared component model would be precisely that shared base. The envelope
/// resolves *which* version is in play and hands the stream over untouched.
class A2uiEnvelope {
  const A2uiEnvelope({required this.version, required this.stream});

  /// `'0.8'` · `'0.9'` · null when the envelope is unrecognisable.
  ///
  /// Null rather than a guess: a wrong guess renders the wrong tree in
  /// silence, whereas null lets the host say it does not know.
  final String? version;

  /// The stream as it arrived. Entries are usually maps but are not required
  /// to be — a malformed node is the renderer's problem to degrade, not the
  /// parser's to drop, because dropping it hides the producer's bug.
  final List<dynamic> stream;

  /// Parse [raw] as it came off the wire.
  ///
  /// [version] is the version the caller negotiated. It wins over an envelope
  /// field because the caller asked for it explicitly and the wire's own
  /// field is optional.
  static A2uiEnvelope parse(Map<String, dynamic> raw, {String? version}) {
    final inner = raw['result'] is Map
        ? (raw['result'] as Map).cast<String, dynamic>()
        : raw;
    final stream = (inner['stream'] as List?) ?? const [];
    return A2uiEnvelope(
      version: version ?? (inner['version'] as String?) ?? _sniff(stream),
      stream: stream,
    );
  }

  /// Best-effort version detection from the first node, used only when
  /// neither the caller nor the envelope said.
  static String? _sniff(List<dynamic> stream) {
    if (stream.isEmpty) return null;
    final first = stream.first;
    if (first is Map) {
      // v0.8 wraps every node in a PascalCase `Component`; v0.9 flattens it
      // to a lowercase `type`. One key each is enough to tell them apart.
      if (first.containsKey('Component')) return '0.8';
      if (first.containsKey('type')) return '0.9';
    }
    return null;
  }
}
