// `InvocationResult.uiFrom` lifts an MCP-Apps `ui://` resource link out of a
// tool result's `_meta.ui.resourceUri`, accepting both the inner result and a
// REST envelope that nests it under `result`.

import 'package:flutter_test/flutter_test.dart';
import 'package:triton_explorer/api/models.dart';

void main() {
  group('InvocationResult.uiFrom', () {
    test('reads _meta.ui.resourceUri from an inner result (MCP)', () {
      expect(
        InvocationResult.uiFrom(const {
          'structuredContent': {'rows': []},
          '_meta': {
            'ui': {'resourceUri': 'ui://peacock/northwind-monthly-revenue'},
          },
        }),
        'ui://peacock/northwind-monthly-revenue',
      );
    });

    test('reads it from a REST envelope nested under result', () {
      expect(
        InvocationResult.uiFrom(const {
          'latency_ms': 5,
          'result': {
            '_meta': {
              'ui': {'resourceUri': 'ui://peacock/r1'},
            },
          },
        }),
        'ui://peacock/r1',
      );
    });

    test('null when absent or empty', () {
      expect(InvocationResult.uiFrom(const {'result': {}}), isNull);
      expect(InvocationResult.uiFrom(null), isNull);
      expect(
        InvocationResult.uiFrom(const {
          '_meta': {
            'ui': {'resourceUri': ''},
          },
        }),
        isNull,
      );
    });
  });
}
