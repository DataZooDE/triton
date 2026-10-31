//! Inbound identity providers, the spec A2A face and long-running task
//! settings of the binary.
//!
//! Kept out of `main.rs` and `settings.rs` on purpose: the variables here
//! are one clap `Args` group flattened into the CLI, and the wiring is one
//! function per concern that `main` calls.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `TRITON_ENTRA_AUDIENCE` | Entra app id URI / client id the token's `aud` must be. Enables Entra. |
//! | `TRITON_ENTRA_ALLOWED_TENANTS` | `tid[=tenant],…` — the only Entra tenants accepted; a bare `tid` is its own data tenant. |
//! | `TRITON_ENTRA_AUTHORITY` / `TRITON_ENTRA_JWKS_URL` | test overrides; refused outside `env=local`. |
//! | `TRITON_GOOGLE_ACCESS_AUDIENCES` | OAuth client ids (comma-separated) an opaque Google access token may be issued to. Enables Google. |
//! | `TRITON_GOOGLE_ACCESS_ALLOWED_HD` | `hd[=tenant],…` — required with the audiences. |
//! | `TRITON_GOOGLE_TOKENINFO_URL` | test override; refused outside `env=local`. |
//! | `TRITON_A2A_PUBLIC_URL` | the A2A endpoint as callers reach it (the card's URL). Enables spec A2A. |
//! | `TRITON_A2A_DEFAULT_TOOL` | the tool a spec A2A message is dispatched to (required with the URL). |
//! | `TRITON_A2A_AGENT_NAME` / `_DESCRIPTION` / `_VERSION` | card fields. |
//! | `TRITON_A2A_SKILLS` | JSON array of A2A `AgentSkill` objects for the card. |
//! | `TRITON_A2A_BLOCK_SECS` | blocking-send cap before the working task returns (60). |
//! | `TRITON_A2A_STREAM_MAX_SECS` | client stream limit (660). |
//! | `TRITON_TASK_DB` | SQLite file for tasks; unset = process memory. |
//! | `TRITON_TASK_TTL_SECS` | retention of stored tasks (86400). |
//! | `TRITON_TASK_MAX_SECS` | a task still running after this fails as expired (3600). |
//! | `TRITON_MCP_ASYNC_WAIT_SECS` | MCP wait for an async upstream before "filed" (20). |
//! | `TRITON_MCP_CHECK_AFTER_SECS` | default `check_after_seconds` of a filed result (30). |
//! | `TRITON_MCP_TENANTS` | `slug=data-tenant:tid,…` — one MCP endpoint per customer at `/mcp/<slug>`, an RFC 9728 protected resource whose authorization server is that Entra tenant. Needs Entra and the origin. |
//! | `TRITON_MCP_PUBLIC_ORIGIN` | `https://host[:port]` as clients reach Triton; the metadata `resource` is `<origin>/mcp/<slug>`. |
//! | `TRITON_ENTRA_SCOPES` | the Entra API's delegated scope(s), space- or comma-separated; default `<Entra API>/access_as_user`. Every Entra token must carry one in `scp` (by its last path segment) on every face, else 403 `insufficient_scope`; MCP metadata and challenges advertise them. |

use std::collections::HashMap;
use std::sync::Arc;

use clap::Args;
use triton_identity::{GoogleAccessTokenVerifier, OidcConfig, OidcVerifier};

#[derive(Debug, Args)]
pub struct IngressArgs {
    /// Audience (`aud`) an Entra access token must carry. Setting it turns
    /// on the multi-tenant Entra verifier (Copilot Studio, Teams SSO, MCP
    /// clients registered in Entra).
    #[arg(long, env = "TRITON_ENTRA_AUDIENCE")]
    pub entra_audience: Option<String>,
    /// Allow-listed Entra tenants: `tid[=data-tenant]`, comma-separated.
    #[arg(long, env = "TRITON_ENTRA_ALLOWED_TENANTS", default_value = "")]
    pub entra_allowed_tenants: String,
    /// Entra authority override (tests only; `env=local`).
    #[arg(long, env = "TRITON_ENTRA_AUTHORITY")]
    pub entra_authority: Option<String>,
    /// Entra JWKS override (tests only; `env=local`).
    #[arg(long, env = "TRITON_ENTRA_JWKS_URL")]
    pub entra_jwks_url: Option<String>,
    /// OAuth client ids an opaque Google access token may be issued to
    /// (Gemini Enterprise agent registrations), comma-separated.
    #[arg(long, env = "TRITON_GOOGLE_ACCESS_AUDIENCES", default_value = "")]
    pub google_access_audiences: String,
    /// Allowed Google Workspace domains: `hd[=data-tenant]`, comma-separated.
    #[arg(long, env = "TRITON_GOOGLE_ACCESS_ALLOWED_HD", default_value = "")]
    pub google_access_allowed_hd: String,
    /// Google tokeninfo override (tests only; `env=local`).
    #[arg(long, env = "TRITON_GOOGLE_TOKENINFO_URL")]
    pub google_tokeninfo_url: Option<String>,

    /// Public URL of the spec A2A endpoint — what the Agent Card names.
    /// Gemini Enterprise drops non-443 ports, so this is configured, never
    /// derived from the bound socket. Setting it turns spec A2A on.
    #[arg(long, env = "TRITON_A2A_PUBLIC_URL")]
    pub a2a_public_url: Option<String>,
    /// The tool spec A2A prose is dispatched to.
    #[arg(long, env = "TRITON_A2A_DEFAULT_TOOL")]
    pub a2a_default_tool: Option<String>,
    #[arg(long, env = "TRITON_A2A_AGENT_NAME", default_value = "Triton agent")]
    pub a2a_agent_name: String,
    #[arg(
        long,
        env = "TRITON_A2A_AGENT_DESCRIPTION",
        default_value = "An agent reached through the DataZoo Triton gateway."
    )]
    pub a2a_agent_description: String,
    #[arg(long, env = "TRITON_A2A_AGENT_VERSION")]
    pub a2a_agent_version: Option<String>,
    /// JSON array of A2A `AgentSkill` objects for the card.
    #[arg(long, env = "TRITON_A2A_SKILLS")]
    pub a2a_skills: Option<String>,
    #[arg(long, env = "TRITON_A2A_BLOCK_SECS", default_value_t = 60)]
    pub a2a_block_secs: u64,
    #[arg(long, env = "TRITON_A2A_STREAM_MAX_SECS", default_value_t = 660)]
    pub a2a_stream_max_secs: u64,
    /// SQLite file for long-running tasks; unset keeps them in memory.
    #[arg(long, env = "TRITON_TASK_DB")]
    pub task_db: Option<std::path::PathBuf>,
    #[arg(long, env = "TRITON_TASK_TTL_SECS", default_value_t = 86_400)]
    pub task_ttl_secs: u64,
    #[arg(long, env = "TRITON_TASK_MAX_SECS", default_value_t = 3600)]
    pub task_max_secs: u64,
    #[arg(long, env = "TRITON_MCP_ASYNC_WAIT_SECS", default_value_t = 20)]
    pub mcp_async_wait_secs: u64,
    #[arg(long, env = "TRITON_MCP_CHECK_AFTER_SECS", default_value_t = 30)]
    pub mcp_check_after_secs: u64,
    /// Per-customer MCP endpoints: `slug=data-tenant:entra-tid`, comma-separated.
    #[arg(long, env = "TRITON_MCP_TENANTS", default_value = "")]
    pub mcp_tenants: String,
    /// Public origin of the MCP endpoints (`https://host[:port]`, no path).
    #[arg(long, env = "TRITON_MCP_PUBLIC_ORIGIN")]
    pub mcp_public_origin: Option<String>,
    /// Scopes advertised for the per-customer endpoints.
    #[arg(long, env = "TRITON_ENTRA_SCOPES")]
    pub entra_scopes: Option<String>,
}

/// The task timing knobs.
pub fn task_settings(args: &IngressArgs) -> triton_adapters_http::tasks::TaskSettings {
    use std::time::Duration;
    triton_adapters_http::tasks::TaskSettings {
        block: Duration::from_secs(args.a2a_block_secs.max(1)),
        stream_max: Duration::from_secs(args.a2a_stream_max_secs.max(1)),
        task_max: Duration::from_secs(args.task_max_secs.max(1)),
        mcp_wait: Duration::from_secs(args.mcp_async_wait_secs),
        mcp_check_after_secs: args.mcp_check_after_secs.max(1),
    }
}

/// The task store: SQLite at `TRITON_TASK_DB`, else process memory.
pub fn task_store(args: &IngressArgs) -> Result<triton_adapters_http::tasks::TaskStore, BootError> {
    use std::time::Duration;
    let retention = Duration::from_secs(args.task_ttl_secs.max(1));
    match &args.task_db {
        Some(path) => {
            let store = triton_adapters_http::tasks::TaskStore::sqlite(path, retention)
                .map_err(|e| BootError(format!("TRITON_TASK_DB: {e}")))?;
            tracing::info!(path = %path.display(), "persistent task store");
            Ok(store)
        }
        None => Ok(triton_adapters_http::tasks::TaskStore::memory().with_retention(retention)),
    }
}

/// The spec A2A configuration, when `TRITON_A2A_PUBLIC_URL` is set.
pub fn spec_a2a_config(
    args: &IngressArgs,
) -> Result<Option<triton_adapters_http::a2a_spec::SpecA2aConfig>, BootError> {
    let Some(url) = args
        .a2a_public_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    else {
        return Ok(None);
    };
    let parsed = url::Url::parse(url)
        .map_err(|e| BootError(format!("TRITON_A2A_PUBLIC_URL is not a URL: {e}")))?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err(BootError("TRITON_A2A_PUBLIC_URL must be http(s)".into()));
    }
    let tool = args
        .a2a_default_tool
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            BootError("TRITON_A2A_PUBLIC_URL is set but TRITON_A2A_DEFAULT_TOOL is not".into())
        })?;
    let skills = match &args.a2a_skills {
        None => None,
        Some(raw) => {
            let v: serde_json::Value = serde_json::from_str(raw)
                .map_err(|e| BootError(format!("TRITON_A2A_SKILLS is not JSON: {e}")))?;
            let arr = v
                .as_array()
                .filter(|a| {
                    a.iter()
                        .all(|s| s.get("id").is_some() && s.get("name").is_some())
                })
                .ok_or_else(|| {
                    BootError(
                        "TRITON_A2A_SKILLS must be a JSON array of skills with `id` and `name`"
                            .into(),
                    )
                })?;
            Some(arr.clone())
        }
    };
    Ok(Some(triton_adapters_http::a2a_spec::SpecA2aConfig {
        name: args.a2a_agent_name.clone(),
        description: args.a2a_agent_description.clone(),
        version: args
            .a2a_agent_version
            .clone()
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
        public_url: url.trim_end_matches('/').to_string(),
        // The public URL IS the endpoint; where it is mounted locally
        // (`/a2a` single-port, the A2A listener's root otherwise) is the
        // ingress's business.
        a2a_path: String::new(),
        default_tool: tool.to_string(),
        skills,
    }))
}

/// A configuration error that must stop the boot. The message names the
/// variable.
#[derive(Debug)]
pub struct BootError(pub String);

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse `key[=value],…`; a bare key maps to itself when `self_default`,
/// else to `None`.
fn parse_map(raw: &str) -> Vec<(String, Option<String>)> {
    raw.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| match e.split_once('=') {
            Some((k, v)) => (
                k.trim().to_ascii_lowercase(),
                Some(v.trim().to_string()).filter(|v| !v.is_empty()),
            ),
            None => (e.to_ascii_lowercase(), None),
        })
        .collect()
}

/// The extra verifiers beside the deployment's own OIDC issuer: Entra
/// multi-tenant (pushed into `verifiers`) and the opaque Google
/// access-token verifier (returned). `env` gates the test overrides.
pub fn identity_providers(
    args: &IngressArgs,
    env: &str,
    verifiers: &mut Vec<Arc<OidcVerifier>>,
    advertised: &mut Vec<(String, String)>,
) -> Result<Option<Arc<GoogleAccessTokenVerifier>>, BootError> {
    let local = env == "local";
    let refuse_override = |var: &str| {
        BootError(format!(
            "{var} overrides a provider endpoint and is accepted only with TRITON_ENV=local \
             (env is `{env}`)"
        ))
    };

    if let Some(aud) = args
        .entra_audience
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        let tenants = parse_map(&args.entra_allowed_tenants);
        if tenants.is_empty() {
            return Err(BootError(
                "TRITON_ENTRA_AUDIENCE is set but TRITON_ENTRA_ALLOWED_TENANTS is empty; \
                 name the Entra tenant ids (tid) that may call"
                    .into(),
            ));
        }
        let map: HashMap<String, String> = tenants
            .into_iter()
            .map(|(tid, tenant)| {
                let t = tenant.unwrap_or_else(|| tid.clone());
                (tid, t)
            })
            .collect();
        if args.entra_authority.is_some() && !local {
            return Err(refuse_override("TRITON_ENTRA_AUTHORITY"));
        }
        if args.entra_jwks_url.is_some() && !local {
            return Err(refuse_override("TRITON_ENTRA_JWKS_URL"));
        }
        let authority = args
            .entra_authority
            .clone()
            .unwrap_or_else(|| triton_identity::ENTRA_AUTHORITY.to_string());
        let jwks = args
            .entra_jwks_url
            .clone()
            .unwrap_or_else(|| triton_identity::ENTRA_COMMON_JWKS_URL.to_string());
        // Delegated access only, on every face (ADR-22): no app-only token.
        let cfg = OidcConfig::entra_multi_tenant_at(aud, map.clone(), authority, jwks)
            .with_required_scopes(scp_names(&entra_scopes(args)?));
        tracing::info!(
            audience = aud,
            tenants = map.len(),
            "Entra multi-tenant verifier enabled"
        );
        advertised.push((cfg.issuer.clone(), aud.to_string()));
        verifiers.push(Arc::new(OidcVerifier::new(cfg)));
    }

    let audiences: Vec<String> = args
        .google_access_audiences
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    if audiences.is_empty() {
        return Ok(None);
    }
    let domains = parse_map(&args.google_access_allowed_hd);
    if domains.is_empty() {
        return Err(BootError(
            "TRITON_GOOGLE_ACCESS_AUDIENCES is set but TRITON_GOOGLE_ACCESS_ALLOWED_HD is \
             empty; name the Google Workspace domains (hd) that may call"
                .into(),
        ));
    }
    // F9: a bare hd is its own data tenant, as a bare Entra tid is.
    let allowed: HashMap<String, Option<String>> = domains
        .into_iter()
        .map(|(hd, tenant)| {
            let t = tenant.unwrap_or_else(|| hd.clone());
            (hd, Some(t))
        })
        .collect();
    let mut google = GoogleAccessTokenVerifier::with_policy(audiences.clone(), Some(allowed));
    if let Some(url) = &args.google_tokeninfo_url {
        if !local {
            return Err(refuse_override("TRITON_GOOGLE_TOKENINFO_URL"));
        }
        google = google.with_tokeninfo_url(url.clone());
    }
    tracing::info!(
        audiences = audiences.len(),
        "Google access-token verifier enabled"
    );
    for aud in &audiences {
        advertised.push(("https://accounts.google.com".to_string(), aud.clone()));
    }
    Ok(Some(Arc::new(google)))
}

/// The scope a client requests when `TRITON_ENTRA_SCOPES` is unset: the
/// Entra API's delegated `access_as_user`, addressed by the audience as an
/// Application ID URI (`api://<client id>` for a bare client id).
fn default_mcp_scope(audience: &str) -> String {
    let aud = audience.trim().trim_end_matches('/');
    if aud.contains("://") {
        format!("{aud}/access_as_user")
    } else {
        format!("api://{aud}/access_as_user")
    }
}

/// The Entra API's delegated scopes (`TRITON_ENTRA_SCOPES`, else
/// `<audience>/access_as_user`); empty without Entra. Each must be an RFC
/// 6749 scope token, because it is quoted into challenges.
pub fn entra_scopes(args: &IngressArgs) -> Result<Vec<String>, BootError> {
    let Some(audience) = args
        .entra_audience
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    else {
        return Ok(Vec::new());
    };
    let scopes: Vec<String> = match args.entra_scopes.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => raw
            .split([' ', ','])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => vec![default_mcp_scope(audience)],
    };
    if let Some(bad) = scopes
        .iter()
        .find(|s| !triton_adapters_http::mcp_tenants::is_scope_token(s))
    {
        return Err(BootError(format!(
            "TRITON_ENTRA_SCOPES (or the default from TRITON_ENTRA_AUDIENCE): `{bad}` is \
             not an RFC 6749 scope token"
        )));
    }
    Ok(scopes)
}

/// What Entra writes into `scp` for a scope: its last path segment
/// (`api://<app>/access_as_user` → `access_as_user`).
pub fn scp_names(scopes: &[String]) -> Vec<String> {
    scopes
        .iter()
        .map(|s| s.rsplit('/').next().unwrap_or(s).to_string())
        .collect()
}

/// The per-customer MCP endpoints from `TRITON_MCP_TENANTS`. Every route
/// names an allow-listed Entra tenant that maps to the same data tenant, so
/// the binding on the path never contradicts the identity boundary.
pub fn mcp_routes(
    args: &IngressArgs,
    oidc_issuer: Option<&str>,
) -> Result<Vec<triton_adapters_http::mcp_tenants::McpRoute>, BootError> {
    let entries: Vec<&str> = args
        .mcp_tenants
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .collect();
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let refuse = |m: String| BootError(format!("TRITON_MCP_TENANTS: {m}"));
    if args
        .entra_audience
        .as_deref()
        .map(str::trim)
        .is_none_or(str::is_empty)
    {
        return Err(refuse(
            "needs the Entra verifier (TRITON_ENTRA_AUDIENCE)".into(),
        ));
    }
    let origin = args
        .mcp_public_origin
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .ok_or_else(|| {
            BootError(
                "TRITON_MCP_PUBLIC_ORIGIN is required with TRITON_MCP_TENANTS \
                 (the origin clients reach, e.g. https://agents.example.com)"
                    .into(),
            )
        })?;
    let parsed = url::Url::parse(origin)
        .map_err(|e| BootError(format!("TRITON_MCP_PUBLIC_ORIGIN is not a URL: {e}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(BootError(
            "TRITON_MCP_PUBLIC_ORIGIN must not carry userinfo (user:password@)".into(),
        ));
    }
    if !matches!(parsed.scheme(), "https" | "http")
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.origin().is_tuple()
    {
        return Err(BootError(
            "TRITON_MCP_PUBLIC_ORIGIN must be scheme://host[:port] without a path".into(),
        ));
    }
    // The canonical form clients compare with and Entra matches exactly:
    // lower-case scheme and host, no default port, no trailing slash.
    let origin = parsed.origin().ascii_serialization();
    let allowed: HashMap<String, String> = parse_map(&args.entra_allowed_tenants)
        .into_iter()
        .map(|(tid, t)| {
            let t = t.unwrap_or_else(|| tid.clone());
            (tid, t)
        })
        .collect();
    let scopes = entra_scopes(args)?;
    let authority = args
        .entra_authority
        .clone()
        .unwrap_or_else(|| triton_identity::ENTRA_AUTHORITY.to_string());
    // The tenant binding trusts an `entra` principal only from the
    // multi-tenant verifier. A deployment issuer that is itself an Entra
    // tenant would also produce `entra` principals (with the token's own
    // `tenant` claim), so the two do not mix.
    if let Some(iss) = oidc_issuer.map(str::trim).filter(|i| !i.is_empty()) {
        let under = |a: &str| iss.starts_with(a.trim_end_matches('/'));
        if under(&authority)
            || under(triton_identity::ENTRA_AUTHORITY)
            || under("https://sts.windows.net")
        {
            return Err(refuse(format!(
                "TRITON_OIDC_ISSUER ({iss}) is an Entra issuer; customer paths take \
                 Entra tokens only through TRITON_ENTRA_AUDIENCE, so unset it or use \
                 a non-Entra deployment issuer"
            )));
        }
    }
    let mut routes: Vec<triton_adapters_http::mcp_tenants::McpRoute> = Vec::new();
    for entry in entries {
        let (slug, rest) = entry
            .split_once('=')
            .ok_or_else(|| refuse(format!("`{entry}` is not slug=data-tenant:tid")))?;
        let (tenant, tid) = rest
            .split_once(':')
            .ok_or_else(|| refuse(format!("`{entry}` is not slug=data-tenant:tid")))?;
        let (slug, tenant, tid) = (slug.trim(), tenant.trim(), tid.trim().to_ascii_lowercase());
        if slug.is_empty()
            || !slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(refuse(format!(
                "slug `{slug}` must be lower-case letters, digits and `-`"
            )));
        }
        if tenant.is_empty() {
            return Err(refuse(format!("`{entry}` names no data tenant")));
        }
        match allowed.get(&tid) {
            None => {
                return Err(refuse(format!(
                    "`{slug}` names Entra tenant {tid}, which is not in \
                     TRITON_ENTRA_ALLOWED_TENANTS"
                )));
            }
            Some(mapped) if mapped != tenant => {
                return Err(refuse(format!(
                    "`{slug}` maps to data tenant `{tenant}`, but \
                     TRITON_ENTRA_ALLOWED_TENANTS maps {tid} to `{mapped}`"
                )));
            }
            Some(_) => {}
        }
        if routes.iter().any(|r| r.slug == slug) {
            return Err(refuse(format!("slug `{slug}` is listed twice")));
        }
        let route = triton_adapters_http::mcp_tenants::McpRoute::new(
            slug,
            tenant,
            &tid,
            &authority,
            &origin,
            scopes.clone(),
        )
        .map_err(|e| {
            BootError(format!(
                "TRITON_ENTRA_SCOPES (or the default from TRITON_ENTRA_AUDIENCE): {e}"
            ))
        })?;
        routes.push(route);
    }
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use super::parse_map;

    #[test]
    fn the_default_mcp_scope_follows_the_entra_audience() {
        assert_eq!(
            super::default_mcp_scope("00001111-aaaa-2222-bbbb-3333cccc4444"),
            "api://00001111-aaaa-2222-bbbb-3333cccc4444/access_as_user"
        );
        assert_eq!(
            super::default_mcp_scope("api://triton-mcp/"),
            "api://triton-mcp/access_as_user"
        );
    }

    #[test]
    fn maps_parse_bare_and_mapped_entries() {
        assert_eq!(
            parse_map(" A=acme, b ,,c= "),
            vec![
                ("a".to_string(), Some("acme".to_string())),
                ("b".to_string(), None),
                ("c".to_string(), None),
            ]
        );
    }
}
