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

    test('auto-opens a surface BUTTON that references a ui:// resource', () {
      // The agent's report-as-surface pattern: the reply carries a button
      // whose `resource` names the report's MCP-App. The Explorer lifts it
      // so the report renders inline WITHOUT a click — through the REST
      // envelope, the agent's own body, and Triton's MCP double-wrap alike.
      const button = {
        'kind': 'button',
        'label': 'Open report: nba-report',
        'tool': 'render_report',
        'args': {
          'report_id': 'nba-report',
          'params': {'account': 'beverages-gmbh'},
        },
        'resource': 'ui://peacock/nba-report?account=beverages-gmbh',
      };
      const surface = {
        'surface': {
          'components': [
            {'kind': 'narration', 'text': 'Recorded.'},
            button,
          ],
        },
      };
      // REST: {latency_ms, result: {surface}, trace_id}
      expect(
        InvocationResult.uiFrom(const {
          'latency_ms': 5,
          'result': surface,
          'trace_id': 't',
        }),
        'ui://peacock/nba-report?account=beverages-gmbh',
      );
      // Triton MCP: the upstream body under structuredContent.
      expect(
        InvocationResult.uiFrom(const {
          'content': [],
          'structuredContent': {
            'latency_ms': 5,
            'result': surface,
          },
        }),
        'ui://peacock/nba-report?account=beverages-gmbh',
      );
      // _meta.ui still wins when both are present.
      expect(
        InvocationResult.uiFrom(const {
          '_meta': {
            'ui': {'resourceUri': 'ui://peacock/direct'},
          },
          'result': surface,
        }),
        'ui://peacock/direct',
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
