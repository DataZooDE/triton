# MCP OAuth discovery with Entra: one path per customer

Status: implemented (per-customer protected resource metadata, challenge, tenant binding), 2026-10-31.
Authorization-server facade: specified as a contingency, not built.
Sources were checked on 2026-10-31. Client behaviour changes often, so re-check them before relying on them.

## Conclusion

Claude and ChatGPT can sign a user in against the customer's own Entra directory **without** a Triton authorization-server facade.
This holds when the Entra API app registration lists every customer endpoint URL, `https://<public origin>/mcp/<slug>`, as an Application ID URI and issues v2.0 access tokens.

Three findings support this:

1. **Both clients discover the authorization server through RFC 9728.** Each one reads the `resource_metadata` pointer in the 401 challenge, or falls back to the well-known URI with the path inserted. Claude uses only the first `authorization_servers` entry. Both then fetch the issuer's discovery document, and Entra serves only `<issuer>/.well-known/openid-configuration`.
2. **Both clients always send the RFC 8707 `resource` parameter.** It carries the MCP endpoint URL, path included, and goes on both `/authorize` and `/token`. Neither client lets you switch it off.
3. **Entra v2 accepts `resource` alongside `scope` if `resource` exactly equals an Application ID URI of the app that owns the scope.** Otherwise the request fails with `AADSTS9010010` (`invalid_target`). The v1-only error `AADSTS901002` ("resource parameter isn't supported") does not occur on the v2 endpoints in the reports reviewed here. Because a mismatch is rejected rather than ignored, the identifier URIs have to be registered.

The facade below becomes necessary only if one of the conditions in "When the conclusion changes" applies.

## What the clients do

| Step | MCP spec 2025-11-25 | Claude (claude.ai, Desktop, mobile, Cowork) | ChatGPT (developer mode / apps) |
|---|---|---|---|
| Start of the flow | The client must parse `WWW-Authenticate` on a 401 | A 401 is required; a challenge on a 200 is ignored | 401 with `WWW-Authenticate` |
| Protected resource metadata (PRM) location | `resource_metadata` from the challenge; else `/.well-known/oauth-protected-resource/<path>`, then the root | The same order; the PRM `resource` must equal the URL the user entered, path included | `/.well-known/oauth-protected-resource`, or the challenge URL |
| `authorization_servers` | The client chooses (RFC 9728 §7.6) | Only the first entry is used | Each entry is tried |
| AS metadata | RFC 8414 path insertion → OIDC path insertion → OIDC path appending | `oauth-authorization-server` first, then `openid-configuration`; the docs name Entra as `openid-configuration` only | `oauth-authorization-server` and `openid-configuration` |
| Client registration | Pre-registered → CIMD → DCR | "Use your own OAuth client" (client id; secret optional); CIMD and DCR are the alternatives | Static credentials are used when given; else CIMD, then DCR |
| Redirect URI | — | `https://claude.ai/api/mcp/auth_callback` (Claude Code: loopback on any port) | `https://chatgpt.com/connector_platform_oauth_redirect`, or `https://chatgpt.com/connector/oauth/{callback_id}`; copy the exact one the management page shows |
| `resource` (RFC 8707) | Must be sent on both authorize and token | Sent on both: the canonical MCP URL with lower-case scheme and host, no trailing slash | Sent on both: the server's canonical HTTPS identifier |
| Scopes | Challenge `scope`, else `scopes_supported` | The same; `offline_access` is added when the AS lists it (Entra does) | `scopes_supported` is requested by default |
| PKCE | S256; refuse if `code_challenge_methods_supported` is absent | Always S256 | PKCE |

Entra issuer discovery, probed live on 2026-10-31:

- `https://login.microsoftonline.com/<tid>/v2.0/.well-known/openid-configuration` returns 200.
- The RFC 8414 path-insertion form, the OIDC path-insertion form and `<issuer>/.well-known/oauth-authorization-server` all return 404.
- The document has no `code_challenge_methods_supported`, no `registration_endpoint` and no `client_id_metadata_document_supported`. Its `token_endpoint_auth_methods_supported` lacks `none`.

Consequences:

- **Neither CIMD nor DCR is possible against Entra.** A pre-registered client is the only option, which matches author decision 2 (one pre-registered client per customer).
- **A strictly conforming client would refuse the PKCE check.** Claude's troubleshooting page lists Entra as a supported provider, and Entra-backed connectors work in both products according to the issue reports below. Neither client is known to enforce the check today, but a client update could start enforcing it. See "When the conclusion changes".

## What Entra accepts

Microsoft's MCP guide (ms.date 2026-09-01) describes the flow:

- The client sends `resource=<MCP server URL>`.
- Entra compares that value with the Application ID URIs (`identifierUris`) of the app registration and issues a token when one matches exactly.
- This needs `api.requestedAccessTokenVersion = 2`, which must be set before an HTTPS identifier URI can be added.

`AADSTS9010010` has three causes:

- an exact-match difference, most often a trailing slash (an identifier URI cannot end in `/`);
- a v1 app;
- a scope that belongs to a different app registration than the one the `resource` names.

Claude's troubleshooting page gives the same fix and the same v2 warning. In v2 tokens:

- `aud` is the API app's client id (a GUID), whichever identifier URI was used;
- `iss` is `https://login.microsoftonline.com/<tid>/v2.0`, which is what the Entra verifier pins;
- `upn` is absent by default. Triton already falls back to `preferred_username`.

Limits for the API app (`AzureADMultipleOrgs`):

- identifier URIs are globally unique;
- each one is at most 255 characters;
- there is no per-property count limit, but the manifest has an aggregate limit of 1,200 entries across its collections;
- an `https://` identifier URI has to sit on a domain verified in the publisher tenant, or the app has to be on v2 tokens, which exempts it from the default identifier-URI policy.

Verify the public Triton domain in DataZoo's tenant anyway. The policy page says a verified domain "may still be required" for `https://` URIs even when the policy is off.

Field reports agree with this picture:

- The Power BI MCP server (Microsoft's own resource) failed with `AADSTS9010010` in Claude, ChatGPT, VS Code and Copilot CLI while its `resource` did not match. Microsoft fixed it server-side in July 2026.
- A custom MCP gateway fixed the same error on 2026-10-22 by adding the exact `resource` URL to `identifierUris`.
- First-party servers on domains the customer does not own, such as Business Central, cannot apply this fix.

## Consequence for Triton

Implemented:

- **One MCP endpoint per customer.** `TRITON_MCP_TENANTS=<slug>=<data tenant>:<tid>[,…]` serves `/mcp/<slug>`.
- **The metadata document.** `/.well-known/oauth-protected-resource/mcp/<slug>` serves it, and so does the suffix form `/mcp/<slug>/.well-known/oauth-protected-resource`:

  ```json
  {"resource": "<canonical TRITON_MCP_PUBLIC_ORIGIN>/mcp/<slug>",
   "authorization_servers": ["https://login.microsoftonline.com/<tid>/v2.0"],
   "scopes_supported": ["<TRITON_ENTRA_SCOPES or api://<entra api>/access_as_user>"],
   "bearer_methods_supported": ["header"]}
  ```

- **The canonical origin.** `TRITON_MCP_PUBLIC_ORIGIN` is reduced to its ASCII origin serialisation: lower-case scheme and host, no default port, no trailing slash. So `HTTPS://Agents.Example.com:443/` advertises `https://agents.example.com/mcp/<slug>`, which is what Claude compares with and what the Application ID URI has to equal.
- **The challenge.** A 401 on `/mcp/<slug>` carries `Bearer realm="triton", resource_metadata="…", scope="…"`, plus `error="invalid_token"` when a credential was sent. The header values are built once at boot, so a 401 can never go out without them.
- **The tenant binding.** A token at `/mcp/<slug>` must be an Entra token verified against exactly `<authority>/<tid>/v2.0` of that slug, and it must map to the slug's data tenant. Any other valid token is refused with 401 `invalid_token`: one from another customer's directory, from the deployment's own issuer, from Google, or with an `iss` that is not the canonical lower-case form.
- **The delegated scope (every face).** The rule below applies to every Entra token on every Triton face, not only customer paths: REST tools, plain `/mcp`, and A2A 0.3/1.0. There is no app-only access (lead decision 2026-10-31). The token's `scp` must contain the last path segment of an advertised scope (`access_as_user` for `api://<client id>/access_as_user`). An app-only token from the customer's tenant (client credentials: `roles`, no `scp`) has no user behind it. It is refused, and so is a user token for another permission. The answer is 403 with `error="insufficient_scope"`, the scope and `resource_metadata` (RFC 6750 §3.1), and nothing is dispatched.
- **Boot checks.** Triton refuses to boot, naming the variable, when any of these holds:
  - a route's tid is not in `TRITON_ENTRA_ALLOWED_TENANTS`, or maps to a different data tenant;
  - a slug is listed twice, or is not lower-case letters, digits and `-`;
  - `TRITON_ENTRA_AUDIENCE` is unset;
  - `TRITON_MCP_PUBLIC_ORIGIN` is missing, has a path or carries userinfo;
  - a scope, the default included, is not an RFC 6749 `scope-token`, which rules out a space, `"` and `\`;
  - `TRITON_OIDC_ISSUER` is under the Entra authority. A deployment issuer that is itself an Entra tenant would also produce `entra` principals, from the token's own `tenant` claim.

  All of these checks run before any port is bound.
- **`/mcp` is unchanged.** It is the single-issuer mode, and its challenge carries no metadata.

Deployment checklist (Entra, DataZoo's multi-tenant API app):

1. Set `api.requestedAccessTokenVersion = 2` on the API app, then set `TRITON_ENTRA_AUDIENCE` to the API app's **client id**, because v2 `aud` is the GUID.
2. Add `https://<public origin>/mcp/<slug>` to `identifierUris` for every customer: exact, lower-case host, no trailing slash. Verify the public domain in DataZoo's tenant.
3. Expose the delegated scope (`access_as_user`) and leave `TRITON_ENTRA_SCOPES` unset; the default is `api://<client id>/access_as_user`. That scope belongs to the same app as the `resource`, so the third cause of `AADSTS9010010` does not apply. If a client still reports the error, set `TRITON_ENTRA_SCOPES=https://<origin>/mcp/<slug>/access_as_user`. Today that value is per deployment, not per slug; making it per slug would be a small change.
4. In each customer tenant, an admin consents to the API, and to the pre-registered client if it is a separate app. The client's redirect URIs are `https://claude.ai/api/mcp/auth_callback` and the ChatGPT redirect shown in its connector page. If the client is confidential, the secret is entered when the connector is created (Claude: "Use your own OAuth client"; a blank secret on a confidential client fails with `AADSTS7000218`).
5. Set `TRITON_MCP_PUBLIC_ORIGIN` to the origin users enter. Triton canonicalises it, so enter the same canonical URL in the connector and in `identifierUris`. Do not redirect: Claude drops `Authorization` on a cross-host redirect.
6. Set **Assignment required** ("User assignment required?" on the API's enterprise application / service principal) in each customer tenant, and assign the users or groups who may use the connector. Entra then refuses to issue a token for anyone else, before Triton sees a request. Admit only delegated clients. Triton refuses app-only tokens on these paths, so do not grant application permissions (app roles) to clients that are meant to call the MCP endpoint.

Accepted risks:

- **Slug enumeration.** The metadata documents are unauthenticated, as RFC 9728 discovery requires. A GET answers 200 or 404 per slug, so anyone can test whether a slug is a customer, and the 200 reveals that customer's Entra tenant id. A tenant id is not a secret: Entra's own discovery documents publish it. If the customer list itself is confidential, use non-guessable slugs; the slug only has to match what the connector URL contains.

## When the conclusion changes

A facade is needed if any of these becomes true:

- A client starts refusing Entra because `code_challenge_methods_supported` is missing, as MCP 2025-11-25 requires.
- The customer cannot or will not keep the identifier URIs on the API app, for example because of a v1-only app, a policy exemption that is refused, or the 1,200-entry manifest limit at several hundred customers.
- Entra changes its `resource` validation again, as it did in March 2026.

## Contingency: minimal authorization-server facade (specified, not built)

The facade is a stateless proxy that turns Entra into a conforming MCP authorization server for each customer path. It issues no tokens of its own. Entra stays the only issuer, so the resource server, JWKS and tenant binding stay as built.

- **Protected resource metadata.** `authorization_servers` becomes `["<origin>/oauth/<slug>"]` instead of the Entra issuer.
- **AS metadata.** Served at `/.well-known/oauth-authorization-server/oauth/<slug>`, and also at `/oauth/<slug>/.well-known/openid-configuration`. It contains:
  - `issuer`: `<origin>/oauth/<slug>`;
  - `authorization_endpoint`: `<origin>/oauth/<slug>/authorize`;
  - `token_endpoint`: `<origin>/oauth/<slug>/token`;
  - `jwks_uri`: Entra's;
  - `response_types_supported: ["code"]`, `grant_types_supported: ["authorization_code","refresh_token"]`;
  - `code_challenge_methods_supported: ["S256"]`;
  - `scopes_supported`: the API scope plus `offline_access`;
  - `token_endpoint_auth_methods_supported: ["client_secret_post","client_secret_basic","none"]`.

  The `iss` in the tokens stays Entra's. That is permitted because the client never validates the access token, and Triton validates it against Entra.
- **`GET /oauth/<slug>/authorize`.** It does the following:
  - removes `resource`;
  - checks that `client_id` is the slug's configured pre-registered client and that `redirect_uri` is on a fixed allow-list (the Claude and ChatGPT redirects);
  - rewrites `scope` to the API scope (plus `offline_access`);
  - answers with a 302 to `https://login.microsoftonline.com/<tid>/oauth2/v2.0/authorize` with everything else unchanged (`state`, `code_challenge`, `code_challenge_method`, `redirect_uri`).

  Entra redirects straight back to the client, so the facade never sees the code.
- **`POST /oauth/<slug>/token`** (form-urlencoded). It removes `resource`, forwards the body and the `Authorization` header to `https://login.microsoftonline.com/<tid>/oauth2/v2.0/token` and returns the response unchanged. That covers `authorization_code` with `code_verifier`, and `refresh_token`. The 10-second response budget set by Claude applies.
- **No state or storage, and no secrets other than optional ones.** If the client should be confidential without entering a secret in Claude and ChatGPT, the facade could add the client secret itself. That adds one secret per deployment and is a separate decision.
- **Tests (when built).** These use `TritonProcess` with a fake Entra (`TestIssuer` plus a tiny `/authorize` and `/token` that reject any `resource`, as Entra's v1 behaviour does). They cover four things: the metadata documents, the authorize redirect without `resource`, a token exchange through the proxy, and a refused foreign `redirect_uri`.

## Sources

- MCP authorization specification 2025-11-25: https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization
- Claude, Authentication for connectors (discovery, `resource`, callback URLs, client options): https://claude.com/docs/connectors/building/authentication.md
- Claude, Troubleshoot your connector, "Microsoft Entra ID rejects the resource value": https://claude.com/docs/connectors/building/troubleshooting.md
- Claude, Add a connector that isn't in the directory ("Use your own OAuth client"): https://claude.com/docs/connectors/custom
- OpenAI Apps SDK, Authentication (PRM, `resource` on authorize and token, CIMD/DCR/predefined client, redirect URIs): https://developers.openai.com/apps-sdk/build/auth
- Microsoft, Secure an MCP server with Microsoft Entra ID (ms.date 2026-09-01): https://learn.microsoft.com/en-us/entra/agent-id/secure-mcp-server-with-entra-id
- Microsoft, Restrictions on identifier URIs: https://learn.microsoft.com/en-us/entra/identity-platform/identifier-uri-restrictions
- Microsoft, Validation differences by supported account types (identifier URI limits): https://learn.microsoft.com/en-us/entra/identity-platform/supported-accounts-validation
- Field report, Power BI MCP `AADSTS9010010` across Claude, ChatGPT and VS Code (March–October 2026): https://github.com/microsoft/powerbi-modeling-mcp/issues/68
- Field report, Azure DevOps MCP `AADSTS9010010` (June 2026): https://github.com/microsoft/azure-devops-mcp/issues/1348
- Field report, Claude custom connector against first-party Entra MCP servers (open, July–October 2026): https://github.com/anthropics/claude-ai-mcp/issues/632
- Secondary summary of AADSTS9010010 versus AADSTS901002: https://zuplo.com/learn/mcp/errors/resource-indicator-missing-or-unknown
- Live probe of `https://login.microsoftonline.com/<tid>/v2.0/.well-known/openid-configuration` and the RFC 8414 forms, 2026-10-31 (see "What the clients do").
