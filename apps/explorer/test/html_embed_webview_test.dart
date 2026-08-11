// Unit cover for the two parts of the non-web embedder (#201) that a device
// test can only observe indirectly: how the guest is encoded into the host
// document, and which targets get a webview at all.
//
// The end-to-end proof lives in
// `integration_test/ui_resource_embed_test.dart`, which runs the real bridge
// on a real device. These tests exist because both failures below are silent —
// a blank box, or a crash on a platform nobody tested — and neither says why.

import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
// Imported directly, not through `html_embed.dart`: this file tests the
// `dart:io` ARM specifically. (Going through the conditional export would also
// make `embedHtml` ambiguous — the analyzer resolves that export to its
// default arm while the VM resolves it to this one.)
import 'package:triton_explorer/widgets/a2ui/html_embed_webview.dart';

/// A guest shaped like a real `ui://` resource: a whole HTML document, which
/// therefore closes its own `<script>` and carries an HTML comment.
const String _guest = '''
<!doctype html>
<!-- a report's own comment -->
<body>
<script>
  var q = "a \\" quote";
  parent.postMessage({type:'mcp:prompt', text:'hi'}, '*');
</script>
</body>
''';

/// Pull the JS string literal the host document assigns to `guest.srcdoc`.
String _srcdocLiteral(String hostDocument) {
  final m = RegExp(r'guest\.srcdoc = (.*);').firstMatch(hostDocument);
  expect(m, isNotNull,
      reason: 'the host document no longer feeds the guest through srcdoc');
  return m!.group(1)!;
}

void main() {
  group('hostDocumentFor', () {
    test('the guest survives encoding byte for byte', () {
      // Positive control for the assertion below: it is easy to stop a
      // `</script>` escaping from leaking by mangling the guest, and this
      // catches that. Undo the two script-context escapes, then read the JS
      // string literal back as JSON — it must be the original document.
      final decoded = jsonDecode(_srcdocLiteral(hostDocumentFor(_guest))
          .replaceAll(r'<\/', '</')
          .replaceAll(r'<\!--', '<!--'));
      expect(decoded, _guest,
          reason: 'the guest is not delivered unchanged — the whole point of '
              'the webview embedder is that a report is byte-identical on web '
              'and on mobile');
    });

    test('the guest cannot terminate the host script block', () {
      final doc = hostDocumentFor(_guest);

      // `jsonEncode` alone is not enough: the HTML parser ends the HOST's
      // script at the byte sequence `</script>` even inside a JS string
      // literal, so the host script dies with a SyntaxError, no bridge is
      // installed, and the embed renders as a silent blank box.
      expect('</script>'.allMatches(doc).length, 1,
          reason: 'the guest\'s own </script> leaked through: the host script '
              'block is terminated early and the bridge is never installed');

      // Same for a comment opener, which flips legacy script parsing into a
      // comment state.
      expect(doc.contains('<!--'), isFalse,
          reason: 'an unescaped <!-- opens an HTML comment inside the host '
              'script');

      // …and the escaped forms ARE present, so the two assertions above
      // cannot be satisfied by an embedder that simply dropped the guest.
      expect(doc, contains(r'<\/script>'));
      expect(doc, contains(r'<\!--'));
    });
  });

  group('webViewAvailableOn', () {
    test('only where webview_flutter ships an implementation', () {
      // The conditional import cannot tell these apart — it only knows the
      // target has `dart:io`. Getting this wrong throws on a missing platform
      // instance, on desktop and in `flutter test` alike.
      expect(webViewAvailableOn('android'), isTrue);
      expect(webViewAvailableOn('ios'), isTrue);
      expect(webViewAvailableOn('linux'), isFalse);
      expect(webViewAvailableOn('macos'), isFalse);
      expect(webViewAvailableOn('windows'), isFalse);
    });
  });

  testWidgets('on a webview-less target the resource degrades to its source',
      (tester) async {
    // This test runs on the Dart VM, which the `dart:io` arm now claims. If
    // that arm built a WebViewController unconditionally, this would throw on
    // the missing platform implementation — which is exactly what desktop
    // users would get. Degrade instead, visibly.
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: embedHtml('<!doctype html><p>report</p>', viewId: 'v0'),
      ),
    ));
    expect(find.textContaining('<!doctype html>'), findsOneWidget);
  });
}
