import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../providers/api_provider.dart';
import '../../../widgets/json_viewer.dart';
import '../../../widgets/panel_help.dart';
import '../../../api/friendly_error.dart';

/// Renders the loaded `adapter.yaml`. The endpoint returns
/// credentials redacted (vault refs are echoed verbatim; literal
/// credentials are masked as `<literal:N chars>`).
class ManifestPage extends ConsumerWidget {
  const ManifestPage({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final manifest = ref.watch(manifestProvider);
    return Scaffold(
      appBar: AppBar(
        title: const Text('Manifest'),
        actions: [
          IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: () => ref.invalidate(manifestProvider),
          ),
        ],
      ),
      body: Column(
        children: [
          const PanelHelp(
            what: 'Shows the loaded adapter.yaml — the chat-channel adapters, '
                'their tools and degrade rules — exactly as Triton parsed it, '
                'with credentials redacted.',
            how: 'Read-only. If it says no manifest, Triton is in v0.1 '
                '(HTTP-trio) mode; set TRITON_MANIFEST_PATH to an adapter.yaml '
                'to enable chat adapters.',
          ),
          Expanded(child: _body(context, ref, manifest)),
        ],
      ),
    );
  }

  Widget _body(BuildContext context, WidgetRef ref,
      AsyncValue<Map<String, dynamic>> manifest) {
    return manifest.when(
        loading: () => const Center(child: CircularProgressIndicator()),
        error: (e, _) => Padding(
          padding: const EdgeInsets.all(24),
          child: Card(
            color: Theme.of(context).colorScheme.errorContainer,
            child: Padding(
              padding: const EdgeInsets.all(16),
              child: Text(friendlyApiError('Could not load /v1/manifest', e)),
            ),
          ),
        ),
        data: (envelope) {
          if (envelope['loaded'] != true) {
            return const Center(
              child: Padding(
                padding: EdgeInsets.all(24),
                child: Text(
                  'No manifest is loaded.\n\n'
                  'Triton runs in v0.1 mode (HTTP trio only) when '
                  'TRITON_MANIFEST_PATH is unset. Set it to an '
                  'adapter.yaml path to enable chat-channel adapters.',
                  textAlign: TextAlign.center,
                ),
              ),
            );
          }
          final m = (envelope['manifest'] as Map?)?.cast<String, dynamic>() ??
              const <String, dynamic>{};
          return ListView(
            padding: const EdgeInsets.all(16),
            children: [
              _versionTile(context, m['version']?.toString() ?? '?'),
              const SizedBox(height: 12),
              ..._adapterCards(context, m),
              const SizedBox(height: 12),
              _toolsCard(context, m),
              const SizedBox(height: 12),
              ExpansionTile(
                title: const Text('Raw JSON'),
                children: [
                  Padding(
                    padding: const EdgeInsets.all(12),
                    child: JsonViewer(m),
                  ),
                ],
              ),
            ],
          );
        },
    );
  }

  Widget _versionTile(BuildContext context, String version) => Card(
        child: ListTile(
          leading: const Icon(Icons.tag),
          title: const Text('Manifest version'),
          subtitle: Text(version),
        ),
      );

  List<Widget> _adapterCards(BuildContext context, Map<String, dynamic> m) {
    final adapters = (m['adapters'] as Map?)?.cast<String, dynamic>() ??
        const <String, dynamic>{};
    if (adapters.isEmpty) {
      return [
        const Card(
          child: ListTile(
            title: Text('Adapters'),
            subtitle: Text('No chat-channel adapters declared.'),
          ),
        ),
      ];
    }
    return adapters.entries.map((e) {
      final cfg = (e.value as Map).cast<String, dynamic>();
      return Card(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  const Icon(Icons.chat_outlined),
                  const SizedBox(width: 8),
                  Text(e.key,
                      style: Theme.of(context).textTheme.titleMedium),
                  const SizedBox(width: 8),
                  Chip(
                    label: Text((cfg['kind'] as String?) ?? '?'),
                    visualDensity: VisualDensity.compact,
                  ),
                ],
              ),
              const SizedBox(height: 12),
              _section(context, 'Inbound',
                  (cfg['inbound'] as Map?)?.cast<String, dynamic>()),
              _section(context, 'Outbound',
                  (cfg['outbound'] as Map?)?.cast<String, dynamic>()),
              _section(context, 'Identity',
                  (cfg['identity'] as Map?)?.cast<String, dynamic>()),
              const SizedBox(height: 8),
              Text('Degrade rules',
                  style: Theme.of(context).textTheme.titleSmall),
              const SizedBox(height: 6),
              _degradeTable(
                context,
                (cfg['degrade'] as Map?)?.cast<String, dynamic>() ??
                    const <String, dynamic>{},
              ),
              const SizedBox(height: 8),
              Row(
                children: [
                  Chip(
                    avatar: const Icon(Icons.speed, size: 16),
                    label: Text(
                      'rate ${cfg['rate_limit']?['messages_per_sec'] ?? '?'}/s '
                      'burst ${cfg['rate_limit']?['burst'] ?? '?'}',
                    ),
                    visualDensity: VisualDensity.compact,
                  ),
                  const SizedBox(width: 8),
                  Chip(
                    avatar: const Icon(Icons.vpn_key, size: 16),
                    label: Text('correlation: ${cfg['correlation_key']}'),
                    visualDensity: VisualDensity.compact,
                  ),
                ],
              ),
            ],
          ),
        ),
      );
    }).toList(growable: false);
  }

  Widget _section(
      BuildContext context, String label, Map<String, dynamic>? body) {
    if (body == null) return const SizedBox.shrink();
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 4),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(label, style: Theme.of(context).textTheme.titleSmall),
          const SizedBox(height: 4),
          for (final e in body.entries)
            Padding(
              padding: const EdgeInsets.symmetric(vertical: 2),
              child: Row(
                children: [
                  SizedBox(
                    width: 140,
                    child: Text(e.key,
                        style: Theme.of(context).textTheme.bodySmall),
                  ),
                  Expanded(
                    child: SelectableText(
                      e.value.toString(),
                      style: const TextStyle(
                        fontFamily: 'monospace',
                        fontSize: 12,
                      ),
                    ),
                  ),
                ],
              ),
            ),
        ],
      ),
    );
  }

  Widget _degradeTable(BuildContext context, Map<String, dynamic> rules) {
    if (rules.isEmpty) {
      return Text('— none —',
          style: Theme.of(context).textTheme.bodySmall);
    }
    return Wrap(
      spacing: 8,
      runSpacing: 6,
      children: [
        for (final e in rules.entries)
          Chip(
            label: Text('${e.key} → ${e.value}'),
            visualDensity: VisualDensity.compact,
          ),
      ],
    );
  }

  Widget _toolsCard(BuildContext context, Map<String, dynamic> m) {
    final tools = (m['tools'] as Map?)?.cast<String, dynamic>() ??
        const <String, dynamic>{};
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                const Icon(Icons.handyman_outlined),
                const SizedBox(width: 8),
                Text('Tools (declared)',
                    style: Theme.of(context).textTheme.titleMedium),
              ],
            ),
            const SizedBox(height: 8),
            if (tools.isEmpty)
              Text('— no tools declared in manifest —',
                  style: Theme.of(context).textTheme.bodySmall)
            else
              for (final e in tools.entries)
                Padding(
                  padding: const EdgeInsets.symmetric(vertical: 4),
                  child: Row(
                    children: [
                      SizedBox(
                        width: 200,
                        child: Text(e.key,
                            style: const TextStyle(
                                fontFamily: 'monospace',
                                fontSize: 13)),
                      ),
                      Expanded(
                        child: Wrap(
                          spacing: 6,
                          children: [
                            for (final c in (e.value['surface_components']
                                    as List? ??
                                const []))
                              Chip(
                                label: Text(c.toString()),
                                visualDensity: VisualDensity.compact,
                              ),
                          ],
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
