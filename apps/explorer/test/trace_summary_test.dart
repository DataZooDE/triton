import 'package:flutter_test/flutter_test.dart';
import 'package:triton_explorer/api/trace_summary.dart';

/// `distinctTraces` collapses the newest-first `/v1/audit` entry list into
/// the distinct trace_ids available in the ring buffer — the data the Trace
/// page's "available traces" picker binds to.
void main() {
  Map<String, dynamic> entry({
    required String traceId,
    String when = '2026-06-17T00:00:00Z',
    String tool = 'echo',
    String protocol = 'rest',
    String phase = 'dispatch',
  }) => {
    'trace_id': traceId,
    'when': when,
    'tool': tool,
    'protocol': protocol,
    'phase': phase,
  };

  test('distinct trace_ids in newest-first first-seen order', () {
    // /v1/audit returns newest-first. Trace "a" has two phases (post is
    // newest), trace "b" one. Expect [a, b] — first-seen order preserved.
    final entries = [
      entry(traceId: 'a', when: '2026-06-17T00:00:03Z', phase: 'post'),
      entry(traceId: 'b', when: '2026-06-17T00:00:02Z'),
      entry(traceId: 'a', when: '2026-06-17T00:00:01Z', phase: 'dispatch'),
    ];
    final out = distinctTraces(entries);
    expect(out.map((t) => t.traceId).toList(), ['a', 'b']);
  });

  test('summary carries the newest entry fields and the phase count', () {
    final entries = [
      entry(
        traceId: 'a',
        when: '2026-06-17T00:00:03Z',
        tool: 'narrate',
        protocol: 'mcp',
        phase: 'post',
      ),
      entry(traceId: 'a', when: '2026-06-17T00:00:01Z', phase: 'dispatch'),
    ];
    final t = distinctTraces(entries).single;
    expect(t.traceId, 'a');
    expect(t.count, 2);
    // Newest (first-seen) entry's fields surface in the label.
    expect(t.latestWhen, '2026-06-17T00:00:03Z');
    expect(t.tool, 'narrate');
    expect(t.protocol, 'mcp');
  });

  test('entries with no trace_id are skipped', () {
    final entries = [
      {'when': 'x', 'tool': 'echo', 'protocol': 'rest'}, // no trace_id
      entry(traceId: ''),
      entry(traceId: 'real'),
    ];
    expect(distinctTraces(entries).map((t) => t.traceId).toList(), ['real']);
  });

  test('empty input yields no traces', () {
    expect(distinctTraces(const []), isEmpty);
  });
}
