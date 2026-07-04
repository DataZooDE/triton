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
      // The NEGOTIATED v0.9 envelope re-shapes the surface: components ride
      // a `stream` array with `type`/`action` keys — the lift must find the
      // resource there too (Triton keeps it since #172).
      expect(
        InvocationResult.uiFrom(const {
          'content': [],
          'structuredContent': {
            'latency_ms': 5,
            'result': {
              'version': '0.9',
              'stream': [
                {'type': 'narration', 'text': 'Recorded.'},
                {
                  'type': 'button',
                  'label': 'Open report: nba-report',
                  'action': {'tool': 'render_report', 'args': {}},
                  'resource': 'ui://peacock/nba-report?account=beverages-gmbh',
                },
              ],
            },
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

    test('SOURCES never auto-open: their items are invisible to the lift', () {
      // A `sources` component carries its `ui://` resources one level down
      // (`items[].resource`) — the click-to-open contract. The lift must
      // return null when sources are the ONLY resource-bearing component…
      const sources = {
        'kind': 'sources',
        'items': [
          {
            'label': 'account · initech-corp',
            'resource': 'ui://peacock/document?skill=account&id=initech-corp',
          },
        ],
      };
      expect(
        InvocationResult.uiFrom(const {
          'latency_ms': 5,
          'result': {
            'surface': {
              'components': [
                {'kind': 'narration', 'text': 'Flagged.'},
                sources,
              ],
            },
          },
        }),
        isNull,
      );
      // …including on the NEGOTIATED envelope's `stream` shape…
      expect(
        InvocationResult.uiFrom(const {
          'content': [],
          'structuredContent': {
            'result': {
              'version': '0.9',
              'stream': [
                {'type': 'narration', 'text': 'Flagged.'},
                {
                  'type': 'sources',
                  'items': [
                    {
                      'label': 'account · initech-corp',
                      'resource':
                          'ui://peacock/document?skill=account&id=initech-corp',
                    },
                  ],
                },
              ],
            },
          },
        }),
        isNull,
      );
      // …while a SIBLING report button still auto-opens as before.
      expect(
        InvocationResult.uiFrom(const {
          'latency_ms': 5,
          'result': {
            'surface': {
              'components': [
                sources,
                {
                  'kind': 'button',
                  'label': 'Open report: nba-report',
                  'tool': 'render_report',
                  'args': {},
                  'resource': 'ui://peacock/nba-report?account=initech-corp',
                },
              ],
            },
          },
        }),
        'ui://peacock/nba-report?account=initech-corp',
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
