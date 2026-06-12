// version.json is the SPA's deployment-config channel: the serving
// layer already injects `api_url` (base URL discovery); a dev stack
// may also inject `dev_token` so the explorer needs ZERO manual
// configuration. The seed must never overwrite a bearer the user
// pasted themselves.

import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:triton_explorer/providers/api_provider.dart';

void main() {
  test('seeds the bearer from version.json when nothing is persisted',
      () async {
    SharedPreferences.setMockInitialValues({});
    final notifier = await TokenNotifier.load();
    await seedDevToken(notifier, {'dev_token': 'dev-token'});
    expect(notifier.state, 'dev-token');
  });

  test('never overwrites a user-pasted token', () async {
    SharedPreferences.setMockInitialValues(
        {'triton.bearer': 'my-real-jwt'});
    final notifier = await TokenNotifier.load();
    await seedDevToken(notifier, {'dev_token': 'dev-token'});
    expect(notifier.state, 'my-real-jwt');
  });

  test('no dev_token in version.json → no seed', () async {
    SharedPreferences.setMockInitialValues({});
    final notifier = await TokenNotifier.load();
    await seedDevToken(notifier, {'app_name': 'triton_explorer'});
    expect(notifier.state, isNull);
    await seedDevToken(notifier, null);
    expect(notifier.state, isNull);
  });
}
