import 'package:dio/dio.dart';

/// Turn an API failure into a message that says what to DO, not a
/// DioException dump. `what` names the attempted action ("Could not
/// load /v1/tools"); the suffix names the most likely fix for the
/// failure class actually seen in dev/nonprod.
String friendlyApiError(String what, Object error) {
  if (error is DioException) {
    final code = error.response?.statusCode;
    switch (code) {
      case 401:
        return '$what: Triton answered 401 (unauthenticated). '
            'Paste a bearer token in Settings — on a nonprod Triton '
            'built with the dev-token feature the token is literally '
            '"dev-token"; with OIDC configured, sign in instead.';
      case 403:
        return '$what: Triton answered 403 (forbidden) — the bearer '
            'token was accepted but lacks access to this surface. '
            'Check the token\'s audience/role in Settings.';
      case 404:
        return '$what: Triton answered 404. The base URL is probably '
            'wrong — it must be the REST adapter root (e.g. '
            'http://127.0.0.1:8081 in local dev, no trailing path). '
            'Fix it in Settings.';
      case null:
        break;
      default:
        return '$what: Triton answered HTTP $code'
            '${error.message != null ? ' (${error.message})' : ''}.';
    }
    switch (error.type) {
      case DioExceptionType.connectionError:
      case DioExceptionType.connectionTimeout:
      case DioExceptionType.receiveTimeout:
      case DioExceptionType.sendTimeout:
        return '$what: no response from ${error.requestOptions.uri.host}:'
            '${error.requestOptions.uri.port}. Is Triton running, and is '
            'this origin allow-listed in TRITON_CORS_ALLOWED_ORIGINS? '
            '(A CORS-blocked request surfaces here too — check the '
            'browser console.)';
      default:
        break;
    }
  }
  return '$what: $error';
}
