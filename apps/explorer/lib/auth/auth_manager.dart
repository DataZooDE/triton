import 'package:flutter/foundation.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:oidc/oidc.dart';
import 'package:oidc_default_store/oidc_default_store.dart';

import '../api/runtime_config.dart';
import '../providers/runtime_provider.dart';

/// Live OIDC PKCE state. The SPA boots into `discovering`, transitions
/// to `loggedOut` once `/v1/runtime` is read, and to `loggedIn` after
/// the IdP redirects back with an authorization code that the
/// `OidcUserManager` exchanges for tokens.
enum AuthStatus { discovering, loggedOut, loggedIn, error }

class AuthState {
  AuthState(this.status, {this.user, this.error});
  final AuthStatus status;
  final OidcUser? user;
  final String? error;

  String? get accessToken => user?.token.accessToken;
}

/// Builds an OIDC manager from the `/v1/runtime` discovery payload.
/// Returns `null` when the operator hasn't enabled the explorer in
/// this env (in that case the SPA stays in `loggedOut` and the
/// settings page lets the user paste a token as the dev escape hatch).
OidcUserManager? _buildManager(RuntimeConfig cfg) {
  if (!cfg.explorerEnabled) return null;
  // Web only: the redirect lands on a same-origin HTML page (see
  // `web/redirect.html`) that broadcasts the auth code back to the
  // Flutter window. Native targets would need platform-specific
  // redirect URIs — not in scope for V1.
  if (!kIsWeb) return null;
  final origin = Uri.base.replace(path: '', query: null, fragment: null);
  return OidcUserManager.lazy(
    discoveryDocumentUri:
        OidcUtils.getOpenIdConfigWellKnownUri(Uri.parse(cfg.oidcIssuer!)),
    clientCredentials:
        OidcClientAuthentication.none(clientId: cfg.oidcClientId!),
    store: OidcDefaultStore(),
    settings: OidcUserManagerSettings(
      redirectUri: origin.replace(path: '/redirect.html'),
      postLogoutRedirectUri: origin.replace(path: '/redirect.html'),
      scope: const ['openid', 'profile', 'email'],
      // The audience triton verifies against is per-env (e.g.
      // `agents-nonprod`); the IdP usually issues it automatically
      // when this client is configured. We pass it as an extra
      // parameter so providers that require it (Auth0, Keycloak with
      // an audience mapper) get the right `aud` claim.
      extraTokenParameters: {'audience': cfg.oidcAudience!},
    ),
  );
}

class AuthNotifier extends StateNotifier<AuthState> {
  AuthNotifier(this._manager)
      : super(AuthState(_manager == null
            ? AuthStatus.loggedOut
            : AuthStatus.discovering)) {
    _init();
  }

  final OidcUserManager? _manager;

  Future<void> _init() async {
    final m = _manager;
    if (m == null) return;
    try {
      await m.init();
      m.userChanges().listen((user) {
        if (!mounted) return;
        state = AuthState(
          user == null ? AuthStatus.loggedOut : AuthStatus.loggedIn,
          user: user,
        );
      });
      // userChanges() emits the cached user (or null) immediately;
      // the listener above will set the right state.
      state = AuthState(
        m.currentUser == null ? AuthStatus.loggedOut : AuthStatus.loggedIn,
        user: m.currentUser,
      );
    } catch (e) {
      if (!mounted) return;
      state = AuthState(AuthStatus.error, error: e.toString());
    }
  }

  Future<void> login() async {
    final m = _manager;
    if (m == null) return;
    try {
      await m.loginAuthorizationCodeFlow();
    } catch (e) {
      state = AuthState(AuthStatus.error, error: e.toString());
    }
  }

  Future<void> logout() async {
    final m = _manager;
    if (m == null) return;
    try {
      await m.logout();
    } catch (e) {
      state = AuthState(AuthStatus.error, error: e.toString());
    }
  }
}

/// Provider for the live auth state. Wired against `/v1/runtime` so
/// the IdP config the SPA uses always agrees with what Triton's
/// verifier will accept.
final authProvider =
    StateNotifierProvider<AuthNotifier, AuthState>((ref) {
  final cfg = ref.watch(runtimeConfigProvider).valueOrNull;
  final manager = cfg == null ? null : _buildManager(cfg);
  return AuthNotifier(manager);
});
