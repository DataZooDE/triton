import 'package:flutter/material.dart';

import '../api/models.dart';

/// Per-tool "what is this / what to do" copy for the bundled demo tools
/// (and the `hello` adk-rust agent). Unknown tools fall back to a hint
/// derived from the tool's schema + `returns_a2ui` flag, so a tool
/// shipped later still gets a sensible banner with no UI work.
({String what, String how}) toolGuide(ToolDescriptor t) {
  switch (t.name) {
    case 'hello':
      return (
        what: 'The adk-rust hello-world agent: a real upstream LLM agent '
            'Triton dispatches into. It greets a subject and returns an A2UI '
            'surface (a narration + a "Greet again" button).',
        how: 'Enter a subject, pick a protocol (REST/MCP/A2A) and an A2UI '
            'version, then Send. Edit the request envelope and Send again to '
            'inject your own message, or press "Send to all 3" for parity.',
      );
    case 'echo':
      return (
        what: 'The simplest tool — returns whatever you send it. Proves a '
            'request reaches the dispatcher and an audit line is emitted.',
        how: 'Type a message and press Send; you get it back with a '
            'trace_id you can look up on the Audit page.',
      );
    case 'narrate':
      return (
        what: 'Builds a small A2UI surface (a line of text, a narration, and '
            'a Refresh button) from a subject — the A2UI builder + the '
            'interactive round-trip.',
        how: 'Enter a subject, pick A2UI v0.9, and Send. Then click the '
            'Refresh button in the rendered surface to re-invoke the tool.',
      );
    case 'demo_panel':
      return (
        what: 'Renders the full A2UI component vocabulary at once — text, '
            'narration, a dashboard, a selection, a form, and a button. The '
            'reference surface.',
        how: 'Pick A2UI v0.8 or v0.9 and Send. Click a selection chip or '
            'the button to watch the surface re-invoke a tool (the stateless '
            'round-trip).',
      );
    case 'delay':
      return (
        what: 'Sleeps for the given number of milliseconds, then returns. '
            'Used to exercise timeouts and graceful shutdown (SIGTERM drain).',
        how: 'Enter a value in ms (0–5000) and Send; the response comes '
            'back after that delay.',
      );
    case 'form_only_demo':
      return (
        what: 'Returns a single-field A2UI form — models a chat modal (e.g. '
            'Discord). Demonstrates form submission.',
        how: 'Pick A2UI, Send, fill the field, and press submit; that '
            'dispatches submitted_form with your values.',
      );
    case 'form_only_demo_multi':
      return (
        what: 'Like form_only_demo but with several fields — models a '
            'multi-field chat form (e.g. Telegram).',
        how: 'Pick A2UI, Send, fill the fields, and submit; the values are '
            'dispatched to submitted_form.',
      );
    case 'submitted_form':
      return (
        what: 'The target tool a form submits to — it echoes back the fields '
            'it received.',
        how: 'Usually invoked for you when you submit a form in demo_panel or '
            'form_only_demo. You can also call it directly with arbitrary JSON '
            'args to see the echo.',
      );
    case 'empty_surface':
      return (
        what: 'Returns an A2UI surface with no components — exercises the '
            'empty / fallback render path.',
        how: 'Pick an A2UI version and Send to see how the renderer handles '
            'an empty surface.',
      );
    default:
      final hasArgs =
          ((t.inputSchema['properties'] as Map?)?.isNotEmpty) ?? false;
      final how = StringBuffer();
      if (hasArgs) how.write('Fill in the arguments, then ');
      how.write('press Send');
      if (t.returnsA2ui) {
        how.write(' (choose an A2UI version to render the surface, or JSON '
            'for the raw result)');
      }
      how.write('; the response shows status, latency, trace_id and raw JSON.');
      return (
        what: 'A tool registered on this Triton gateway, discovered live from '
            '/v1/tools.',
        how: how.toString(),
      );
  }
}

/// A compact "what is this panel / how to use it" banner shown at the
/// top of each feature page (and per-tool in the Console). Keeps the
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
