//! v0.2 PR 33 — Google Chat (formerly Hangouts Chat) adapter.
//!
//! Google Chat posts events to our webhook with a Google-signed JWT
//! in the `Authorization: Bearer …` header. Unlike Telegram (which
//! POSTs back via a courier) Google Chat reads the HTTP 200 body of
//! our webhook as the reply ("synchronous-response" pattern); for
//! PR 33 we use only this pattern so the adapter has no outbound
//! HTTP call.
//!
//! Scope (PR 33):
//!
//!   * Inbound auth: validate the JWT against Google's published
//!     certs (RS256, `iss` ∈ Google's OIDC / service-account issuers
//!     per #134, `aud == manifest.audience`). FR-I-8 constant-time semantics
//!     come from the cryptographic verifier itself, not byte
//!     comparisons we own — the only constant-time-relevant
//!     surface is "did the signature verify?" which jsonwebtoken
//!     answers via RSA verify.
//!   * Identity: `sender_table` lookup by the platform sender's
//!     `users/<id>` string. Same shape as Telegram + Discord.
//!   * Dispatch: text + narration only. Buttons / Selection /
//!     Form / Dashboard defer with counters (architecture.md §8.7
//!     — interactive Cards are a follow-up PR).
//!   * Audit pivot: `record_rejection` on refused inbounds,
//!     dispatcher invoke audits the `phase: dispatch`, and we call
//!     `record_post` with `latency_ms=0, http_status=200` for the
//!     inline response (the substrate has no per-post HTTP roundtrip
//!     to measure, but the audit shape must still carry one post
//!     line per delivered message).
//!
//! Layout mirrors Telegram (`lib.rs` + `surface_mapper.rs` +
//! `jwt_verifier.rs`). Adapter struct stays under the CLAUDE.md
//! §4 LOC budget; JWT details live in `jwt_verifier.rs`.

pub mod jwt_verifier;
pub mod surface_mapper;
pub use surface_mapper::RenderedMessage;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::Value;
use triton_core::{Dispatcher, PostOutcome, Principal, TritonError};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, SignatureScheme};
use triton_secrets::{ResolveError, SecretResolver};

use crate::jwt_verifier::{GoogleJwtVerifier, VerifierConfig};

pub const PROTOCOL: &str = "messenger:google_chat";

/// Per-Google-Chat-user claims resolved from the `sender_table`.
/// The table is keyed by the full `users/<id>` string Google sends
/// in `message.sender.name`.
#[derive(Debug, Clone, Deserialize)]
pub struct SenderClaims {
    pub sub: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub tenant: String,
}

/// Per-user claims in the `self_enrol` strategy's `fallback_table`.
/// Unlike `sender_table` there is no `sub`: the subject is always the
/// platform sender id (`users/<id>`), so it stays stable across the
/// unknown→enrolled transition (M-ENROL-1 "same subject"). The table
/// only supplies the scopes + tenant an enrolled sender receives.
#[derive(Debug, Clone, Deserialize)]
pub struct EnrolClaims {
    #[serde(default)]
    pub scopes: Vec<String>,
    pub tenant: String,
}

/// How this adapter resolves an inbound sender to a `Principal`.
enum IdentityMode {
    /// Operator-enumerated `users/<id>` → claims. Unknown sender = 401.
    SenderTable(HashMap<String, SenderClaims>),
    /// Pairing flow: unknown senders are admitted with the literal
    /// scope `"pairing"` (subject = sender id) so an upstream pairing
    /// tool can issue a code; operator confirmation enrols the sender
    /// in this table out-of-band, after which they get full scopes.
    SelfEnrol(HashMap<String, EnrolClaims>),
    /// Delegate resolution to a resolver tool reached through the
    /// upstream router (FR-I-7). The adapter calls `resolver_tool`
    /// with the platform sender id; the tool returns `{sub, scopes,
    /// tenant}`. A resolver error rejects the inbound.
    Upstream { resolver_tool: String },
}

/// Scope granted to an unknown sender during the `self_enrol` pairing
/// phase. The only scope they carry until an operator enrols them.
const PAIRING_SCOPE: &str = "pairing";
/// Tenant marker for pairing-phase principals. All unknown-sender
/// traffic shares this bucket for rate-limiting and is trivially
/// distinguishable in the audit trail.
const PAIRING_TENANT: &str = "pairing";

/// Protocol label for the resolver-tool dispatch under the `upstream`
/// identity strategy. Distinct from [`PROTOCOL`] so the resolve call's
/// audit lines never blur with the real command's.
const PROTOCOL_RESOLVE: &str = "messenger:google_chat:identity";

/// Byte cap on a button's signed correlation token. Google Chat's
/// `onClick.action` parameters are far roomier than Telegram's 64-byte
/// `callback_data` (`triton_correlation`'s default), so we allow a
/// generous bound while still rejecting an absurd token before HMACing
/// it (the webhook is an HTTP boundary; an oversized inbound shouldn't
/// force a large allocate + MAC). 1.5 KiB comfortably fits a tool name +
/// a modest args object signed and base64'd.
const CARD_CORRELATION_CAP: usize = 1536;

/// Principal shape the `upstream` resolver tool returns.
#[derive(Debug, Deserialize)]
struct ResolvedPrincipal {
    sub: String,
    #[serde(default)]
    scopes: Vec<String>,
    tenant: String,
}

pub struct GoogleChatAdapter {
    name: String,
    verifier: Arc<GoogleJwtVerifier>,
    identity: IdentityMode,
    /// Manifest `tool`: where plain inbound text dispatches (default
    /// `echo`). Commands (`/narrate` etc.) keep their special routes.
    inbound_tool: String,
    dispatcher: Arc<Dispatcher>,
    rate_limit: triton_core::ratelimit::TokenBucket,
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
    /// HMAC key signing/verifying button correlation tokens (FR-I,
    /// the `CARD_CLICKED` round-trip). Resolved from `correlation_key`.
    correlation_key: Vec<u8>,
}

impl GoogleChatAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        jwks_uri: String,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::GoogleChat {
            return Err(BuildError::WrongKind);
        }
        if adapter.inbound.signature != SignatureScheme::GoogleOidcJwt {
            return Err(BuildError::Unsupported(format!(
                "google_chat adapter requires `signature: google_oidc_jwt`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if !matches!(
            adapter.identity.kind,
            IdentityKind::SenderTable | IdentityKind::SelfEnrol | IdentityKind::Upstream
        ) {
            return Err(BuildError::Unsupported(format!(
                "google_chat adapter supports `identity.kind: sender_table`, `self_enrol`, or `upstream`; got {:?}",
                adapter.identity.kind
            )));
        }

        let aud_field = adapter
            .inbound
            .credentials
            .get("audience")
            .ok_or(BuildError::MissingCredential("inbound.audience"))?;
        let audience = resolver
            .resolve(aud_field)
            .await
            .map_err(|e| BuildError::Resolve("inbound.audience", e))?;
        if audience.is_empty() {
            return Err(BuildError::Unsupported(
                "inbound.audience MUST be non-empty".into(),
            ));
        }

        // FR-L-6 / NFR-S-5: every credential field MUST resolve at
        // boot. `outbound.token` is reserved for the future async
        // courier path (`https://chat.googleapis.com/...`); we still
        // resolve it now so a misconfigured `env://` ref fails closed.
        if let Some(field) = adapter.outbound.credentials.get("token") {
            resolver
                .resolve(field)
                .await
                .map_err(|e| BuildError::Resolve("outbound.token", e))?;
        }

        let identity = match adapter.identity.kind {
            IdentityKind::SenderTable => {
                let table_field = adapter
                    .identity
                    .credentials
                    .get("table")
                    .ok_or(BuildError::MissingCredential("identity.table"))?;
                let table_json = resolver
                    .resolve(table_field)
                    .await
                    .map_err(|e| BuildError::Resolve("identity.table", e))?;
                let table: HashMap<String, SenderClaims> = serde_json::from_str(&table_json)
                    .map_err(|e| BuildError::TableParse(e.to_string()))?;
                IdentityMode::SenderTable(table)
            }
            IdentityKind::SelfEnrol => {
                let table_field = adapter
                    .identity
                    .credentials
                    .get("fallback_table")
                    .ok_or(BuildError::MissingCredential("identity.fallback_table"))?;
                let table_json = resolver
                    .resolve(table_field)
                    .await
                    .map_err(|e| BuildError::Resolve("identity.fallback_table", e))?;
                let table: HashMap<String, EnrolClaims> = serde_json::from_str(&table_json)
                    .map_err(|e| BuildError::TableParse(e.to_string()))?;
                IdentityMode::SelfEnrol(table)
            }
            IdentityKind::Upstream => {
                let field = adapter
                    .identity
                    .credentials
                    .get("resolver_tool")
                    .ok_or(BuildError::MissingCredential("identity.resolver_tool"))?;
                let resolver_tool = resolver
                    .resolve(field)
                    .await
                    .map_err(|e| BuildError::Resolve("identity.resolver_tool", e))?;
                if resolver_tool.trim().is_empty() {
                    return Err(BuildError::Unsupported(
                        "identity.resolver_tool must be non-empty".into(),
                    ));
                }
                // The resolver MUST be an upstream tool (FR-I-7
                // "reached through the upstream router"). If its name
                // collides with an in-process tool, dispatcher.invoke
                // would run that locally and silently bypass the static
                // upstream router + the per-call RS256 JWT. Refuse at boot.
                if dispatcher
                    .descriptors()
                    .iter()
                    .any(|d| d.name == resolver_tool)
                {
                    return Err(BuildError::Unsupported(format!(
                        "identity.resolver_tool `{resolver_tool}` collides with an in-process \
                         tool; the upstream resolver must be a distinct static-upstream agent"
                    )));
                }
                IdentityMode::Upstream { resolver_tool }
            }
            // Guarded above; unreachable for other kinds.
            other => {
                return Err(BuildError::Unsupported(format!(
                    "google_chat adapter supports `identity.kind: sender_table` or `self_enrol`; got {other:?}"
                )));
            }
        };

        // `correlation_key` signs the HMAC tokens on rendered buttons
        // and verifies them on the `CARD_CLICKED` callback (see the
        // `surface_mapper::buttons_from_result` + `card_token` paths).
        // Resolved at boot so a bad `env://` ref also fails closed.
        let correlation_key = resolver
            .resolve(&adapter.correlation_key)
            .await
            .map_err(|e| BuildError::Resolve("correlation_key", e))?
            .into_bytes();

        // PR 28 headroom rationale: see triton-chat-telegram.
        const ADAPTER_HEADROOM: u32 = 10;
        let rate_limit = triton_core::ratelimit::TokenBucket::new(
            adapter
                .rate_limit
                .messages_per_sec
                .saturating_mul(ADAPTER_HEADROOM),
            adapter.rate_limit.burst.saturating_mul(ADAPTER_HEADROOM),
        );
        let per_tenant_limit = triton_core::ratelimit::PerTenantBuckets::new(
            adapter.rate_limit.messages_per_sec,
            adapter.rate_limit.burst,
        );
        let verifier = Arc::new(GoogleJwtVerifier::new(VerifierConfig::new(
            jwks_uri,
            audience.clone(),
        )));
        Ok(Self {
            name: name.to_string(),
            verifier,
            identity,
            inbound_tool: adapter.tool.clone(),
            dispatcher,
            rate_limit,
            per_tenant_limit,
            correlation_key,
        })
    }

    /// Mount the inbound webhook at `/<adapter-name>/webhook`.
    pub fn router(self: Arc<Self>) -> Router {
        let name = self.name.clone();
        let path = format!("/{name}/webhook");
        Router::new()
            .route(&path, post(handle_webhook))
            .with_state(self)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("adapter is not declared `kind: google_chat`")]
    WrongKind,
    #[error("PR 33 limitation: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("could not resolve credential field `{0}`: {1}")]
    Resolve(&'static str, #[source] ResolveError),
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
}

#[derive(Debug, Deserialize)]
struct GoogleChatEvent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: Option<GoogleChatMessage>,
    /// Present on `CARD_CLICKED`: the user who clicked (top-level on the
    /// interaction event, unlike `MESSAGE` where the actor is
    /// `message.sender`).
    #[serde(default)]
    user: Option<GoogleChatSender>,
    /// `CARD_CLICKED` action payload (modern shape): `common.parameters`
    /// is the flat `{key: value}` map of the clicked button's
    /// `onClick.action.parameters`.
    #[serde(default)]
    common: Option<EventCommon>,
    /// `CARD_CLICKED` action payload (legacy shape): `action.parameters`
    /// is a `[{key, value}]` list. We read whichever is present.
    #[serde(default)]
    action: Option<EventAction>,
}

#[derive(Debug, Default, Deserialize)]
struct EventCommon {
    #[serde(default)]
    parameters: std::collections::HashMap<String, String>,
    /// Selection/Form input values the user typed or chose, keyed by the
    /// widget `name`. Present on a `CARD_CLICKED` from a dropdown or form.
    #[serde(default, rename = "formInputs")]
    form_inputs: std::collections::HashMap<String, FormInput>,
}

/// One `formInputs` entry. Google nests the value under `stringInputs`
/// (text inputs, dropdowns, switches); we read the first value.
#[derive(Debug, Default, Deserialize)]
struct FormInput {
    #[serde(default, rename = "stringInputs")]
    string_inputs: Option<StringInputs>,
}

#[derive(Debug, Default, Deserialize)]
struct StringInputs {
    #[serde(default)]
    value: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct EventAction {
    #[serde(default)]
    parameters: Vec<EventActionParam>,
}

#[derive(Debug, Deserialize)]
struct EventActionParam {
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: String,
}

impl GoogleChatEvent {
    /// The signed correlation token a `CARD_CLICKED` carries in the
    /// button's [`surface_mapper::BUTTON_TOKEN_PARAM`] parameter, read
    /// from whichever action shape Google sent (modern `common.parameters`
    /// map or legacy `action.parameters` list).
    fn card_token(&self) -> Option<&str> {
        if let Some(common) = &self.common
            && let Some(v) = common.parameters.get(surface_mapper::BUTTON_TOKEN_PARAM)
        {
            return Some(v);
        }
        if let Some(action) = &self.action {
            for p in &action.parameters {
                if p.key == surface_mapper::BUTTON_TOKEN_PARAM {
                    return Some(&p.value);
                }
            }
        }
        None
    }

    /// The Selection/Form input values from a `CARD_CLICKED`, as
    /// `{ name: first_value }`. Empty for a plain button click. These are
    /// merged onto the token's signed base args before re-dispatch — they
    /// are user-supplied query parameters, not a trust decision.
    fn form_inputs(&self) -> Vec<(String, String)> {
        let Some(common) = &self.common else {
            return Vec::new();
        };
        common
            .form_inputs
            .iter()
            .filter_map(|(name, input)| {
                let v = input.string_inputs.as_ref()?.value.first()?;
                Some((name.clone(), v.clone()))
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct GoogleChatMessage {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    sender: Option<GoogleChatSender>,
}

#[derive(Debug, Deserialize)]
struct GoogleChatSender {
    /// Full resource name `users/<id>`. Adapter uses this verbatim
    /// as the sender_table key — exposing the leading `users/` in
    /// the key lets operators tell at a glance which entries are
    /// human senders (other Google Chat actors would use
    /// `bots/<id>`).
    #[serde(default)]
    name: Option<String>,
}

async fn handle_webhook(
    State(adapter): State<Arc<GoogleChatAdapter>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // FR-I-8 — verify the JWT FIRST, before parsing the body. The
    // JWT is in the header; we never read the body for an
    // unauthenticated request. Constant-time signature verification
    // is the cryptographic property of RSA-verify, not a manual
    // byte comparison.
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let raw = match auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
    {
        Some(t) if !t.is_empty() => t.trim(),
        _ => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Auth("missing or malformed Authorization header".into()),
            );
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };
    let claims = match adapter.verifier.verify(raw).await {
        Ok(c) => c,
        Err(e) => {
            // Never log the JWT body. Map the typed error to a
            // short reason so the audit pivot sees `error:auth`
            // without the token contents.
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Auth(format!("google jwt verify: {e}")),
            );
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };
    // The reply envelope depends on the app's deployment flavor, which
    // the verified token's actor tells us authoritatively: a Workspace
    // Add-on requires a `hostAppDataAction` wrapper, a classic Chat app a
    // bare `{text}` (see `surface_mapper::text_reply_body`).
    let workspace_addon = claims.is_workspace_addon();

    // NFR-P-3 first tier (adapter-wide rate limit). Consumed AFTER
    // the JWT check (so attackers can't waste tokens with bogus
    // bearers) but BEFORE body parse or sender resolution.
    if let Err(retry_after) = adapter.rate_limit.try_take() {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::RateLimited(format!(
                "google_chat adapter `{}` rate limit hit; retry in {:.2}s",
                adapter.name, retry_after
            )),
        );
        let secs = retry_after.ceil().max(1.0) as u64;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", secs.to_string())],
            "rate limited",
        )
            .into_response();
    }

    let event: GoogleChatEvent = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Validation(format!("malformed event body: {e}")),
            );
            return (StatusCode::BAD_REQUEST, "malformed").into_response();
        }
    };

    // Resolve the event into `(sender, tool, args)` for the shared
    // identity → dispatch → reply tail below. Two interactive kinds:
    //   * MESSAGE      — a typed message; `route_command` picks the tool.
    //   * CARD_CLICKED — a button click; the signed correlation token on
    //     the button carries the `(tool, args)` to re-invoke. We
    //     HMAC-verify it here, so a forged click that reaches the
    //     (authenticated) webhook still can't drive an arbitrary tool.
    // Other events (ADDED_TO_SPACE, REMOVED_FROM_SPACE, ...) are acked
    // 200 with an empty body so Google doesn't retry.
    let (sender_name, tool_name, args): (String, String, Value) = match event.kind.as_str() {
        "MESSAGE" => {
            let Some(message) = event.message else {
                record_rejection(
                    &adapter,
                    "-",
                    "-",
                    TritonError::Validation("MESSAGE event missing message body".into()),
                );
                return (StatusCode::BAD_REQUEST, "missing message").into_response();
            };
            let Some(text) = message.text.as_deref().filter(|s| !s.is_empty()) else {
                // Empty text (Google sends these for image/attachments) —
                // out of scope; ack and ignore.
                return (
                    StatusCode::OK,
                    axum::Json(Value::Object(Default::default())),
                )
                    .into_response();
            };
            let sender = message
                .sender
                .as_ref()
                .and_then(|s| s.name.as_deref())
                .unwrap_or("")
                .to_string();
            let (tool, args) = route_command(text, &adapter.inbound_tool);
            (sender, tool, args)
        }
        "CARD_CLICKED" => {
            let Some(token) = event.card_token() else {
                record_rejection(
                    &adapter,
                    "-",
                    "-",
                    TritonError::Validation("CARD_CLICKED missing correlation token".into()),
                );
                return (StatusCode::BAD_REQUEST, "missing action").into_response();
            };
            let (tool, mut args) = match triton_correlation::decode_with_cap(
                token,
                &adapter.correlation_key,
                CARD_CORRELATION_CAP,
            ) {
                Ok(p) => p,
                Err(_) => {
                    // Forged/expired/oversized token — never trust the
                    // click's tool/args. Audited as `error:auth`.
                    record_rejection(
                        &adapter,
                        "-",
                        "-",
                        TritonError::Auth("CARD_CLICKED correlation token invalid".into()),
                    );
                    return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
                }
            };
            // Merge any Selection/Form values the user supplied onto the
            // token's signed base args (the token fixed the TOOL; the
            // values are user query params bound by the named-query layer).
            let inputs = event.form_inputs();
            if !inputs.is_empty() {
                let map = match &mut args {
                    Value::Object(m) => m,
                    other => {
                        *other = Value::Object(Default::default());
                        other.as_object_mut().unwrap()
                    }
                };
                for (k, v) in inputs {
                    map.insert(k, Value::String(v));
                }
            }
            let sender = event
                .user
                .as_ref()
                .and_then(|u| u.name.as_deref())
                .unwrap_or("")
                .to_string();
            (sender, tool, args)
        }
        _ => {
            return (
                StatusCode::OK,
                axum::Json(Value::Object(Default::default())),
            )
                .into_response();
        }
    };
    let sender_name = sender_name.as_str();

    // FR-I-7 sender resolution → (sub, scopes, tenant).
    let (sub, scopes, tenant) = match &adapter.identity {
        // `upstream` delegates to a resolver tool reached through the
        // upstream router; it's async, so it lives outside the pure
        // `resolve_sender`.
        IdentityMode::Upstream { resolver_tool } => {
            match resolve_via_upstream(&adapter.dispatcher, resolver_tool, sender_name).await {
                Ok(p) => p,
                Err(e) => {
                    record_rejection(&adapter, "-", "-", e);
                    return (StatusCode::UNAUTHORIZED, "identity resolution failed")
                        .into_response();
                }
            }
        }
        // Sync strategies. `None` means reject (sender_table unknown,
        // or a malformed self_enrol sender).
        other => match resolve_sender(other, sender_name) {
            Some(p) => p,
            None => {
                record_rejection(
                    &adapter,
                    "-",
                    "-",
                    TritonError::Auth(format!("unknown sender `{sender_name}`")),
                );
                return (StatusCode::UNAUTHORIZED, "unknown sender").into_response();
            }
        },
    };

    // NFR-P-3 second tier: per-tenant fair-share, keyed by the
    // resolved tenant id (never the platform sender name).
    if let Err(retry_after) = adapter.per_tenant_limit.try_take(&tenant) {
        record_rejection(
            &adapter,
            &sub,
            &tenant,
            TritonError::RateLimited(format!(
                "tenant `{}` rate limit hit on adapter `{}`; retry in {:.2}s",
                tenant, adapter.name, retry_after
            )),
        );
        let secs = retry_after.ceil().max(1.0) as u64;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", secs.to_string())],
            "tenant rate limited",
        )
            .into_response();
    }

    let principal = Principal {
        sub: sub.clone(),
        scopes: scopes.clone(),
        groups: Vec::new(),
        tenant: tenant.clone(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };
    let principal_for_post = principal.clone();

    let result = adapter
        .dispatcher
        .invoke(&tool_name, args, principal, PROTOCOL)
        .await;
    match result {
        Ok(dispatch) => match render_dispatch_result(&dispatch.result) {
            Ok(rendered) => {
                // Selection/Form/Dashboard still defer (Cards for those
                // aren't wired); Buttons now render as Cards v2 actions.
                if rendered.deferred_selections
                    + rendered.deferred_forms
                    + rendered.deferred_dashboards
                    > 0
                {
                    tracing::warn!(
                        tool = tool_name,
                        deferred_selections = rendered.deferred_selections,
                        deferred_forms = rendered.deferred_forms,
                        deferred_dashboards = rendered.deferred_dashboards,
                        "google_chat surface mapper: non-button interactive components deferred (Cards not yet wired)",
                    );
                }
                if rendered.truncated {
                    tracing::warn!(
                        tool = tool_name,
                        cap_bytes = surface_mapper::GOOGLE_CHAT_TEXT_MAX_BYTES,
                        "google_chat surface mapper: rendered text exceeded cap; truncated",
                    );
                }
                // Sign each interactive component's (tool, base_args) into a
                // correlation token and render it as a Cards v2 widget
                // (button / dropdown / form); on submit Google echoes the
                // token (plus any typed/selected formInputs) on CARD_CLICKED,
                // where we HMAC-verify and re-dispatch. A component whose
                // token would exceed the cap is dropped (logged), never sent.
                let signed: Vec<(surface_mapper::InteractiveSpec, String)> =
                    surface_mapper::interactive_from_result(&dispatch.result)
                        .into_iter()
                        .filter_map(|spec| {
                            match triton_correlation::encode_with_cap(
                                spec.tool(),
                                &spec.base_args(),
                                &adapter.correlation_key,
                                CARD_CORRELATION_CAP,
                            ) {
                                Ok(token) => Some((spec, token)),
                                Err(e) => {
                                    tracing::warn!(tool = spec.tool(), error = %e, "google_chat: dropping interactive component (correlation token too large)");
                                    None
                                }
                            }
                        })
                        .collect();
                let body = if signed.is_empty() {
                    surface_mapper::build_inline_response(&rendered, workspace_addon)
                } else {
                    surface_mapper::build_interactive_card(&rendered.text, &signed, workspace_addon)
                };
                adapter.dispatcher.record_post(
                    &tool_name,
                    PROTOCOL,
                    &principal_for_post,
                    0,
                    Ok((200, PostOutcome::Posted, None)),
                );
                (StatusCode::OK, axum::Json(body)).into_response()
            }
            Err(surface_mapper::RenderError::EmptyAfterRender) => {
                tracing::warn!(
                    tool = tool_name,
                    "google_chat surface mapper: empty surface; skipping inline response",
                );
                let provider =
                    TritonError::Provider("google_chat surface mapper: empty surface".into());
                adapter.dispatcher.record_post(
                    &tool_name,
                    PROTOCOL,
                    &principal_for_post,
                    0,
                    Err((&provider, 0, PostOutcome::Dropped, None)),
                );
                // Still 200 so Google doesn't retry; empty body.
                (
                    StatusCode::OK,
                    axum::Json(Value::Object(Default::default())),
                )
                    .into_response()
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "google_chat dispatch failed");
            // Permanent app-layer failures ack 200 to avoid retry
            // storms — Google Chat keeps trying non-2xx for a long
            // while. Transient infra faults earn a 502 so the
            // platform can re-deliver.
            let status = match &e {
                TritonError::Provider(_) => StatusCode::BAD_GATEWAY,
                _ => StatusCode::OK,
            };
            // Audit the failure path as `phase: post` with the
            // observed class so operators see one consistent shape
            // across success + failure.
            adapter.dispatcher.record_post(
                &tool_name,
                PROTOCOL,
                &principal_for_post,
                0,
                Err((&e, 0, PostOutcome::Dropped, Some("error_response"))),
            );
            (
                status,
                axum::Json(surface_mapper::text_reply_body(
                    &format!("(error: {})", e.class()),
                    workspace_addon,
                )),
            )
                .into_response()
        }
    }
}

/// Resolve a platform sender to `(sub, scopes, tenant)`. `None` means
/// the sender must be rejected (401).
///
/// Policy note (intentional, per requirements §7): the `"pairing"`
/// scope is an *identity signal forwarded to upstream tools*, not an
/// authorization boundary Triton enforces. Triton is identity-aware,
/// not policy-rich — upstream agents enforce per-tool policy on the
/// scopes they receive. A pairing-phase principal is clearly marked
/// (`scopes == ["pairing"]`, `tenant == "pairing"`) so an upstream
/// pairing tool admits it and every other tool refuses it.
///
/// Enrolment is not hot-reloaded: like `sender_table`, the
/// `fallback_table` is resolved once at boot from the immutable
/// manifest (FR-L-4). Operator confirmation takes effect on the next
/// manifest reload / alloc restart.
fn resolve_sender(
    identity: &IdentityMode,
    sender_name: &str,
) -> Option<(String, Vec<String>, String)> {
    match identity {
        IdentityMode::SenderTable(table) => table
            .get(sender_name)
            .map(|c| (c.sub.clone(), c.scopes.clone(), c.tenant.clone())),
        IdentityMode::SelfEnrol(table) => {
            // A pairing subject must be a real human user resource
            // name. Reject empty / non-`users/` senders (e.g. Google's
            // `bots/<id>` actors) rather than admit a malformed subject.
            if !is_valid_user_sender(sender_name) {
                return None;
            }
            // M-ENROL-1: unknown senders are admitted with scope
            // "pairing" only; subject = the platform sender id so it
            // is stable once the operator enrols them in fallback_table.
            Some(match table.get(sender_name) {
                Some(c) => (sender_name.to_string(), c.scopes.clone(), c.tenant.clone()),
                None => (
                    sender_name.to_string(),
                    vec![PAIRING_SCOPE.to_string()],
                    PAIRING_TENANT.to_string(),
                ),
            })
        }
        // `upstream` is async and handled by `resolve_via_upstream` at
        // the call site; never resolved here. Defensive `None` keeps
        // the match total.
        IdentityMode::Upstream { .. } => None,
    }
}

/// A Google Chat human sender resource name: non-empty and of the
/// form `users/<id>` with a non-empty id.
fn is_valid_user_sender(name: &str) -> bool {
    name.strip_prefix("users/").is_some_and(|id| !id.is_empty())
}

/// Resolve a sender to `(sub, scopes, tenant)` by invoking the
/// `resolver_tool` through the upstream router (FR-I-7 `upstream`).
/// The resolver receives `{platform, sender}` and returns `{sub,
/// scopes, tenant}`. Any failure (empty sender, resolver error,
/// malformed reply) is an `Auth` error so the inbound is rejected
/// rather than dispatched with a guessed principal.
///
/// The resolver call is itself a dispatch: it emits a `phase:
/// dispatch` audit line under [`PROTOCOL_RESOLVE`] plus the upstream
/// router's `phase: upstream` line (the latter hardcoded to
/// `protocol: "upstream"`), both under the bootstrap principal's
/// trace_id — distinct from the real command's audit pair.
async fn resolve_via_upstream(
    dispatcher: &Dispatcher,
    resolver_tool: &str,
    sender_name: &str,
) -> Result<(String, Vec<String>, String), TritonError> {
    if sender_name.is_empty() {
        return Err(TritonError::Auth(
            "empty sender for upstream resolver".into(),
        ));
    }
    let bootstrap = Principal {
        sub: "identity-resolver".to_string(),
        scopes: vec!["resolve".to_string()],
        groups: Vec::new(),
        tenant: "system".to_string(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };
    let args = serde_json::json!({ "platform": "google_chat", "sender": sender_name });
    let dispatch = dispatcher
        .invoke(resolver_tool, args, bootstrap, PROTOCOL_RESOLVE)
        .await
        .map_err(|e| TritonError::Auth(format!("identity resolver `{resolver_tool}`: {e}")))?;
    let resolved: ResolvedPrincipal = serde_json::from_value(dispatch.result)
        .map_err(|e| TritonError::Auth(format!("resolver reply not {{sub,scopes,tenant}}: {e}")))?;
    if resolved.sub.trim().is_empty() || resolved.tenant.trim().is_empty() {
        return Err(TritonError::Auth(
            "resolver returned empty sub or tenant".into(),
        ));
    }
    Ok((resolved.sub, resolved.scopes, resolved.tenant))
}

/// Strip a leading `@bot ` mention if present, then route by the
/// first word. Mirrors Telegram's `route_command`: `/<tool> <rest>`
/// goes to `tool` with `{ subject: rest }` for narrate, otherwise
/// falls through to the manifest-configured default tool.
fn route_command(text: &str, default_tool: &str) -> (String, Value) {
    let trimmed = text
        .trim_start_matches("@bot ")
        .trim_start_matches("@bot")
        .trim_start();
    if let Some(rest) = trimmed.strip_prefix('/') {
        let (tool, subject) = rest.split_once(' ').unwrap_or((rest, ""));
        if tool == "narrate" {
            return (
                "narrate".to_string(),
                serde_json::json!({ "subject": subject }),
            );
        }
    }
    (
        default_tool.to_string(),
        serde_json::json!({ "message": trimmed }),
    )
}

fn render_dispatch_result(
    result: &serde_json::Value,
) -> Result<RenderedMessage, surface_mapper::RenderError> {
    if let Some(r) = surface_mapper::try_render_surface(result) {
        return r;
    }
    // Fall back to the bare-text path so echo-shaped results
    // (`{"echo": "..."}`) still render without an A2UI envelope.
    let text = if let Some(obj) = result.as_object()
        && obj.len() == 1
        && let Some(s) = obj.values().next().and_then(|v| v.as_str())
    {
        s.to_string()
    } else if let Some(s) = result.as_str() {
        s.to_string()
    } else {
        serde_json::to_string(result).unwrap_or_else(|_| "<unrenderable>".to_string())
    };
    if text.is_empty() {
        return Err(surface_mapper::RenderError::EmptyAfterRender);
    }
    Ok(RenderedMessage {
        text,
        deferred_buttons: 0,
        deferred_selections: 0,
        deferred_forms: 0,
        deferred_dashboards: 0,
        truncated: false,
    })
}

fn record_rejection(adapter: &GoogleChatAdapter, sub: &str, tenant: &str, e: TritonError) {
    adapter.dispatcher.record_rejection(
        &adapter.name,
        PROTOCOL,
        sub,
        tenant,
        &uuid::Uuid::new_v4().to_string(),
        &e,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enrol_table() -> IdentityMode {
        let mut t = HashMap::new();
        t.insert(
            "users/77".to_string(),
            EnrolClaims {
                scopes: vec!["chat".to_string()],
                tenant: "acme".to_string(),
            },
        );
        IdentityMode::SelfEnrol(t)
    }

    #[test]
    fn self_enrol_unknown_sender_gets_only_pairing_scope() {
        // The audit trail can't show scopes; pin the exact pairing
        // principal here. Unknown sender → subject = sender id,
        // scopes == ["pairing"] EXACTLY (no leakage), tenant marker.
        let (sub, scopes, tenant) = resolve_sender(&enrol_table(), "users/55").unwrap();
        assert_eq!(sub, "users/55", "subject = platform sender id");
        assert_eq!(scopes, vec!["pairing".to_string()]);
        assert_eq!(tenant, "pairing");
    }

    #[test]
    fn self_enrol_enrolled_sender_keeps_sender_id_as_subject() {
        // Enrolled sender → same subject (sender id), full scopes +
        // tenant from the fallback_table.
        let (sub, scopes, tenant) = resolve_sender(&enrol_table(), "users/77").unwrap();
        assert_eq!(
            sub, "users/77",
            "subject stays the sender id across enrolment"
        );
        assert_eq!(scopes, vec!["chat".to_string()]);
        assert_eq!(tenant, "acme");
    }

    #[test]
    fn self_enrol_rejects_empty_or_nonuser_sender() {
        let id = enrol_table();
        // Empty sender → reject.
        assert!(resolve_sender(&id, "").is_none());
        // Non-`users/` actor (bot) → reject.
        assert!(resolve_sender(&id, "bots/42").is_none());
        // `users/` with empty id → reject.
        assert!(resolve_sender(&id, "users/").is_none());
        // Valid human user → admitted (pairing, since not enrolled).
        assert!(resolve_sender(&id, "users/55").is_some());
    }

    #[test]
    fn sender_table_unknown_returns_none_for_rejection() {
        let mut t = HashMap::new();
        t.insert(
            "users/99".to_string(),
            SenderClaims {
                sub: "alice".to_string(),
                scopes: vec!["chat".to_string()],
                tenant: "acme".to_string(),
            },
        );
        let id = IdentityMode::SenderTable(t);
        assert!(resolve_sender(&id, "users/unknown").is_none());
        let (sub, _, _) = resolve_sender(&id, "users/99").unwrap();
        assert_eq!(
            sub, "alice",
            "sender_table uses the table's sub, not the id"
        );
    }
}
