// The MCP-Apps host bridge, exercised on a REAL device against the REAL
// embedder the running target selects (#201).
//
// This is the non-web half of the `ui://` story. On web the resource lives in
// a sandboxed `<iframe>` and talks to the Explorer through `postMessage`; on
// Android/iOS it must do the same, or every interactive upstream (a Peacock
// report, an Escurel document) degrades to a monospace dump of its own source.
//
// Nothing here is mocked away: a real WebView loads a real host document,
// which loads the guest in a real iframe, and the two halves of the bridge are
// asserted in both directions.
//
// Run it:
//   export JAVA_HOME=/usr/lib/jvm/java-17-openjdk   # JDK 26 breaks Gradle
//   cd apps/explorer
//   flutter test integration_test/ui_resource_embed_test.dart -d emulator-5554
//
// On web the same contract is covered by the browser; this file targets the
// `dart:io` arm and is skipped on desktop (see the guard in `main`).

import 'dart:io' show Platform;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

import 'package:triton_explorer/widgets/a2ui/html_embed.dart';

/// A stand-in for what `resources/read` returns from a Peacock report: one
/// self-contained HTML document that fetches its own data over the bridge.
///
/// It deliberately contains double quotes, single quotes, newlines and — most
/// importantly — a `</script>` sequence of its own, because those are exactly
/// what a naive "substitute the HTML into the host page" embedder mangles.
///
/// The round trip:
///   1. on load, post `mcp:callServerTool` asking for `render_report`;
///   2. on `mcp:callServerTool:result`, read `structuredContent.rows`;
///   3. post `mcp:prompt` carrying a summary derived FROM THE RESULT.
///
/// Step 3 is what makes this assertable from Dart: Flutter cannot see inside
/// the webview's DOM, but it can observe that the guest received an intact
/// payload and said so back through the bridge.
const String _guestResourceHtml = r'''
<!doctype html>
<meta charset="utf-8">
<title>Q3 "renewal" report</title>
<style>
  body { font: 14px/1.4 system-ui, sans-serif; margin: 12px; }
  .row { padding: 4px 0; border-bottom: 1px solid #ddd; }
</style>
<body>
  <h1>Hoffmann Automotive — Q3 "renewal"</h1>
  <div id="rows"></div>
<script>
  var REQ = 'req-1';

  window.addEventListener('message', function (e) {
    var d = e.data;
    if (!d || d.type !== 'mcp:callServerTool:result' || d.reqId !== REQ) return;

    var rows = (d.result && d.result.structuredContent && d.result.structuredContent.rows) || [];
    rows.forEach(function (r) {
      var el = document.createElement('div');
      el.className = 'row';
      el.textContent = r.customer + ' — ' + r.amount;
      document.getElementById('rows').appendChild(el);
    });

    // Report back through the bridge. Includes a value from INSIDE the
    // result, not just a count, so a well-formed but empty reply still fails.
    parent.postMessage({
      type: 'mcp:prompt',
      text: 'bridge-ok rows=' + rows.length + ' first=' + (rows[0] ? rows[0].customer : 'none')
    }, '*');
  });

  parent.postMessage({
    type: 'mcp:callServerTool',
    reqId: REQ,
    name: 'render_report',
    arguments: { report: 'q3-renewal', filter: "status = 'active'" }
  }, '*');
</script>
</body>
''';

/// Harness: surfaces what crossed the bridge in the Flutter widget tree, keyed
/// so the assertions survive re-wording.
class _BridgeHarness extends StatefulWidget {
  const _BridgeHarness();

  @override
  State<_BridgeHarness> createState() => _BridgeHarnessState();
}

class _BridgeHarnessState extends State<_BridgeHarness> {
  String _toolSeen = 'none';
  String _fromGuest = 'waiting';

  /// Stands in for `UiResourceView._callServerTool`: dispatch through Triton
  /// and hand back the upstream's own MCP result, which is the shape the
  /// embedded runtime consumes (`data.structuredContent.rows`).
  Future<Object?> _callServerTool(String name, Object? args) async {
    setState(() => _toolSeen = '$name|$args');
    return {
      'structuredContent': {
        'rows': [
          {'customer': 'Hoffmann Automotive', 'amount': '€ 240,000'},
          {'customer': 'Alpina Biotech', 'amount': '€ 88,500'},
        ],
      },
    };
  }

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      home: Scaffold(
        body: Column(
          children: [
            Text('tool: $_toolSeen', key: const Key('tool')),
            Text('guest: $_fromGuest', key: const Key('guest')),
            Expanded(
              child: embedHtml(
                _guestResourceHtml,
                viewId: 'uiResource-0',
                height: 400,
                onCallServerTool: _callServerTool,
                onPrompt: (text) => setState(() => _fromGuest = text),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('a ui:// resource round-trips callServerTool on a real device',
      (tester) async {
    await tester.pumpWidget(const _BridgeHarness());

    // The webview loads, runs the guest's script and completes the round trip
    // on the PLATFORM thread. `tester.pump(duration)` only advances Flutter's
    // fake clock, so that work never runs and this would poll a frozen app —
    // a failure mode that looks exactly like a broken embedder. `runAsync`
    // leaves the fake-async zone so the platform side actually executes.
    //
    // Poll rather than sleep a fixed time: a fixed delay is either flaky or
    // slow, and on a cold emulator it is both.
    String guestText = '';
    for (var i = 0; i < 60; i++) {
      await tester.runAsync(
        () => Future<void>.delayed(const Duration(milliseconds: 500)),
      );
      await tester.pump();
      guestText = tester.widget<Text>(find.byKey(const Key('guest'))).data!;
      if (guestText.contains('bridge-ok')) break;
    }
    final toolText = tester.widget<Text>(find.byKey(const Key('tool'))).data!;

    // guest → host
    expect(toolText, contains('render_report'),
        reason: 'the guest\'s callServerTool never reached the host: the '
            'embedder selected for this target has no bridge');
    expect(toolText, contains('q3-renewal'),
        reason: 'arguments were dropped between guest and host');
    expect(toolText, contains("status = 'active'"),
        reason: 'a quoted argument was mangled in transit');

    // host → guest
    expect(guestText, contains('rows=2'),
        reason: 'the result payload did not survive the host→guest hop');
    expect(guestText, contains('first=Hoffmann Automotive'),
        reason: 'the guest could not read values out of the result payload');

    // …and the resource is RENDERED, not dumped. The stub embedder satisfies
    // every "widget exists" check while showing the user raw HTML source, so
    // assert its tell-tale is absent. The positive assertions above are this
    // one's control: they can only pass if a live bridge actually ran.
    expect(find.textContaining('<!doctype html>'), findsNothing,
        reason: 'the resource is being shown as HTML SOURCE — this is #201');
  },
      skip: !(Platform.isAndroid || Platform.isIOS),
      timeout: const Timeout(Duration(minutes: 2)));
}
