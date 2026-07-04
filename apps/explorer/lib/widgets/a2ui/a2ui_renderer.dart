import 'package:flutter/material.dart';

import 'a2ui_v08_renderer.dart';
import 'a2ui_v09_renderer.dart';

/// Dispatches to the appropriate renderer. Two independent trees
/// underneath — see ADR-4 in `doc/architecture.md`.
///
/// The wire envelope from Triton's HTTP trio nests the A2UI stream
/// under `result` (`{latency_ms, result: {stream: [...]}}`) and does
/// **not** carry a `version` field — the version is implicit in the
/// `Accept` the caller sent. So this widget:
///   1. unwraps `result` when present (synthetic test envelopes that
///      already have `stream` at top level still work), and
///   2. resolves the version from the explicit [version] the caller
///      requested, falling back to an envelope `version` field, then
///      to sniffing the stream shape (v0.8 wraps each node in a
///      PascalCase `Component`; v0.9 uses a flat lowercase `type`).
class A2UIRenderer extends StatelessWidget {
  const A2UIRenderer({
    super.key,
    required this.envelope,
    this.version,
    this.onAction,
    this.onOpenResource,
  });

  final Map<String, dynamic> envelope;

  /// The A2UI version the caller negotiated (e.g. '0.8' / '0.9').
  /// Authoritative when set, because the wire envelope omits it.
  final String? version;

  final void Function(String tool, Map<String, dynamic> args)? onAction;

  /// A `sources` item was clicked: open its `ui://` resource inline.
  /// Sources never auto-open — this fires only on an explicit tap.
  final void Function(String uri)? onOpenResource;

  @override
  Widget build(BuildContext context) {
    final inner = (envelope['result'] is Map)
        ? (envelope['result'] as Map).cast<String, dynamic>()
        : envelope;
    final resolved =
        version ?? (inner['version'] as String?) ?? _sniff(inner);
    switch (resolved) {
      case '0.8':
        return A2UIv08Renderer(
            envelope: inner, onAction: onAction, onOpenResource: onOpenResource);
      case '0.9':
        return A2UIv09Renderer(
            envelope: inner, onAction: onAction, onOpenResource: onOpenResource);
      default:
        return Card(
          color: Colors.amber.shade100,
          child: Padding(
            padding: const EdgeInsets.all(12),
            child: Text(
              'Unknown A2UI version: ${resolved ?? 'missing'}.\n'
              'Add a renderer for it under widgets/a2ui/.',
            ),
          ),
        );
    }
  }

  /// Best-effort version detection from the stream's first node when
  /// neither an explicit nor an envelope version is available.
  static String? _sniff(Map<String, dynamic> envelope) {
    final stream = envelope['stream'] as List?;
    if (stream == null || stream.isEmpty) return null;
    final first = stream.first;
    if (first is Map) {
      if (first.containsKey('Component')) return '0.8';
      if (first.containsKey('type')) return '0.9';
    }
    return null;
  }
}
