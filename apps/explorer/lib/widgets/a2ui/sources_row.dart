import 'package:flutter/material.dart';

/// One clickable reference in a [SourcesRow].
class SourceChip {
  const SourceChip({required this.label, required this.resource});
  final String label;

  /// The `ui://` resource a tap opens.
  final String resource;
}

/// The `sources` component: click-to-open references to the documents a
/// reply touched. Nothing opens on render — the host's auto-open lift
/// skips sources by construction (items carry their resource one level
/// down) and [onOpen] fires only on an explicit tap. Version-agnostic
/// leaf widget shared by both A2UI renderers (a presentation atom, not
/// envelope parsing — ADR-4 applies to the latter).
class SourcesRow extends StatelessWidget {
  const SourcesRow({super.key, required this.items, this.onOpen});

  final List<SourceChip> items;
  final void Function(String uri)? onOpen;

  @override
  Widget build(BuildContext context) {
    if (items.isEmpty) return const SizedBox.shrink();
    final style = Theme.of(context).textTheme.bodySmall?.copyWith(
          color: Theme.of(context).colorScheme.onSurfaceVariant,
        );
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Sources', style: style),
          const SizedBox(height: 4),
          Wrap(
            spacing: 6,
            runSpacing: 6,
            children: [
              for (final s in items)
                ActionChip(
                  avatar: const Icon(Icons.description_outlined, size: 16),
                  label: Text(s.label),
                  onPressed: onOpen == null || s.resource.isEmpty
                      ? null
                      : () => onOpen!(s.resource),
                ),
            ],
          ),
        ],
      ),
    );
  }
}
