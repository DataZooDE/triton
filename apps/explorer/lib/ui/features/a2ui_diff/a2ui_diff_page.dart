import 'package:flutter/material.dart';

class A2uiDiffPage extends StatelessWidget {
  const A2uiDiffPage({super.key});

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(title: const Text('A2UI diff')),
        body: const Center(
          child: Padding(
            padding: EdgeInsets.all(24),
            child: Text(
              'A2UI v0.8 vs v0.9 side-by-side renderer lands in PR E3 — '
              'invokes the same tool with both Accept versions and renders '
              'them in two native Flutter views to validate the builders.',
              textAlign: TextAlign.center,
            ),
          ),
        ),
      );
}
