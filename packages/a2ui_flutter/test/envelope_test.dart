// The envelope layer is the part of A2UI that has already shipped a bug: a
// client read top-level `version`/`stream` instead of unwrapping `result`
// first, and nothing failed until a surface rendered wrong. These tests pin
// the unwrap and the version-precedence rules so a second host cannot make
// the same mistake independently.
//
// Every negative assertion here carries a positive control in the same test,
// because "the decoy was ignored" and "the parser reads nothing at all" look
// identical from a single expectation.

import 'package:a2ui_flutter/a2ui_flutter.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('A2uiEnvelope.parse — unwrapping', () {
    test('reads `result` and ignores the decoys beside it', () {
      final env = A2uiEnvelope.parse(const {
        'latency_ms': 12,
        // Decoys: the shapes a client gets if it forgets to unwrap.
        'version': '0.8',
        'stream': [
          {'Component': {'Text': {'value': 'outer'}}},
        ],
        'result': {
          'version': '0.9',
          'stream': [
            {'type': 'text', 'text': 'inner'},
          ],
        },
      });

      expect(env.version, '0.9');
      expect((env.stream.single as Map)['text'], 'inner');
    });

    test(
      'reads the top level when there is no `result` — the control for the '
      'unwrap test above',
      () {
        // Without this, "ignored the decoy" is indistinguishable from
        // "never reads top-level fields at all".
        final env = A2uiEnvelope.parse(const {
          'version': '0.8',
          'stream': [
            {'Component': {'Text': {'value': 'outer'}}},
          ],
        });

        expect(env.version, '0.8');
        expect((env.stream.single as Map).containsKey('Component'), isTrue);
      },
    );

    test('a non-map `result` is not treated as the envelope', () {
      // Triton nests the stream under `result`; an upstream returning a
      // scalar `result` must not blank the surface.
      final env = A2uiEnvelope.parse(const {
        'result': 'not-an-object',
        'version': '0.9',
        'stream': [
          {'type': 'text', 'text': 'top'},
        ],
      });

      expect(env.version, '0.9');
      expect((env.stream.single as Map)['text'], 'top');
    });
  });

  group('A2uiEnvelope.parse — version precedence', () {
    test('an explicit version wins over the envelope field', () {
      // The wire omits `version`; the caller knows it from the `Accept` it
      // sent. When both are present the caller is authoritative.
      final explicit = A2uiEnvelope.parse(
        const {
          'result': {
            'version': '0.8',
            'stream': [
              {'type': 'text', 'text': 'x'},
            ],
          },
        },
        version: '0.9',
      );
      expect(explicit.version, '0.9');

      // Control: with no explicit version the envelope field is honoured, so
      // the assertion above is about precedence rather than about the
      // envelope field being unread.
      final implicit = A2uiEnvelope.parse(const {
        'result': {
          'version': '0.8',
          'stream': [
            {'type': 'text', 'text': 'x'},
          ],
        },
      });
      expect(implicit.version, '0.8');
    });

    test('sniffs 0.8 from a PascalCase `Component` wrapper', () {
      final env = A2uiEnvelope.parse(const {
        'result': {
          'stream': [
            {'Component': {'Text': {'value': 'x'}}},
          ],
        },
      });
      expect(env.version, '0.8');
    });

    test('sniffs 0.9 from a flat lowercase `type`', () {
      final env = A2uiEnvelope.parse(const {
        'result': {
          'stream': [
            {'type': 'text', 'text': 'x'},
          ],
        },
      });
      expect(env.version, '0.9');
    });

    test('an unrecognisable envelope resolves to no version, not a guess', () {
      // A wrong guess renders the wrong tree silently. Null is what lets the
      // host show "unknown A2UI version" instead.
      final unknown = A2uiEnvelope.parse(const {
        'result': {
          'stream': [
            {'wat': 'x'},
          ],
        },
      });
      expect(unknown.version, isNull);
      // Control: the same parser does resolve a version it recognises, so the
      // null above is a refusal rather than a broken sniffer.
      expect(
        A2uiEnvelope.parse(const {
          'result': {
            'stream': [
              {'type': 'text'},
            ],
          },
        }).version,
        '0.9',
      );
    });

    test('an empty or missing stream yields an empty stream, never null', () {
      expect(A2uiEnvelope.parse(const {}).stream, isEmpty);
      expect(A2uiEnvelope.parse(const {'stream': []}).stream, isEmpty);
      // Control: a present stream survives, so "empty" above is not the
      // parser dropping everything.
      expect(
        A2uiEnvelope.parse(const {
          'stream': [
            {'type': 'text'},
          ],
        }).stream,
        hasLength(1),
      );
    });
  });
}
