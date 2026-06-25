/// A distinct `trace_id` present in the audit ring buffer, summarised for
/// the Trace page's "available traces" picker so an operator can browse and
/// click a trace instead of having to already know its id.
class TraceSummary {
  const TraceSummary({
    required this.traceId,
    required this.latestWhen,
    required this.tool,
    required this.protocol,
    required this.count,
  });

  final String traceId;

  /// `when` of the NEWEST audit entry for this trace (the buffer is
  /// newest-first, so this is the first entry we see for the id).
  final String latestWhen;

  /// Tool + protocol of that newest entry — enough to recognise the call.
  final String tool;
  final String protocol;

  /// How many audit entries (phases) carry this trace_id in the slice.
  final int count;
}

/// Collapse a newest-first `/v1/audit` entry list (each a decoded JSON map)
/// into the distinct trace_ids it contains, preserving first-seen order so
/// the most recent traces lead. Entries without a non-empty `trace_id` are
/// skipped. Pure (no I/O) so it's unit-testable; the provider feeds it the
/// `/v1/audit` response.
List<TraceSummary> distinctTraces(List<Map<String, dynamic>> entries) {
  final order = <String>[];
  final newest = <String, Map<String, dynamic>>{};
  final counts = <String, int>{};
  for (final e in entries) {
    final id = (e['trace_id'] as String?)?.trim() ?? '';
    if (id.isEmpty) continue;
    if (!newest.containsKey(id)) {
      // First sighting = newest entry for this id (input is newest-first).
      order.add(id);
      newest[id] = e;
    }
    counts[id] = (counts[id] ?? 0) + 1;
  }
  return [
    for (final id in order)
      TraceSummary(
        traceId: id,
        latestWhen: (newest[id]!['when'] as String?) ?? '',
        tool: (newest[id]!['tool'] as String?) ?? '',
        protocol: (newest[id]!['protocol'] as String?) ?? '',
        count: counts[id]!,
      ),
  ];
}
