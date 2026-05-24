import 'package:flutter/material.dart';

class AdaptersPage extends StatelessWidget {
  const AdaptersPage({super.key});

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(title: const Text('Adapters')),
        body: const Center(
          child: Padding(
            padding: EdgeInsets.all(24),
            child: Text(
              'Side-by-side REST | MCP | A2A compare lands in PR E4 — '
              "fires the same {tool, args} against all three of Triton's "
              'adapters and shows correlation_id parity across them.',
              textAlign: TextAlign.center,
            ),
          ),
        ),
      );
}
