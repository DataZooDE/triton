import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/models.dart';
import '../../../providers/api_provider.dart';
import '../../../widgets/a2ui/a2ui_renderer.dart';
import '../../../widgets/a2ui/ui_resource_view.dart';

/// A chrome-free, full-bleed render of one tool's A2UI surface — the Explorer
/// used as a **generic dashboard** you can embed (e.g. an iframe pointed at
/// `?surface=<tool>`). It invokes the tool over REST (A2UI v0.9), renders the
/// surface, and embeds any `ui://` report the result points at
/// (`_meta.ui.resourceUri`). Surface buttons re-invoke statelessly (FR-D-3),
/// so it stays interactive without the Console's debugging chrome.
class GenericSurfacePage extends ConsumerStatefulWidget {
  const GenericSurfacePage({
    super.key,
    required this.tool,
    this.args = const {},
    this.title,
  });

  final String tool;
  final Map<String, dynamic> args;
  final String? title;

  @override
  ConsumerState<GenericSurfacePage> createState() => _GenericSurfacePageState();
}

class _GenericSurfacePageState extends ConsumerState<GenericSurfacePage> {
  InvocationResult? _result;
  bool _loading = false;

  @override
  void initState() {
    super.initState();
    _invoke(widget.tool, widget.args);
  }

  /// Invoke a tool and show its surface. Surface actions call back here with
  /// the button's own tool, so a cross-tool button (an agent surface offering
  /// a `render_report`) dispatches correctly.
  Future<void> _invoke(String tool, Map<String, dynamic> args) async {
    setState(() => _loading = true);
    final r = await ref
        .read(restClientProvider)
        .invoke(tool, args, a2uiVersion: '0.9');
    if (!mounted) return;
    setState(() {
      _result = r;
      _loading = false;
    });
  }

  @override
  Widget build(BuildContext context) {
    final r = _result;
    return Scaffold(
      body: SafeArea(
        child: r == null
            ? const Center(child: CircularProgressIndicator())
            : SingleChildScrollView(
                padding: const EdgeInsets.all(20),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    if (widget.title != null && widget.title!.isNotEmpty)
                      Padding(
                        padding: const EdgeInsets.only(bottom: 12),
                        child: Text(widget.title!,
                            style: Theme.of(context).textTheme.headlineSmall),
                      ),
                    if (_loading) const LinearProgressIndicator(),
                    if (r.error != null)
                      Card(
                        color: Theme.of(context).colorScheme.errorContainer,
                        child: Padding(
                          padding: const EdgeInsets.all(12),
                          child: Text(r.error!),
                        ),
                      )
                    else
                      A2UIRenderer(
                        envelope: r.raw,
                        version: '0.9',
                        onAction: _invoke,
                      ),
                    if (r.uiResourceUri != null) ...[
                      const SizedBox(height: 16),
                      UiResourceView(uri: r.uiResourceUri!),
                    ],
                  ],
                ),
              ),
      ),
    );
  }
}
