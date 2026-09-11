//! REST adapter — well-known operational endpoints (`/healthz`,
//! `/version`) and the tool surface (`POST /v1/tools/:name`, plus
//! the `GET /v1/tools` listing landing in PR 5).
//!
//! Per ADR-6 this module is a pure unwrap/wrap shell: identity is
//! delegated to [`crate::identity`], the dispatcher owns timing,
//! audit emission, **and** the rejected-phase emission for boundary
//! failures (so adapters never own the audit schema). Error
//! variants map to HTTP statuses per architecture.md §8.3.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::ACCEPT;
use axum::http::request::Parts;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use triton_core::a2ui::{build_envelope, envelope_to_surface, extract_surface};
use triton_core::audit::AuditBuffer;
use triton_core::{A2uiVersion, Dispatcher, RuntimeInfo, StreamEvent, TritonError, envelope};

use crate::identity::IdentityProvider;

/// Anonymous discovery payload served at `GET /v1/runtime`. Read by
/// the Flutter explorer SPA at boot to learn which OIDC issuer to
/// redirect to for PKCE login and which env/image it's looking at.
/// `oidc_*` fields are `null` when the operator hasn't configured
/// the explorer for this env — the SPA renders a clear "ask an
/// operator to register me" message instead of failing PKCE
/// opaquely.
#[derive(Clone, serde::Serialize)]
pub struct RuntimeDiscovery {
    pub env: String,
    pub image_sha: Option<String>,
    pub package_version: String,
    pub binary_sha: String,
    pub oidc_issuer: Option<String>,
    pub oidc_audience: Option<String>,
    pub oidc_client_id: Option<String>,
    /// EVERY accepted issuer/audience pair, in configuration order —
    /// including when there is only one, so a client reading this never
    /// has to special-case "one pair means read the scalars instead".
    ///
    /// The three `oidc_*` scalars above keep their exact meaning and
    /// still describe the FIRST pair: they are a published contract
    /// (ADR-0017's verification reads `oidc_issuer`, and the Explorer
    /// SPA points PKCE at it), so this is an additive field, never a
    /// redefinition. Omitted when empty — a host with no OIDC at all, or
    /// `triton-bin`, which is single-pair by construction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub oidc_providers: Vec<OidcProviderInfo>,
    /// Base path/URL for the MCP and A2A surfaces, when they are NOT on
    /// the conventional dev ports (8001/8002). The embedded single-port
    /// host (triton-embed) sets these to `/mcp` and `/a2a` so the SPA can
    /// reach the trio same-origin; `triton-bin` leaves them `null` and the
    /// SPA falls back to its `:8003→:8001/:8002` port-swap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a2a_base: Option<String>,
}

/// One accepted OIDC issuer/audience pair, as advertised at
/// `/v1/runtime`. Public config: both values are already visible in a
/// PR diff and in browser URLs, and a caller needs them to know which
/// token to present.
#[derive(Clone, serde::Serialize)]
pub struct OidcProviderInfo {
    pub issuer: String,
    pub audience: String,
}

/// Shared state owned by the binary, cloned into every handler via
/// axum `State`. `Arc` everywhere so handler signatures stay cheap
/// and the realization "wrap settings in Arc from the start" holds
/// (Rust port §2).
#[derive(Clone)]
pub struct RestState {
    pub runtime: Arc<RuntimeInfo>,
    pub discovery: Arc<RuntimeDiscovery>,
    pub dispatcher: Arc<Dispatcher>,
    pub identity: Arc<IdentityProvider>,
    /// Loaded v0.2 `adapter.yaml`, if any. None when the binary
    /// boots without TRITON_MANIFEST_PATH (v0.1 mode).
    pub manifest: Option<Arc<triton_manifest::Manifest>>,
    /// Shared Prometheus metrics registry. Same instance backs the
    /// unauthenticated tailnet-only `/metrics` listener on
    /// `TRITON_METRICS_PORT`; the REST route here is the
    /// authenticated CORS-friendly path the explorer uses.
    pub metrics: Arc<triton_core::Metrics>,
    /// #306 crew F6/F7: principals the DEPLOYMENT names as operators,
    /// parsed once at boot. Read per request previously, which re-parsed
    /// the environment on every `/v1/audit` call and printed a
    /// denylist-branded warning for a malformed audit entry.
    pub audit_operators: Arc<std::collections::HashSet<(String, String)>>,
    /// OIDC signer for static-upstream dispatch. When set, Triton acts as the
    /// issuer for the JWTs it mints to agents: it serves discovery + JWKS at the
    /// `/.well-known/*` routes below so agents verify those tokens. `None`
    /// outside static-signing mode (e.g. dev-token mode, or unsigned static mode).
    pub oidc_signer: Option<Arc<triton_identity::JwtSigner>>,
}

pub fn router(state: RestState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        // OIDC issuer surface for the JWTs Triton mints in static-upstream mode
        // (agents fetch these via AGENT_OIDC_ISSUER). Unauthenticated, like any
        // OIDC discovery/JWKS endpoint. 404 when not signing.
        .route(
            "/.well-known/openid-configuration",
            get(openid_configuration),
        )
        .route("/.well-known/jwks.json", get(jwks))
        .route("/v1/runtime", get(runtime_discovery))
        .route("/v1/tools", get(list_tools))
        .route("/v1/tools/{name}", post(invoke_tool))
        .route("/v1/audit", get(audit_tail))
        .route("/v1/trace/{trace_id}", get(trace_view))
        .route("/v1/manifest", get(manifest_view))
        .route("/v1/metrics", get(metrics_view))
        .route("/v1/surface/render", post(surface_render))
        .with_state(state)
}

/// `GET /.well-known/openid-configuration` — OIDC discovery for Triton's
/// static-upstream signing key. 404 when Triton isn't signing.
async fn openid_configuration(State(state): State<RestState>) -> Response {
    match &state.oidc_signer {
        Some(signer) => Json(signer.discovery()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /.well-known/jwks.json` — public keys for verifying the JWTs Triton
/// mints to agents in static-upstream mode. 404 when Triton isn't signing.
async fn jwks(State(state): State<RestState>) -> Response {
    match &state.oidc_signer {
        Some(signer) => Json(signer.jwks().clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /v1/metrics` — returns the same Prometheus text exposition
/// the tailnet-only `/metrics` listener serves, but authenticated
/// and reachable through Triton's REST adapter so the Flutter
/// explorer (cross-origin, behind CORS) can render it without
/// punching through the substrate's tag-based ACL on `:9090`.
///
/// G-7 still holds: the tailnet-only listener stays the canonical
/// scrape target for the substrate's Prometheus. This route is
/// purely for operators inspecting metrics through the browser.
async fn metrics_view(State(state): State<RestState>, parts: Parts) -> Response {
    if let Err(e) = state.identity.verify(&parts).await {
        state.dispatcher.record_rejection(
            "v1/metrics",
            "rest",
            "-",
            "-",
            &uuid::Uuid::new_v4().to_string(),
            &e,
        );
        return error_response(&e, None);
    }
    let body = state.metrics.render();
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct SurfaceRenderRequest {
    /// Which preview adapter to ask — one of [`PREVIEW_ADAPTERS`]
    /// (`telegram`, `discord`, `googlechat`, `msteams`, `signal`,
    /// `whatsapp`, plus the product-named `copilot` / `gemini`).
    adapter: String,
    /// Raw A2UI `result` envelope `{ "surface": {...} }` as a tool
    /// would return — the same shape `extract_surface` parses.
    result: serde_json::Value,
}

/// Adapters the preview endpoint can render. Kept in one place so
/// the `400 unknown adapter` message and the explorer's dropdown
/// can agree on the closed set.
const PREVIEW_ADAPTERS: &[&str] = &[
    "telegram",
    "discord",
    "googlechat",
    "msteams",
    "signal",
    "whatsapp",
    // Product-named preview surfaces: `copilot` reuses the msteams Adaptive
    // Cards mapper (Copilot's card host); `gemini` is the markdown-forward
    // Gemini Enterprise answer surface.
    "copilot",
    "gemini",
    // The richest surface: HTML email renders the message complete — buttons
    // as links, dashboard as a table, plus a subject line no other mapper has.
    "email",
];

/// `POST /v1/surface/render` — runs the supplied A2UI Surface
/// through a chat-channel surface mapper and returns what the
/// adapter would post. Lets the explorer's A2UI diff page show the
/// L6′ degradation (Telegram inline keyboards, Discord components
/// v2, MS Teams Adaptive Cards, …) alongside the v0.8 / v0.9
/// envelopes without the operator actually wiring a live bot.
///
/// Every mapper is the SAME function its live courier calls, so the
/// preview can't drift from production rendering.
async fn surface_render(
    State(state): State<RestState>,
    parts: Parts,
    Json(req): Json<SurfaceRenderRequest>,
) -> Response {
    if let Err(e) = state.identity.verify(&parts).await {
        state.dispatcher.record_rejection(
            "v1/surface/render",
            "rest",
            "-",
            "-",
            &uuid::Uuid::new_v4().to_string(),
            &e,
        );
        return error_response(&e, None);
    }

    // The button-bearing mappers (Telegram, Discord) sign
    // callback_data with this key. The preview is read-only — the
    // rendered buttons are never posted — so a fixed zero key is
    // safe: tokens carrying it can't be replayed against any live
    // adapter, every one of which uses a distinct env-resolved
    // key. Mappers without interactive callbacks ignore it.
    const PREVIEW_KEY: [u8; 32] = [0u8; 32];

    let not_a2ui = || {
        error_response(
            &TritonError::Validation(
                "result is not an A2UI surface (missing `surface` field)".into(),
            ),
            None,
        )
    };
    let empty = |adapter: &str| {
        Json(json!({ "adapter": adapter, "rendered": false, "reason": "empty_after_render" }))
            .into_response()
    };

    // The mappers each call `extract_surface`, which wants the canonical
    // `{ "surface": … }` shape. Accept that directly — but also accept an
    // already-negotiated v0.9 envelope (what the Explorer holds for a turn
    // it is *already showing*) by reversing it back to a surface, so a
    // channel preview never has to re-invoke the tool. Anything else passes
    // through untouched and trips `not_a2ui` below.
    let surface_input = if req.result.get("surface").is_some() {
        req.result.clone()
    } else if let Some(surface) = envelope_to_surface(&req.result) {
        json!({ "surface": surface })
    } else {
        req.result.clone()
    };

    // One arm per adapter. Each crate's `RenderedMessage` /
    // `RenderedInteraction` is a distinct type, so we map each into
    // the common JSON envelope explicitly. The shared keys (`text`,
    // `deferred_*`, `truncated`) line up; adapter-specific extras
    // (`parse_mode` + `reply_markup` for telegram, `components` for
    // discord, `has_dashboard_raster` for the rasterising ones) are
    // added only where they exist.
    match req.adapter.as_str() {
        "telegram" => {
            match triton_chat_telegram::surface_mapper::try_render_surface(
                &surface_input,
                &PREVIEW_KEY,
                // Preview only — see the discord arm.
                "preview",
                "preview",
            ) {
                None => not_a2ui(),
                Some(Err(_)) => empty("telegram"),
                Some(Ok(m)) => Json(json!({
                    "adapter": "telegram",
                    "rendered": true,
                    "text": m.text,
                    "parse_mode": m.parse_mode,
                    "reply_markup": m.reply_markup,
                    "deferred_buttons": m.deferred_buttons,
                    "deferred_selections": m.deferred_selections,
                    "deferred_dashboards": m.deferred_dashboards,
                    "truncated": m.truncated,
                    "has_dashboard_raster": m.dashboard.is_some(),
                }))
                .into_response(),
            }
        }
        "discord" => {
            match triton_chat_discord::surface_mapper::try_render_surface(
                &surface_input,
                &PREVIEW_KEY,
                // Preview only: this endpoint renders a surface for the
                // explorer and the tokens it mints are never dispatched,
                // so the tenant and sender are placeholders like
                // `PREVIEW_KEY` itself.
                "preview",
                "preview",
            ) {
                None => not_a2ui(),
                Some(Err(_)) => empty("discord"),
                Some(Ok(m)) => Json(json!({
                    "adapter": "discord",
                    "rendered": true,
                    "text": m.content,
                    "components": m.components,
                    "deferred_buttons": m.deferred_buttons,
                    "deferred_selections": m.deferred_selections,
                    "deferred_forms": m.deferred_forms,
                    "deferred_dashboards": m.deferred_dashboards,
                    "truncated": m.truncated,
                    "has_dashboard_raster": m.dashboard.is_some(),
                }))
                .into_response(),
            }
        }
        "googlechat" => {
            match triton_chat_googlechat::surface_mapper::try_render_surface(&surface_input) {
                None => not_a2ui(),
                Some(Err(_)) => empty("googlechat"),
                Some(Ok(m)) => Json(json!({
                    "adapter": "googlechat",
                    "rendered": true,
                    "text": m.text,
                    "deferred_buttons": m.deferred_buttons,
                    "deferred_selections": m.deferred_selections,
                    "deferred_forms": m.deferred_forms,
                    "deferred_dashboards": m.deferred_dashboards,
                    "truncated": m.truncated,
                }))
                .into_response(),
            }
        }
        "msteams" => {
            match triton_chat_msteams::surface_mapper::try_render_surface(&surface_input) {
                None => not_a2ui(),
                Some(Err(_)) => empty("msteams"),
                Some(Ok(m)) => Json(json!({
                    "adapter": "msteams",
                    "rendered": true,
                    "text": m.text,
                    "deferred_buttons": m.deferred_buttons,
                    "deferred_selections": m.deferred_selections,
                    "deferred_forms": m.deferred_forms,
                    "deferred_dashboards": m.deferred_dashboards,
                    "truncated": m.truncated,
                }))
                .into_response(),
            }
        }
        "signal" => match triton_chat_signal::surface_mapper::try_render_surface(&surface_input) {
            None => not_a2ui(),
            Some(Err(_)) => empty("signal"),
            Some(Ok(m)) => Json(json!({
                "adapter": "signal",
                "rendered": true,
                "text": m.text,
                "deferred_buttons": m.deferred_buttons,
                "deferred_selections": m.deferred_selections,
                "deferred_forms": m.deferred_forms,
                "deferred_dashboards": m.deferred_dashboards,
                "truncated": m.truncated,
            }))
            .into_response(),
        },
        "whatsapp" => match triton_chat_whatsapp::surface_mapper::try_render_surface(
            &surface_input,
            &PREVIEW_KEY,
            // Preview only — see the discord arm.
            "preview",
            "preview",
        ) {
            None => not_a2ui(),
            Some(Err(_)) => empty("whatsapp"),
            Some(Ok(m)) => Json(json!({
                "adapter": "whatsapp",
                "rendered": true,
                "text": m.text,
                "deferred_buttons": m.deferred_buttons,
                "deferred_selections": m.deferred_selections,
                "deferred_forms": m.deferred_forms,
                "deferred_dashboards": m.deferred_dashboards,
                "truncated": m.truncated,
                "has_dashboard_raster": m.dashboard.is_some(),
            }))
            .into_response(),
        },
        // Microsoft Copilot renders M365 Adaptive Cards — the same primitive
        // the msteams mapper produces — so `copilot` is that mapper under the
        // product name (echoing `copilot` back so the UI labels it right).
        "copilot" => {
            match triton_chat_msteams::surface_mapper::try_render_surface(&surface_input) {
                None => not_a2ui(),
                Some(Err(_)) => empty("copilot"),
                Some(Ok(m)) => Json(json!({
                    "adapter": "copilot",
                    "rendered": true,
                    "text": m.text,
                    "deferred_buttons": m.deferred_buttons,
                    "deferred_selections": m.deferred_selections,
                    "deferred_forms": m.deferred_forms,
                    "deferred_dashboards": m.deferred_dashboards,
                    "truncated": m.truncated,
                }))
                .into_response(),
            }
        }
        // Gemini Enterprise — a markdown-forward answer surface that renders
        // Dashboards as tables and Sources as citations (no raster, no cards).
        "gemini" => match triton_chat_gemini::surface_mapper::try_render_surface(&surface_input) {
            None => not_a2ui(),
            Some(Err(_)) => empty("gemini"),
            Some(Ok(m)) => Json(json!({
                "adapter": "gemini",
                "rendered": true,
                "text": m.text,
                "deferred_buttons": m.deferred_buttons,
                "deferred_selections": m.deferred_selections,
                "deferred_forms": m.deferred_forms,
                "deferred_dashboards": m.deferred_dashboards,
                "truncated": m.truncated,
                "has_dashboard_raster": false,
            }))
            .into_response(),
        },
        "email" => match triton_chat_email::surface_mapper::try_render_surface(&surface_input) {
            None => not_a2ui(),
            Some(Err(_)) => empty("email"),
            // Email carries two extras no other mapper does: a `subject`
            // (derived from the lead text) and a full `html` body. `text` is
            // the plaintext alternative, so the common envelope keys still
            // line up. Buttons/dashboards render inline, so their `deferred_*`
            // counters are 0 — only a form's submit defers.
            Some(Ok(m)) => Json(json!({
                "adapter": "email",
                "rendered": true,
                "subject": m.subject,
                "html": m.html,
                "text": m.text,
                "deferred_buttons": m.deferred_buttons,
                "deferred_selections": m.deferred_selections,
                "deferred_forms": m.deferred_forms,
                "deferred_dashboards": m.deferred_dashboards,
                "truncated": m.truncated,
            }))
            .into_response(),
        },
        other => error_response(
            &TritonError::Validation(format!(
                "unknown adapter `{other}`: expected one of {}",
                PREVIEW_ADAPTERS.join(", ")
            )),
            None,
        ),
    }
}

/// `GET /v1/manifest` — returns the loaded `adapter.yaml` as JSON,
/// with credentials redacted by the `SecretField` serializer. Auth-
/// gated; same Bearer as `/v1/tools`. Returns `{ loaded: false }`
/// when no manifest is configured (v0.1 mode) so the SPA can render
/// a clear "no manifest" hint.
async fn manifest_view(State(state): State<RestState>, parts: Parts) -> Response {
    if let Err(e) = state.identity.verify(&parts).await {
        state.dispatcher.record_rejection(
            "v1/manifest",
            "rest",
            "-",
            "-",
            &uuid::Uuid::new_v4().to_string(),
            &e,
        );
        return error_response(&e, None);
    }
    match &state.manifest {
        Some(m) => Json(json!({
            "loaded": true,
            "manifest": &**m,
        }))
        .into_response(),
        None => Json(json!({ "loaded": false })).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct AuditQuery {
    /// Number of recent entries to return. Capped server-side at 500
    /// so an unbounded `?limit=...` can't allocate the whole buffer
    /// into one response.
    #[serde(default = "default_limit")]
    limit: usize,
    /// Optional trace_id filter — returns only entries whose stored
    /// trace_id matches exactly. Empty == no filter.
    #[serde(default)]
    trace_id: Option<String>,
}

const AUDIT_LIMIT_DEFAULT: usize = 50;
const AUDIT_LIMIT_MAX: usize = 500;
const fn default_limit() -> usize {
    AUDIT_LIMIT_DEFAULT
}

/// Scope that grants the cross-tenant view of `/v1/audit` and
/// `/v1/trace`. Without it a caller sees only their own tenant's rows.
pub const AUDIT_READ_ALL_SCOPE: &str = "audit:read-all";

/// Is this principal an operator the DEPLOYMENT named?
///
/// The one place the two-authority rule is written down. Both
/// [`audit_visibility_in`] and [`may_read_trace`] gate on it, and they
/// used to answer it independently — a duplicated `scope && (local ||
/// contains)` in each. That is one line to get right and two places to
/// forget: a crew review found that deleting the `named` half from
/// `may_read_trace` left every `/v1/audit` test green while A2A
/// `tasks/get` handed out other tenants' traces.
///
/// BOTH halves are required outside `local`: the scope is a claim only
/// the ISSUER can mint, the list is writable only by the DEPLOYMENT.
/// Neither alone hands out everyone's audit trail. In `local` the scope
/// suffices, mirroring the dev-token gate (ADR-10 / factor X), so a dev
/// loop can read its own trail without an env var.
pub fn is_named_operator(
    principal: &triton_core::principal::Principal,
    operators: &std::collections::HashSet<(String, String)>,
    env: &str,
) -> bool {
    let claims_scope = principal.scopes.iter().any(|s| s == AUDIT_READ_ALL_SCOPE);
    let named =
        env == "local" || operators.contains(&(principal.tenant.clone(), principal.sub.clone()));
    claims_scope && named
}

/// Which audit rows this principal may read (#282).
///
/// An operator (holding [`AUDIT_READ_ALL_SCOPE`]) sees everything. Anyone
/// else sees their own tenant only — and NOT the unattributed rows.
///
/// Unattributed rows (`tenant: "-"`) are the boundary rejections: they
/// exist precisely because no principal was resolved, so there is no
/// tenant to scope them by, and they are the rows most likely to name
/// another tenant's sender or reply target. Operator-only is the
/// fail-closed reading. The cost is real and worth stating: a
/// tenant-scoped caller cannot see their own failed authentications,
/// because at the moment of failure nothing knew they were theirs.
fn audit_visibility_in(
    principal: &triton_core::principal::Principal,
    // Operators the DEPLOYMENT named — not a claim the issuer can mint.
    operators: &std::collections::HashSet<(String, String)>,
    // The RESOLVED environment (`Dispatcher::env`), not the process
    // variable: clap reads `TRITON_ENV` into `Settings` but never sets
    // it, so `--env prod` would look local here and hand the
    // cross-tenant view to the `audit:read-all` claim alone.
    env: &str,
) -> impl Fn(&triton_core::audit::AuditEntry) -> bool {
    // BOTH the scope the issuer can mint AND membership of a list only
    // the deployment can write — see `audit_operators`.
    //
    // Outside `local` an unset list means NOBODY holds the cross-tenant
    // view. That is a behaviour change for a deployment relying on the
    // scope alone, and it is the fail-closed direction: the symptom is an
    // operator seeing only their own rows, which is visible and fixable,
    // rather than a caller silently minting themselves everyone's. The
    // boot warning below names the fix.
    //
    // In `local` the scope alone still suffices, mirroring how the
    // dev-token path is already gated (ADR-10 / factor X) — otherwise
    // every local dev loop needs an env var to see its own audit trail.
    let operator = is_named_operator(principal, operators, env);
    let tenant = principal.tenant.clone();
    // A reserved tenant is a shared marker, not a tenant: `-` is what
    // nearly every live OIDC caller carries and `pairing` is what every
    // un-enrolled chat sender shares. Comparing them for equality would
    // hand one caller every other unattributed caller's rows — which is
    // most of the buffer, including the boundary rejections that name
    // other tenants' senders. They match nothing but an operator.
    let scopable = !triton_core::principal::is_reserved_tenant(&tenant);
    move |e| operator || (scopable && e.tenant == tenant)
}

/// `GET /v1/audit?limit=N&trace_id=X` — newest-first slice of the
/// in-process audit ring buffer. Authenticated; this is operational
/// metadata about every request the gateway has processed since
/// boot, so it sees the same OIDC bearer as `/v1/tools`.
async fn audit_tail(
    State(state): State<RestState>,
    Query(q): Query<AuditQuery>,
    parts: Parts,
) -> Response {
    // #282: the Principal was previously verified and then DISCARDED,
    // so any authenticated caller read every tenant's rows. This is the
    // one confidentiality surface no upstream contract can cover —
    // Triton serves the data itself.
    let principal = match state.identity.verify(&parts).await {
        Ok(p) => p,
        Err(e) => {
            state.dispatcher.record_rejection(
                "v1/audit",
                "rest",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            return error_response(&e, None);
        }
    };
    if let Err(e) = state
        .dispatcher
        .deny_if_revoked(&principal, "v1/audit", "rest")
    {
        return error_response(&e, Some(&principal.trace_id));
    }
    let limit = q.limit.clamp(1, AUDIT_LIMIT_MAX);
    let trace_id = q.trace_id.as_deref().filter(|s| !s.is_empty());
    let entries = AuditBuffer::recent_where(
        limit,
        trace_id,
        audit_visibility_in(&principal, &state.audit_operators, state.dispatcher.env()),
    );
    Json(json!({
        "entries": entries,
        "limit": limit,
        "trace_id": trace_id,
    }))
    .into_response()
}

/// May this caller see the captured bodies for a trace?
///
/// The capture store keys on `trace_id` alone and carries no tenant, so
/// it cannot be filtered per entry. The tenant-scoped `entries` are the
/// proxy: non-empty means at least one audited step of this trace ran
/// under the caller's tenant, which is what entitles them to the rest of
/// it. Empty means the trace is somebody else's.
///
/// Without this the pivot was one hop and needed no out-of-band
/// knowledge — read your own `/v1/audit`, lift any `trace_id`, and
/// receive the whole trace's bodies, including the identity-resolver
/// dispatch that runs under `tenant: "system"`.
///
/// Separated from the handler so it is testable: `bodies` is only ever
/// populated when the dev `capture` feature is compiled in, which the
/// integration-test binary does not enable, so an end-to-end test cannot
/// tell this fix from its absence.
fn bodies_visible(entries: &[triton_core::audit::AuditEntry]) -> bool {
    !entries.is_empty()
}

/// May this caller read anything keyed on `trace_id`?
///
/// The one answer to that question, shared by `/v1/trace` and spec-A2A's
/// `tasks/get` — whose task ids ARE trace ids. Two implementations of
/// this rule would drift, and the second surface is how the first one's
/// fix got bypassed.
pub fn may_read_trace(
    principal: &triton_core::principal::Principal,
    operators: &std::collections::HashSet<(String, String)>,
    env: &str,
    trace_id: &str,
) -> bool {
    // Deliberately NOT `audit_visibility_in`. That answers "may this
    // caller BROWSE the tail", where a reserved tenant must match
    // nothing — two callers holding `-` are both unattributed, not
    // tenant-mates. This answers "is this specific trace THEIRS", and
    // there the subject is the precise key: a caller's own dispatches
    // carry their `sub` whatever their tenant resolves to.
    //
    // Using the browse predicate here would lock every `-` caller out of
    // their OWN task — which is nearly every live caller, since a
    // single-tenant OIDC token with no `tenant` claim resolves to `-`.
    let tenant_scopable = !triton_core::principal::is_reserved_tenant(&principal.tenant);
    let sub = principal.sub.clone();
    let tenant = principal.tenant.clone();
    let operator = is_named_operator(principal, operators, env);
    // The subject match is TENANT-QUALIFIED. `AuditEntry.subject` is the
    // bare `principal.sub` (`dispatcher.rs`), so comparing subjects alone
    // let two principals who merely share a `sub` string read each
    // other's traces across tenants — and this predicate gates A2A
    // `tasks/get`, whose ids ARE trace ids and travel to the counterparty
    // in every `message/send` reply. A colliding `sub` is ordinary: chat
    // adapters derive it from platform sender ids, and two issuers can
    // mint the same string.
    //
    // Pairing it keeps the case the comment above defends — a caller on a
    // reserved tenant reading their OWN trace — because their entries
    // carry that same reserved tenant, so the pair matches.
    let visible = AuditBuffer::recent_where(AUDIT_LIMIT_MAX, Some(trace_id), move |e| {
        operator
            || (e.subject == sub && e.tenant == tenant)
            || (tenant_scopable && e.tenant == tenant)
    });
    bodies_visible(&visible)
}

/// `GET /v1/trace/{trace_id}` — the one communication as a timeline: all
/// audit phases for `trace_id` in chronological order (inbound → dispatch
/// → upstream → post). Authenticated like `/v1/audit`. The `bodies` field
/// is populated only when the dev `capture` feature is compiled in
/// (request/response/surface payloads); otherwise it is empty.
async fn trace_view(
    State(state): State<RestState>,
    Path(trace_id): Path<String>,
    parts: Parts,
) -> Response {
    // #282: same shape as /v1/audit — verified, then discarded.
    let principal = match state.identity.verify(&parts).await {
        Ok(p) => p,
        Err(e) => {
            state.dispatcher.record_rejection(
                "v1/trace",
                "rest",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            return error_response(&e, None);
        }
    };
    if let Err(e) = state
        .dispatcher
        .deny_if_revoked(&principal, "v1/trace", "rest")
    {
        return error_response(&e, Some(&principal.trace_id));
    }
    let mut entries = AuditBuffer::recent_where(
        AUDIT_LIMIT_MAX,
        Some(&trace_id),
        audit_visibility_in(&principal, &state.audit_operators, state.dispatcher.env()),
    );
    entries.reverse(); // chronological for a timeline
    let bodies = if may_read_trace(
        &principal,
        &state.audit_operators,
        state.dispatcher.env(),
        &trace_id,
    ) {
        triton_core::trace::captured(&trace_id)
    } else {
        Vec::new()
    };
    Json(json!({
        "trace_id": trace_id,
        "entries": entries,
        "bodies": bodies,
    }))
    .into_response()
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// `GET /v1/runtime` — anonymous SPA bootstrap. See [`RuntimeDiscovery`].
async fn runtime_discovery(State(state): State<RestState>) -> Json<RuntimeDiscovery> {
    Json((*state.discovery).clone())
}

async fn version(State(state): State<RestState>) -> Json<RuntimeInfo> {
    Json((*state.runtime).clone())
}

/// `GET /v1/tools` — FR-A-5. Authenticated; surfaces every
/// registered tool's name + input JSON schema + `returns_a2ui`
/// flag. Adapters never reach into the registry directly; they
/// ask the dispatcher (ADR-6 — single seam).
async fn list_tools(State(state): State<RestState>, parts: Parts) -> Response {
    // Auth check first; even the listing leaks tool inventory which
    // could be useful to an attacker on a multi-tenant gateway.
    if let Err(e) = state.identity.verify(&parts).await {
        state.dispatcher.record_rejection(
            "v1/tools",
            "rest",
            "-",
            "-",
            &uuid::Uuid::new_v4().to_string(),
            &e,
        );
        return error_response(&e, None);
    }
    Json(json!({ "tools": state.dispatcher.descriptors_all().await })).into_response()
}

async fn invoke_tool(
    State(state): State<RestState>,
    Path(name): Path<String>,
    parts: Parts,
    body: Bytes,
) -> Response {
    // Parse the Accept header before any auth check so a malformed
    // A2UI version surfaces as Validation (400) before we even
    // touch identity.
    let requested = match parse_a2ui_accept(&parts) {
        Ok(v) => v,
        Err(e) => {
            state.dispatcher.record_rejection(
                &name,
                "rest",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            return error_response(&e, None);
        }
    };

    // Pre-parse a trace id so a boundary rejection still carries
    // one in the audit line (it'd be misleading to omit). The
    // dispatcher generates its own when a Principal is built; this
    // path is only used when there's no Principal yet.
    let principal = match state.identity.verify(&parts).await {
        Ok(p) => p,
        Err(e) => {
            state.dispatcher.record_rejection(
                &name,
                "rest",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            return error_response(&e, None);
        }
    };
    let trace_id = principal.trace_id.clone();

    // Content negotiation (issue #132): `Accept: text/event-stream`
    // streams the dispatch as SSE; every other caller keeps the
    // byte-identical buffered JSON envelope below (open/closed). The
    // dispatcher still emits exactly one ADR-6 audit line on either path.
    if wants_sse(&parts) {
        return match state
            .dispatcher
            .invoke_streaming_with_bytes(&name, &body, principal, "rest", requested)
            .await
        {
            // 200 stream open: frames flow, audit fires at termination.
            Ok(events) => sse_response(events),
            // Pre-first-byte failure: no SSE headers flushed yet, so we
            // can still answer with an ordinary HTTP error (audited once
            // inside the dispatcher).
            Err(e) => error_response(&e, Some(trace_id.as_str())),
        };
    }

    match state
        .dispatcher
        .invoke_with_bytes(&name, &body, principal, "rest")
        .await
    {
        Ok(mut d) => {
            // Capture any per-turn tool trace the upstream attached under
            // `_meta.tool_trace` BEFORE the A2UI wrap rewrites `d.result` and
            // drops `_meta` — then reflect it as an envelope sibling (REST has
            // no top-level `_meta`; `trace_id` rides the same way). Only a
            // structured array is mirrored, never a scalar/object blob.
            let tool_trace = d
                .result
                .get("_meta")
                .and_then(|m| m.get("tool_trace"))
                .filter(|t| t.is_array())
                .cloned();
            match wrap_a2ui_if_requested(&mut d, requested) {
                Ok(()) => {
                    let mut body = envelope(&d);
                    if let Some(trace) = tool_trace {
                        body["tool_trace"] = trace;
                    }
                    (StatusCode::OK, Json(body)).into_response()
                }
                Err(e) => error_response(&e, Some(trace_id.as_str())),
            }
        }
        Err(e) => error_response(&e, Some(trace_id.as_str())),
    }
}

/// True when any `Accept` media range names `text/event-stream` — the
/// caller wants the SSE response path (issue #132). Kept separate from
/// [`parse_a2ui_accept`] so the A2UI media-type negotiation is untouched.
/// Shared with the A2A adapter.
pub(crate) fn wants_sse(parts: &Parts) -> bool {
    parts.headers.get_all(ACCEPT).iter().any(|v| {
        v.to_str().is_ok_and(|s| {
            s.split(',')
                .any(|range| range.split(';').next().map(str::trim) == Some("text/event-stream"))
        })
    })
}

/// Render a stream of [`StreamEvent`]s as an axum SSE response. Each
/// event becomes one `event: <name>` / `data: <compact-json>` frame.
/// A ~15s keep-alive comment frame keeps proxies (kamal-proxy, nginx)
/// from dropping a connection that idles while an upstream LLM
/// synthesises — it never fires once the stream has terminated.
/// Shared with the A2A adapter.
pub(crate) fn sse_response(events: futures::stream::BoxStream<'static, StreamEvent>) -> Response {
    let frames = events.map(|ev| {
        Ok::<Event, std::convert::Infallible>(
            Event::default()
                .event(ev.event_name())
                .data(ev.data().to_string()),
        )
    });
    Sse::new(frames)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// Wrap a dispatch result into an A2UI envelope when the tool opted
/// in via `returns_a2ui` AND the caller negotiated A2UI. Otherwise
/// the raw result is returned untouched (FR-A-5).
///
/// A tool that advertises `returns_a2ui` but emits an unparseable
/// `surface` is a bug — we surface it as `TritonError::Tool` so the
/// client sees 502 instead of being silently handed raw JSON.
fn wrap_a2ui_if_requested(
    d: &mut triton_core::Dispatch,
    requested: Option<A2uiVersion>,
) -> Result<(), TritonError> {
    let Some(version) = requested else {
        return Ok(());
    };
    if !d.returns_a2ui {
        return Ok(());
    }
    let surface = extract_surface(&d.result).map_err(|e| {
        tracing::warn!(tool_advertised_a2ui = true, error = %e, "tool returned non-A2UI shape");
        TritonError::Tool(format!("tool advertised A2UI but {e}"))
    })?;
    d.result = build_envelope(&surface, version.into());
    Ok(())
}

/// FR-A-3: parse `Accept: application/json+a2ui[; version=0.9]` into
/// an [`A2uiVersion`]. Returns `Some(version)` if **any** Accept
/// range names `application/json+a2ui`, regardless of its position
/// in the comma-separated list. Returns `None` only when no A2UI
/// range is present (caller is happy with plain JSON). Unknown
/// versions inside an A2UI range are an explicit error — never
/// silently downgrade.
fn parse_a2ui_accept(parts: &Parts) -> Result<Option<A2uiVersion>, TritonError> {
    let Some(raw) = parts.headers.get(ACCEPT) else {
        return Ok(None);
    };
    let s = raw
        .to_str()
        .map_err(|_| TritonError::Validation("non-ASCII Accept header".into()))?;

    // Walk every comma-separated media range — an A2UI offer
    // anywhere in the list wins over a leading `application/json`
    // (Codex PR 10 finding). We don't implement RFC 9110 q-value
    // sorting; the spec only enumerates two A2UI values.
    let mut found = None;
    for entry in s.split(',') {
        let mut parts = entry.split(';').map(str::trim);
        let Some(media) = parts.next() else { continue };
        if media != "application/json+a2ui" {
            continue;
        }
        for param in parts {
            if let Some(version) = param.strip_prefix("version=") {
                let v = match version.trim_matches('"') {
                    "0.8" => A2uiVersion::V08,
                    "0.9" => A2uiVersion::V09,
                    other => {
                        return Err(TritonError::Validation(format!(
                            "unknown A2UI version: {other}"
                        )));
                    }
                };
                return Ok(Some(v));
            }
        }
        found = Some(A2uiVersion::default());
    }
    Ok(found)
}

pub(crate) fn error_response(e: &TritonError, trace_id: Option<&str>) -> Response {
    let status = http_status_for(e);
    let mut body = json!({
        "error": e.class(),
        "message": e.to_string(),
    });
    if let Some(tid) = trace_id {
        body["trace_id"] = json!(tid);
    }
    (status, Json(body)).into_response()
}

fn http_status_for(e: &TritonError) -> StatusCode {
    // TritonError::http_status() is the single source of truth shared
    // with A2A and the dispatcher audit (architecture §8.3).
    StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::BAD_GATEWAY)
}

#[cfg(test)]
mod trace_scope_tests {
    use super::{AUDIT_READ_ALL_SCOPE, audit_visibility_in, bodies_visible};
    use triton_core::audit::{AuditEntry, AuditPhase};
    use triton_core::principal::Principal;

    fn entry(tenant: &str) -> AuditEntry {
        AuditEntry {
            kind: "audit",
            phase: AuditPhase::Dispatch,
            when: "2026-09-06T00:00:00Z".into(),
            who: "someone".into(),
            what: "echo".into(),
            env: "test".into(),
            result: "ok".into(),
            protocol: "rest".into(),
            tool: "echo".into(),
            subject: "someone".into(),
            tenant: tenant.into(),
            latency_ms: 0,
            status: 200,
            status_label: None,
            status_detail: None,
            error_detail: None,
            ttfb_ms: None,
            sender_ref: None,
            destination: None,
            suppressed: None,
            trace_id: "t-1".into(),
        }
    }

    /// No operators named — the deployment granted nobody.
    fn no_operators() -> std::collections::HashSet<(String, String)> {
        std::collections::HashSet::new()
    }

    fn principal(tenant: &str, scopes: &[&str]) -> Principal {
        Principal {
            sub: "caller".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            groups: Vec::new(),
            tenant: tenant.into(),
            raw_token: String::new(),
            trace_id: "t".into(),
            sender_ref: None,
            conversation_ref: None,
        }
    }

    /// The gate the handler applies to `bodies`. It is unit-tested rather
    /// than driven end-to-end because `bodies` is only populated with the
    /// dev `capture` feature, which the integration-test binary does not
    /// compile in — an end-to-end assertion there passes whether or not
    /// the fix is present, which is how the first version of this test
    /// was written and why it proved nothing.
    #[test]
    fn bodies_follow_the_entries_beside_them() {
        assert!(!bodies_visible(&[]), "no visible entries ⇒ no bodies");
        assert!(bodies_visible(&[entry("acme")]), "own trace ⇒ bodies");
    }

    /// Outside `local` the `audit:read-all` claim alone is NOT enough:
    /// that namespace belongs to the issuer. The deployment must also
    /// name the principal in `TRITON_AUDIT_OPERATORS`.
    ///
    /// This is the test that would have caught reading `TRITON_ENV` from
    /// the process instead of the resolved value — a `--env prod`
    /// deployment leaves the variable unset and looked local.
    #[test]
    fn outside_local_the_scope_claim_alone_grants_nothing() {
        let op = principal("acme", &[AUDIT_READ_ALL_SCOPE]);
        // No named operators: the deployment granted nobody.
        let ops = no_operators();
        let visible = audit_visibility_in(&op, &ops, "prod");
        assert!(
            !visible(&entry("globex")),
            "an issuer-minted scope must not grant the cross-tenant view \
             in a real deployment"
        );
        // Their own tenant is unaffected — this is not a lockout.
        assert!(visible(&entry("acme")));
    }

    /// A shared marker is not a tenant, so it must not match another
    /// caller carrying the same marker.
    #[test]
    fn a_reserved_tenant_matches_nothing_but_an_operator() {
        for marker in ["-", "pairing", ""] {
            let p = principal(marker, &["chat"]);
            let ops = no_operators();
            let can_see = audit_visibility_in(&p, &ops, "local");
            assert!(
                !can_see(&entry(marker)),
                "`{marker}` is a shared marker, not a tenant — two callers \
                 carrying it are both unattributed, not tenant-mates"
            );
            let op = principal(marker, &[AUDIT_READ_ALL_SCOPE]);
            let operator = audit_visibility_in(&op, &ops, "local");
            assert!(
                operator(&entry(marker)),
                "the operator grant must still restore the view"
            );
        }
        // A real tenant still matches itself.
        let pa = principal("acme", &["chat"]);
        let ops2 = no_operators();
        let acme = audit_visibility_in(&pa, &ops2, "local");
        assert!(acme(&entry("acme")));
        assert!(!acme(&entry("globex")));
    }
}
