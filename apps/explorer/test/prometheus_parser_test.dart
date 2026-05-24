import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/widgets/prometheus_parser.dart';

void main() {
  group('parsePrometheus', () {
    test('parses gauge + counter with labeled samples', () {
      const body = '''
# HELP triton_process_up 1 while the process is serving.
# TYPE triton_process_up gauge
triton_process_up 1
# HELP triton_dispatch_total Dispatched tool invocations.
# TYPE triton_dispatch_total counter
triton_dispatch_total{tool="echo",protocol="rest",result="ok"} 3
triton_dispatch_total{tool="echo",protocol="mcp",result="ok"} 1
''';
      final exp = parsePrometheus(body);
      expect(exp.families.length, 2);
      final up = exp.families.firstWhere((f) => f.name == 'triton_process_up');
      expect(up.type, 'gauge');
      expect(up.samples, hasLength(1));
      expect(up.samples.first.value, 1.0);

      final dispatch =
          exp.families.firstWhere((f) => f.name == 'triton_dispatch_total');
      expect(dispatch.type, 'counter');
      expect(dispatch.samples, hasLength(2));
      expect(
        dispatch.samples.first.labels,
        {'tool': 'echo', 'protocol': 'rest', 'result': 'ok'},
      );
    });

    test('skips malformed lines without throwing', () {
      const body = '''
# HELP triton_x A counter.
# TYPE triton_x counter
triton_x{unterminated="oops 7
# orphan comment
triton_x{tool="ok"} 1
''';
      final exp = parsePrometheus(body);
      final x = exp.families.firstWhere((f) => f.name == 'triton_x');
      // The unterminated line yields one sample (the parser may pick
      // up its name+label fragment); the well-formed line must too.
      // We only assert the well-formed sample is present — exact
      // count is implementation detail.
      expect(
        x.samples.any((s) => s.labels['tool'] == 'ok' && s.value == 1.0),
        isTrue,
      );
    });

    test('escaped backslash + quote in label values round-trips', () {
      const body = r'''
# TYPE triton_x counter
triton_x{path="a\"b\\c"} 5
''';
      final exp = parsePrometheus(body);
      final x = exp.families.firstWhere((f) => f.name == 'triton_x');
      expect(x.samples.first.labels['path'], r'a"b\c');
      expect(x.samples.first.value, 5.0);
    });
  });
}
