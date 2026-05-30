import 'package:flutter/material.dart';

/// A compact "what is this panel / how to use it" banner shown at the
/// top of each feature page (and per-tool in the Playground). Keeps the
/// explorer self-explanatory for someone meeting Triton for the first
/// time — every screen says what it demonstrates and what to do.
class PanelHelp extends StatelessWidget {
  const PanelHelp({
    super.key,
    required this.what,
    required this.how,
    this.margin = const EdgeInsets.fromLTRB(16, 16, 16, 0),
  });

  /// One or two sentences: what this panel is / what the tool does.
  final String what;

  /// One or two sentences: the concrete steps to take here.
  final String how;

  final EdgeInsetsGeometry margin;

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final text = Theme.of(context).textTheme;
    return Card(
      margin: margin,
      elevation: 0,
      color: cs.surfaceContainerHigh,
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(Icons.info_outline, size: 18, color: cs.primary),
            const SizedBox(width: 10),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(what, style: text.bodyMedium),
                  const SizedBox(height: 8),
                  Text(
                    'How to use',
                    style: text.labelSmall?.copyWith(
                      color: cs.primary,
                      fontWeight: FontWeight.w700,
                      letterSpacing: 0.4,
                    ),
                  ),
                  const SizedBox(height: 2),
                  Text(
                    how,
                    style: text.bodySmall?.copyWith(
                      color: cs.onSurfaceVariant,
                    ),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}
