import 'package:flutter/material.dart';

class PlaygroundPage extends StatelessWidget {
  const PlaygroundPage({super.key});

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(title: const Text('Playground')),
        body: const Center(
          child: Padding(
            padding: EdgeInsets.all(24),
            child: Text(
              'Tool playground lands in PR E2 — lists every tool from '
              "Triton's /v1/tools and renders a JSON-schema form per tool. "
              'When the upstream agent ships a new tool, it shows up here '
              'on next refresh, no UI work needed.',
              textAlign: TextAlign.center,
            ),
          ),
        ),
      );
}
