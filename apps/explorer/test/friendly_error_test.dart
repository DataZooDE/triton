// Errors shown to the operator must say what to DO, not dump a
// DioException. The cases that actually happen in dev/nonprod:
//
//   401 → no/wrong bearer: point at Settings (dev-token in nonprod);
//   403 → token lacks access;
//   404 → base URL is wrong (trailing slash, stale port, non-REST
//          root): point at Settings / the base URL;
//   connection errors → Triton down or CORS not allow-listing this
//          origin: name both.

import 'package:dio/dio.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/friendly_error.dart';

DioException _http(int code) => DioException(
      requestOptions: RequestOptions(path: 'http://127.0.0.1:8081/v1/tools'),
      response: Response(
        requestOptions:
            RequestOptions(path: 'http://127.0.0.1:8081/v1/tools'),
        statusCode: code,
      ),
      type: DioExceptionType.badResponse,
    );

void main() {
  test('401 explains the missing bearer and names Settings + dev-token',
      () {
    final msg = friendlyApiError('Could not load /v1/tools', _http(401));
    expect(msg, contains('Could not load /v1/tools'));
    expect(msg, contains('401'));
    expect(msg, contains('Settings'));
    expect(msg, contains('dev-token'));
    expect(msg, isNot(contains('DioException')));
  });

  test('403 explains the token lacks access', () {
    final msg = friendlyApiError('Could not load /v1/audit', _http(403));
    expect(msg, contains('403'));
    expect(msg.toLowerCase(), contains('token'));
  });

  test('404 points at the base URL', () {
    final msg = friendlyApiError('Could not reach Triton', _http(404));
    expect(msg, contains('404'));
    expect(msg.toLowerCase(), contains('base url'));
  });

  test('connection error names Triton-down and CORS', () {
    final e = DioException(
      requestOptions: RequestOptions(path: 'http://127.0.0.1:8081/v1/tools'),
      type: DioExceptionType.connectionError,
    );
    final msg = friendlyApiError('Could not load /v1/tools', e);
    expect(msg.toLowerCase(), contains('running'));
    expect(msg, contains('CORS'));
  });

  test('non-Dio errors fall through verbatim', () {
    final msg = friendlyApiError('Could not parse', StateError('boom'));
    expect(msg, contains('Could not parse'));
    expect(msg, contains('boom'));
  });
}
