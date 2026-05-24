import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../providers/runtime_provider.dart';

class SettingsPage extends ConsumerWidget {
  const SettingsPage({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final base = ref.watch(tritonBaseUrlProvider);
    return Scaffold(
      appBar: AppBar(title: const Text('Settings')),
      body: ListView(
        padding: const EdgeInsets.all(24),
        children: [
          Card(
            child: ListTile(
              title: const Text('Triton base URL'),
              subtitle: Text(base),
              trailing: const Icon(Icons.lock_outline),
            ),
          ),
          const SizedBox(height: 16),
          const Text(
            'Editable settings (base URL override, token paste, default A2UI '
            'version) land in PR E2.',
          ),
        ],
      ),
    );
  }
}
