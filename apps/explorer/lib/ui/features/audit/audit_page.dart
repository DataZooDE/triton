import 'package:flutter/material.dart';

class AuditPage extends StatelessWidget {
  const AuditPage({super.key});

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(title: const Text('Audit')),
        body: const Center(
          child: Padding(
            padding: EdgeInsets.all(24),
            child: Text(
              'Triton emits audit lines to stdout today; once a retrieval '
              'endpoint lands, this page renders the two-record '
              'accepted/completed shape with trace_id correlation. Stay tuned.',
              textAlign: TextAlign.center,
            ),
          ),
        ),
      );
}
