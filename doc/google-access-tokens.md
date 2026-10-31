# Google access tokens: audience, user and hosted domain

Gemini Enterprise forwards an opaque Google OAuth **access token**. Triton
verifies it in `GoogleAccessTokenVerifier` (`crates/triton-identity`) and,
under `TRITON_GOOGLE_ACCESS_ALLOWED_HD`, admits only members of an allowed
Google Workspace domain (`hd`).

## Two calls, fail closed

1. **tokeninfo** (`https://oauth2.googleapis.com/tokeninfo?access_token=…`):
   the audience must be a configured client id, the email present and
   verified, the token unexpired.
2. **userinfo**, only when tokeninfo carries no `hd`:
   `GET https://openidconnect.googleapis.com/v1/userinfo` with
   `Authorization: Bearer <access token>`. The answer must name the same
   user (email equal, case-insensitive; `sub` equal when both carry it).
   Its `hd` then goes through the same allow-list as a tokeninfo `hd`. No
   `hd` means a consumer account: refused. Userinfo unreachable, non-2xx
   or undecodable: refused with the neutral message "google access token
   could not be verified"; the detail goes to the log only.

The userinfo answer is cached per SHA-256 of the token until the token
expires (bounded LRU, 1024 entries); the whole identity is re-introspected
at tokeninfo at most every 5 minutes, as before.

`TRITON_GOOGLE_USERINFO_URL` overrides the endpoint for tests and, like
`TRITON_GOOGLE_TOKENINFO_URL`, is refused outside `TRITON_ENV=local`.

## Why userinfo

Real-platform finding (Gemini Enterprise, 2026-10-31): a Workspace user's
access token was refused with "google access token has no hosted domain
(hd)". tokeninfo returns the claims of an **ID token** when given one, and
`hd` is documented as an ID-token claim. For an access token it answers (observed, not documented)
`azp`, `aud`, `sub`, `scope`, `exp`, `expires_in`, `email`,
`email_verified`, `access_type` and no `hd`.

## Sources (checked 2026-10-31)

- Google, *Authenticate with a backend server* —
  <https://developers.google.com/identity/sign-in/web/backend-auth>:
  tokeninfo returns "the JSON-formatted ID token claims"; `hd` "indicates
  the hosted domain of the user", and its absence "indicates that the
  account does not belong to a Google Workspace hosted domain". The page
  also calls tokeninfo unsuitable for production because it may be
  throttled; this verifier caches to keep the call rate low.
- Google, *OpenID Connect* —
  <https://developers.google.com/identity/openid-connect/openid-connect>:
  the userinfo endpoint is called "with an access token in the
  Authorization header"; the `openid` scope is required; `hd` is "the
  domain associated with the Google Workspace or Cloud organization of the
  user", present only for organisation members.
- Google discovery document —
  <https://accounts.google.com/.well-known/openid-configuration>:
  `"userinfo_endpoint": "https://openidconnect.googleapis.com/v1/userinfo"`.

## Caveats

- The discovery document's `claims_supported` does not list `hd`, and
  Google's pages define `hd` as an ID-token claim. That userinfo returns
  `hd` for a Workspace member is observed behaviour of the v1 endpoint,
  not a documented guarantee. If Google stops sending it, Workspace
  callers are refused again (fail closed), not admitted.
- Userinfo needs the `openid` scope on the access token. A Gemini
  Enterprise authorization configured without it gets 401 at userinfo,
  and Triton refuses the caller. Request `openid email` in the agent's
  OAuth authorization.
