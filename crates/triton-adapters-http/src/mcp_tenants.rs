//! One MCP endpoint per customer, each an RFC 9728 OAuth protected
//! resource whose authorization server is that customer's Entra directory.
//!
//! Claude and ChatGPT connectors find the authorization server only through
//! protected resource metadata: the `resource_metadata` pointer in the 401
//! challenge (RFC 9728 §5.1), else the well-known URI with the endpoint path
//! inserted (§3.1). Entra has no issuer a client can sign in against for
//! "some customer", so each customer gets a path, `/mcp/<slug>`, whose
//! metadata names `https://login.microsoftonline.com/<tid>/v2.0`.
//!
//! The path is also a tenant binding ([`McpRoute::admit`]): the token must
//! have been verified as an Entra token from exactly that issuer and map to
//! the slug's data tenant. Everything else about MCP is the shared adapter
//! in [`crate::mcp`]; this module only adds the routes, the metadata
//! document and the challenge. Research and the Entra consequences:
//! `doc/mcp-oauth-discovery.md`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use triton_core::{Principal, TritonError};

use crate::mcp::McpState;

/// The well-known suffix of RFC 9728 §3.
pub const WELL_KNOWN: &str = "/.well-known/oauth-protected-resource";

/// One customer's MCP endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRoute {
    /// The path segment: the endpoint is `/mcp/<slug>`.
    pub slug: String,
    /// The data tenant every principal on this path carries.
    pub data_tenant: String,
    /// The customer's Entra tenant id (lower-case GUID).
    pub tid: String,
    /// The Entra v2 issuer of that tenant, `<authority>/<tid>/v2.0`: the
    /// one `authorization_servers` entry and the only accepted `iss`.
    pub authorization_server: String,
    /// `<public origin>/mcp/<slug>`: the RFC 9728 `resource` and what an
    /// RFC 8707 client sends as `resource`. The origin is expected in its
    /// canonical ASCII serialisation (lower-case, no default port).
    pub resource: String,
    /// The scopes a client requests (`scopes_supported`, challenge `scope`).
    pub scopes: Vec<String>,
    /// What the token's `scp` must contain (any one): the last path segment
    /// of each scope, because Entra's `scp` carries `access_as_user`, not
    /// `api://<app>/access_as_user`.
    required_scopes: Vec<String>,
    /// Prebuilt `WWW-Authenticate` values, so a header can never fail to
    /// build at request time and silently go missing.
    challenge_bare: HeaderValue,
    challenge_invalid: HeaderValue,
    challenge_scope: HeaderValue,
}

/// RFC 6749 §3.3 `scope-token = 1*( %x21 / %x23-5B / %x5D-7E )`: no space,
/// `"` or `\`, so a scope can sit inside a quoted challenge parameter.
pub fn is_scope_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b))
}

impl McpRoute {
    /// Build a route; `origin` is the public `scheme://host[:port]`. Fails
    /// when there is no scope, a scope is not an RFC 6749 scope token, or a
    /// challenge would not be a valid header value.
    pub fn new(
        slug: &str,
        data_tenant: &str,
        tid: &str,
        authority: &str,
        origin: &str,
        scopes: Vec<String>,
    ) -> Result<Self, String> {
        if scopes.is_empty() {
            return Err("no scope to advertise and require".into());
        }
        if let Some(bad) = scopes.iter().find(|s| !is_scope_token(s)) {
            return Err(format!("`{bad}` is not an RFC 6749 scope token"));
        }
        let tid = tid.to_ascii_lowercase();
        let required_scopes = scopes
            .iter()
            .map(|s| s.rsplit('/').next().unwrap_or(s).to_string())
            .collect();
        let resource = format!("{}/mcp/{slug}", origin.trim_end_matches('/'));
        let metadata_url = format!("{}{WELL_KNOWN}/mcp/{slug}", origin.trim_end_matches('/'));
        let base = format!(
            r#"Bearer realm="triton", resource_metadata="{metadata_url}", scope="{}""#,
            scopes.join(" ")
        );
        let header = |v: String| {
            HeaderValue::from_str(&v).map_err(|e| format!("challenge `{v}` is not a header: {e}"))
        };
        Ok(Self {
            slug: slug.to_string(),
            data_tenant: data_tenant.to_string(),
            authorization_server: format!("{}/{tid}/v2.0", authority.trim_end_matches('/')),
            resource,
            tid,
            scopes,
            required_scopes,
            challenge_invalid: header(format!(r#"{base}, error="invalid_token""#))?,
            challenge_scope: header(format!(r#"{base}, error="insufficient_scope""#))?,
            challenge_bare: header(base)?,
        })
    }

    /// The local path of the endpoint.
    pub fn path(&self) -> String {
        format!("/mcp/{}", self.slug)
    }

    /// RFC 9728 §3.1: the metadata URL, well-known segment inserted between
    /// the origin and the endpoint path.
    pub fn metadata_url(&self) -> String {
        let path = self.path();
        let origin = self.resource.strip_suffix(&path).unwrap_or(&self.resource);
        format!("{origin}{WELL_KNOWN}{path}")
    }

    /// The RFC 9728 §2 document.
    pub fn metadata(&self) -> Value {
        json!({
            "resource": self.resource,
            "authorization_servers": [self.authorization_server],
            "scopes_supported": self.scopes,
            "bearer_methods_supported": ["header"],
        })
    }

    /// The tenant binding, in two steps.
    ///
    /// 1. Only an Entra token verified against this route's issuer and
    ///    mapped to this route's data tenant. A valid token from another
    ///    customer's directory, or from any other issuer, is `Auth` (401
    ///    `invalid_token`): it was never a credential for this resource.
    /// 2. The delegated scope: `scp` must hold one of the route's scopes.
    ///    An app-only token (client credentials: `roles`, no `scp`) has no
    ///    user behind it, and a user token for another permission is not
    ///    this one. That is `Forbidden` (403 `insufficient_scope`, RFC 6750
    ///    §3.1), so a client can step up instead of starting over.
    pub fn admit(&self, p: &Principal) -> Result<(), TritonError> {
        let from_here = p.idp.as_ref().is_some_and(|idp| {
            idp.name == "entra"
                && idp.tenant.as_deref() == Some(self.tid.as_str())
                && idp.issuer.as_deref() == Some(self.authorization_server.as_str())
        });
        if !from_here || p.tenant != self.data_tenant {
            return Err(TritonError::Auth(
                "the token is not from this endpoint's Entra directory".into(),
            ));
        }
        if !p.scopes.iter().any(|s| self.required_scopes.contains(s)) {
            return Err(TritonError::Forbidden(format!(
                "insufficient_scope: the token carries none of {}",
                self.scopes.join(" ")
            )));
        }
        Ok(())
    }
}

/// The routes of every customer path, at absolute paths so the same router
/// merges into the single-port router and the MCP listener alike:
/// `POST /mcp/<slug>`, the metadata at `/.well-known/oauth-protected-
/// resource/mcp/<slug>` and, for clients that append rather than insert,
/// `/mcp/<slug>/.well-known/oauth-protected-resource`.
pub fn router(state: McpState, routes: &[McpRoute]) -> Router {
    let mut out = Router::new();
    for route in routes {
        let route = Arc::new(route.clone());
        let doc = route.metadata();
        let metadata = get(move || {
            let doc = doc.clone();
            async move { Json(doc) }
        });
        let endpoint = post(crate::mcp::rpc).with_state(McpState {
            route: Some(route.clone()),
            ..state.clone()
        });
        out = out
            .route(&format!("{WELL_KNOWN}{}", route.path()), metadata.clone())
            .route(&format!("{}{WELL_KNOWN}", route.path()), metadata)
            .merge(
                Router::new()
                    .route(&route.path(), endpoint)
                    .layer(axum::middleware::from_fn_with_state(route, challenge)),
            );
    }
    out
}

/// RFC 6750 §3 plus RFC 9728 §5.1: every 401 on a customer path names the
/// route's metadata and scope; a 403 (only the scope check answers one on
/// this path) says `insufficient_scope`. The values are prebuilt, so the
/// header is always present.
async fn challenge(State(route): State<Arc<McpRoute>>, request: Request, next: Next) -> Response {
    let had_credentials = request.headers().contains_key(AUTHORIZATION);
    let mut response = next.run(request).await;
    let value = match response.status() {
        StatusCode::UNAUTHORIZED if had_credentials => &route.challenge_invalid,
        StatusCode::UNAUTHORIZED => &route.challenge_bare,
        StatusCode::FORBIDDEN => &route.challenge_scope,
        _ => return response,
    };
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, value.clone());
    response
}

#[cfg(test)]
mod tests {
    use super::McpRoute;

    #[test]
    fn metadata_url_inserts_the_well_known_segment_before_the_path() {
        let r = McpRoute::new(
            "acme",
            "acme",
            "AAAAAAAA-0000-0000-0000-000000000000",
            "https://login.microsoftonline.com/",
            "https://agents.example/",
            vec!["api://x/access_as_user".into()],
        )
        .unwrap();
        assert_eq!(r.resource, "https://agents.example/mcp/acme");
        assert_eq!(
            r.metadata_url(),
            "https://agents.example/.well-known/oauth-protected-resource/mcp/acme"
        );
        assert_eq!(
            r.authorization_server,
            "https://login.microsoftonline.com/aaaaaaaa-0000-0000-0000-000000000000/v2.0"
        );
        assert_eq!(r.required_scopes, vec!["access_as_user".to_string()]);
    }

    #[test]
    fn scopes_outside_the_rfc_6749_grammar_are_refused() {
        for bad in ["", "a b", "a\"b", "a\\b", "a\u{7f}", "a\nb"] {
            assert!(!super::is_scope_token(bad), "{bad:?}");
            assert!(
                McpRoute::new(
                    "s",
                    "t",
                    "x",
                    "https://a",
                    "https://o",
                    vec![bad.to_string()]
                )
                .is_err(),
                "{bad:?}"
            );
        }
        assert!(super::is_scope_token("api://x/access_as_user"));
        assert!(McpRoute::new("s", "t", "x", "https://a", "https://o", vec![]).is_err());
    }
}
