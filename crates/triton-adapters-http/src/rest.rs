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

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::ACCEPT;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use triton_core::a2ui::{build_envelope, extract_surface};
use triton_core::audit::AuditBuffer;
use triton_core::{A2uiVersion, Dispatcher, RuntimeInfo, TritonError, envelope};

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
    /// Which chat-channel adapter to ask. One of `telegram`,
    /// `discord`, `googlechat`, `msteams`, `signal`, `whatsapp`.
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
                &req.result,
                &PREVIEW_KEY,
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
            match triton_chat_discord::surface_mapper::try_render_surface(&req.result, &PREVIEW_KEY)
            {
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
            match triton_chat_googlechat::surface_mapper::try_render_surface(&req.result) {
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
        "msteams" => match triton_chat_msteams::surface_mapper::try_render_surface(&req.result) {
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
        },
        "signal" => match triton_chat_signal::surface_mapper::try_render_surface(&req.result) {
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
            &req.result,
            &PREVIEW_KEY,
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

/// `GET /v1/audit?limit=N&trace_id=X` — newest-first slice of the
/// in-process audit ring buffer. Authenticated; this is operational
/// metadata about every request the gateway has processed since
/// boot, so it sees the same OIDC bearer as `/v1/tools`.
async fn audit_tail(
    State(state): State<RestState>,
    Query(q): Query<AuditQuery>,
    parts: Parts,
) -> Response {
    if let Err(e) = state.identity.verify(&parts).await {
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
    let limit = q.limit.clamp(1, AUDIT_LIMIT_MAX);
    let trace_id = q.trace_id.as_deref().filter(|s| !s.is_empty());
    let entries = AuditBuffer::recent(limit, trace_id);
    Json(json!({
        "entries": entries,
        "limit": limit,
        "trace_id": trace_id,
    }))
    .into_response()
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
    if let Err(e) = state.identity.verify(&parts).await {
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
    let mut entries = AuditBuffer::recent(AUDIT_LIMIT_MAX, Some(&trace_id));
    entries.reverse(); // chronological for a timeline
    let bodies = triton_core::trace::captured(&trace_id);
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
    match state
        .dispatcher
        .invoke_with_bytes(&name, &body, principal, "rest")
        .await
    {
        Ok(mut d) => match wrap_a2ui_if_requested(&mut d, requested) {
            Ok(()) => (StatusCode::OK, Json(envelope(&d))).into_response(),
            Err(e) => error_response(&e, Some(trace_id.as_str())),
        },
        Err(e) => error_response(&e, Some(trace_id.as_str())),
    }
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
