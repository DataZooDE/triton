//! v0.2 PR 35 — Microsoft Teams chat-channel adapter.
//!
//! Bot-Framework-style inbound webhook: Teams posts an Activity
//! JSON document with `Authorization: Bearer <jwt>` to our `/<name>/
//! webhook` route. We verify the JWT against Microsoft's published
//! JWKS (constant-time via `jsonwebtoken` + `ring`), enforce the
//! sender_table, rate-limit, dispatch, and reply by POST-ing a
//! reply Activity to the platform-asserted `serviceUrl` taken from
//! the JWT.
//!
//! Adapter discipline (ADR-6 + CLAUDE.md §4):
//! * Adapter stays at ~200 LOC; JWT validation, token fetch, and
//!   surface rendering live in dedicated modules.
//! * Dispatcher is the single audit pivot. We call
//!   `record_rejection` on every refused inbound and `record_post`
//!   on every reply attempt — no other audit emission.

pub mod jwt_verifier;
pub mod surface_mapper;
pub mod token_client;

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
use triton_manifest::{Adapter, AdapterKind, IdentityKind, OutboundKind, SignatureScheme};
use triton_secrets::{ResolveError, SecretResolver};

use jwt_verifier::{JwtVerifier, VerifiedClaims};
use token_client::TokenClient;

pub const PROTOCOL: &str = "messenger:msteams";

#[derive(Debug, Clone, Deserialize)]
pub struct SenderClaims {
    pub sub: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub tenant: String,
}

/// Config for the `azure` identity strategy (FR-I-7).
///
/// **Trust model.** The inbound Bot Framework JWT proves the request
/// came from Microsoft's connector (signature, `iss`, `aud`, `exp`
/// all verified before this config is consulted). It does NOT
/// cryptographically bind the per-user identity fields — those
/// (`from.aadObjectId`, `channelData.tenant.id`) ride in the request
/// body, not in the JWT claims. So the derived principal is
/// *connector-authenticated body metadata*, not a signed per-user
/// proof: a party holding a valid bearer for this bot could replay it
/// with a different body within the token's validity window. The
/// mitigations are (a) tokens never logged (FR-AU-3), (b) tailnet/
/// Fabio ingress restricted, (c) the `channelId == "msteams"` gate in
/// `dispatch_message`, and (d) the mandatory `allowed_tenants`
/// allowlist below.
///
/// `scopes` are adapter-granted roles (the channel JWT carries no
/// per-user OAuth scopes), not user-delegated OAuth scopes.
#[derive(Debug, Clone, Deserialize)]
pub struct AzureConfig {
    /// Entra tenant GUIDs accepted by this adapter. MUST be non-empty
    /// — `from_manifest` refuses to build otherwise (fail-closed
    /// cross-tenant isolation: an empty list would accept any tenant,
    /// which is not isolation).
    #[serde(default)]
    pub allowed_tenants: Vec<String>,
    /// Adapter-granted scopes for azure-authenticated senders.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// How this adapter resolves an inbound sender to a `Principal`.
enum IdentityMode {
    /// `from.id` (the AAD object id encoded as `29:...`) keyed into an
    /// operator-enumerated table.
    SenderTable(HashMap<String, SenderClaims>),
    /// Principal derived from the activity's Entra claims:
    /// `from.aadObjectId` → sub, `channelData.tenant.id` → tenant.
    Azure(AzureConfig),
}

/// Optional override hook for tests. Production builds use the
/// canonical Microsoft endpoints; the integration test points the
/// adapter at its `FakeBotFramework` axum app.
#[derive(Debug, Clone, Default)]
pub struct AdapterOverrides {
    pub openid_url: Option<String>,
    pub token_url: Option<String>,
    /// PR 37: additional `serviceUrl` hosts the JWT verifier should
    /// accept beyond Microsoft's documented suffixes. Production
    /// wiring leaves this empty; the binary refuses to populate it
    /// outside `local` env. Test fixtures pass `["127.0.0.1"]` (or
    /// the fake bot framework's host) so the integration tests can
    /// drive the adapter without minting `*.botframework.com` URLs.
    pub extra_service_url_hosts: Vec<String>,
}

pub struct MsTeamsAdapter {
    name: String,
    #[allow(dead_code)]
    audience: String,
    /// HMAC key signing/verifying the correlation tokens on Adaptive
    /// Card actions and the inbound callback (issue #155).
    correlation_key: Vec<u8>,
    identity: IdentityMode,
    /// Manifest `tool`: where plain inbound text dispatches (default
    /// `echo`). Commands (`/narrate` etc.) keep their special routes.
    inbound_tool: String,
    dispatcher: Arc<Dispatcher>,
    verifier: JwtVerifier,
    token_client: TokenClient,
    http: reqwest::Client,
    rate_limit: triton_core::ratelimit::TokenBucket,
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
}

impl MsTeamsAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        overrides: AdapterOverrides,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::MsTeams {
            return Err(BuildError::WrongKind);
        }
        if adapter.inbound.signature != SignatureScheme::BotFrameworkJwt {
            return Err(BuildError::Unsupported(format!(
                "msteams adapter requires `signature: bot_framework_jwt`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if adapter.outbound.kind != OutboundKind::BotConnector {
            return Err(BuildError::Unsupported(format!(
                "msteams adapter requires `outbound.kind: bot_connector`; got {:?}",
                adapter.outbound.kind
            )));
        }
        if !matches!(
            adapter.identity.kind,
            IdentityKind::SenderTable | IdentityKind::Azure
        ) {
            return Err(BuildError::Unsupported(format!(
                "msteams adapter supports `identity.kind: sender_table` or `azure`; got {:?}",
                adapter.identity.kind
            )));
        }

        let audience_field = adapter
            .inbound
            .credentials
            .get("audience")
            .ok_or(BuildError::MissingCredential("inbound.audience"))?;
        let audience = resolver
            .resolve(audience_field)
            .await
            .map_err(|e| BuildError::Resolve("inbound.audience", e))?;
        if audience.trim().is_empty() {
            return Err(BuildError::Unsupported(
                "inbound.audience must not be empty".into(),
            ));
        }

        let client_id_field = adapter
            .outbound
            .credentials
            .get("client_id")
            .ok_or(BuildError::MissingCredential("outbound.client_id"))?;
        let client_id = resolver
            .resolve(client_id_field)
            .await
            .map_err(|e| BuildError::Resolve("outbound.client_id", e))?;
        let client_secret_field = adapter
            .outbound
            .credentials
            .get("client_secret")
            .ok_or(BuildError::MissingCredential("outbound.client_secret"))?;
        let client_secret = resolver
            .resolve(client_secret_field)
            .await
            .map_err(|e| BuildError::Resolve("outbound.client_secret", e))?;

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
            IdentityKind::Azure => {
                let cfg_field = adapter
                    .identity
                    .credentials
                    .get("azure_identity")
                    .ok_or(BuildError::MissingCredential("identity.azure_identity"))?;
                let cfg_json = resolver
                    .resolve(cfg_field)
                    .await
                    .map_err(|e| BuildError::Resolve("identity.azure_identity", e))?;
                let cfg: AzureConfig = serde_json::from_str(&cfg_json)
                    .map_err(|e| BuildError::TableParse(e.to_string()))?;
                // Fail closed: an empty allowlist is not cross-tenant
                // isolation. A single-tenant deployment lists its one
                // tenant explicitly.
                if cfg.allowed_tenants.is_empty() {
                    return Err(BuildError::Unsupported(
                        "azure identity requires a non-empty `allowed_tenants` list \
                         (fail-closed cross-tenant isolation)"
                            .into(),
                    ));
                }
                IdentityMode::Azure(cfg)
            }
            // Guarded above; unreachable for other kinds.
            other => {
                return Err(BuildError::Unsupported(format!(
                    "msteams adapter supports `identity.kind: sender_table` or `azure`; got {other:?}"
                )));
            }
        };

        let correlation_key = resolver
            .resolve(&adapter.correlation_key)
            .await
            .map_err(|e| BuildError::Resolve("correlation_key", e))?
            .into_bytes();

        // Adapter-wide rate limit is the DoS floor (10x headroom
        // over per-tenant). Same rationale as Telegram/Discord —
        // see triton-chat-telegram for the long-form comment.
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

        let openid_url = overrides
            .openid_url
            .unwrap_or_else(|| jwt_verifier::DEFAULT_OPENID_URL.to_string());
        let verifier = JwtVerifier::new(openid_url, audience.clone())
            .with_extra_service_url_hosts(overrides.extra_service_url_hosts);
        let token_client = match overrides.token_url {
            Some(url) => TokenClient::with_token_url(client_id, client_secret, url),
            None => TokenClient::new(client_id, client_secret),
        };
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| BuildError::Unsupported(format!("courier http client: {e}")))?;

        Ok(Self {
            name: name.to_string(),
            audience,
            correlation_key,
            identity,
            inbound_tool: adapter.tool.clone(),
            dispatcher,
            verifier,
            token_client,
            http,
            rate_limit,
            per_tenant_limit,
        })
    }

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
    #[error("adapter is not declared `kind: ms_teams`")]
    WrongKind,
    #[error("msteams adapter limitation: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("could not resolve credential field `{0}`: {1}")]
    Resolve(&'static str, #[source] ResolveError),
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
}

#[derive(Debug, Deserialize)]
struct Activity {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    from: Option<ActivityFrom>,
    #[serde(default)]
    conversation: Option<ActivityConversation>,
    #[serde(default)]
    recipient: Option<ActivityRecipient>,
    #[serde(default, rename = "channelId")]
    channel_id: Option<String>,
    #[serde(default, rename = "channelData")]
    channel_data: Option<ChannelData>,
    /// Present on an `invoke` Activity (`adaptiveCard/action`) and on a
    /// `message` carrying an `Action.Submit` payload — the card's
    /// gathered inputs plus the action's `data` (which holds the signed
    /// correlation token). Issue #155.
    #[serde(default)]
    value: Option<Value>,
    /// The invoke name (`adaptiveCard/action` for a universal action).
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ActivityFrom {
    id: String,
    /// Entra (AAD) object id. Present on AAD-backed channels (Teams);
    /// the `azure` identity strategy derives `Principal.sub` from it.
    #[serde(default, rename = "aadObjectId")]
    aad_object_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChannelData {
    #[serde(default)]
    tenant: Option<ChannelTenant>,
}

#[derive(Debug, Deserialize)]
struct ChannelTenant {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ActivityConversation {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ActivityRecipient {
    id: String,
}

async fn handle_webhook(
    State(adapter): State<Arc<MsTeamsAdapter>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // FR-I-8 / M-SIG-1: JWT verification BEFORE body parse. We pull
    // the bearer out by hand (no helper crate) so a malformed
    // Authorization header lands in the same rejection path as a
    // bad signature, not at the axum extractor level.
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if bearer.is_empty() {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::Auth("missing or malformed Authorization bearer".into()),
        );
        return (StatusCode::UNAUTHORIZED, "missing bearer").into_response();
    }
    let verified = match adapter.verifier.verify(bearer).await {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Auth(format!("bot framework jwt: {e}")),
            );
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };

    // NFR-P-3 first tier: adapter-wide rate limit. Consumed AFTER
    // the JWT check so attackers can't waste tokens by spraying
    // bogus bearers, BEFORE body parse so noisy bots can't bypass
    // by varying `from.id`.
    if let Err(retry_after) = adapter.rate_limit.try_take() {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::RateLimited(format!(
                "msteams adapter `{}` rate limit hit; retry in {:.2}s",
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

    let activity: Activity = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Validation(format!("malformed activity body: {e}")),
            );
            return (StatusCode::BAD_REQUEST, "malformed activity").into_response();
        }
    };

    // Route by Activity type (issue #155):
    //   * `invoke` / `adaptiveCard/action` — an `Action.Execute`
    //     universal action. The signed token in `value.action.data`
    //     re-invokes a tool; the reply is a refreshed card returned in
    //     the HTTP response (in-place drill-down).
    //   * `message` with a `value` — an `Action.Submit` from a card.
    //     Same verify-and-route, but the reply is POSTed back as a new
    //     Activity.
    //   * `message` with `text` — a typed message (the text path).
    //   * anything else (conversationUpdate, typing, messageReaction,
    //     ...) — silently 200'd so the Bot Connector doesn't retry.
    //     Not auth-relevant: the JWT was already verified.
    match activity.kind.as_deref() {
        Some("invoke") if activity.name.as_deref() == Some("adaptiveCard/action") => {
            let value = activity.value.clone().unwrap_or(Value::Null);
            handle_callback(
                &adapter,
                &verified,
                &activity,
                &value,
                CallbackKind::Execute,
            )
            .await
        }
        Some("message") => {
            if let Some(value) = activity.value.clone() {
                return handle_callback(
                    &adapter,
                    &verified,
                    &activity,
                    &value,
                    CallbackKind::Submit,
                )
                .await;
            }
            let Some(text) = activity.text.as_ref().filter(|t| !t.is_empty()) else {
                return StatusCode::OK.into_response();
            };
            dispatch_message(&adapter, &verified, &activity, text).await
        }
        _ => StatusCode::OK.into_response(),
    }
}

/// Which card action produced an inbound callback — decides the reply
/// channel: `Execute` refreshes the card in the HTTP response, `Submit`
/// POSTs a new reply Activity.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CallbackKind {
    Execute,
    Submit,
}

/// Identity resolved off an inbound Activity: the channel-scoped
/// `from.id` (always the reply target) plus the `(sub, scopes, tenant)`
/// the sender maps to. Shared by the text-message and callback paths.
struct ResolvedSender {
    from_id: String,
    sub: String,
    scopes: Vec<String>,
    tenant: String,
}

/// FR-I-7 sender resolution. Returns the resolved sender or a ready
/// rejection `Response` (already audited). Identical semantics for a
/// typed message, an `Action.Submit`, and an `Action.Execute`.
// The `Err` is an axum `Response` (inherently large); boxing it would
// add an allocation on every rejection for no real benefit.
#[allow(clippy::result_large_err)]
fn resolve_sender(
    adapter: &Arc<MsTeamsAdapter>,
    activity: &Activity,
) -> Result<ResolvedSender, Response> {
    // The `from.id` carries the channel-scoped id (`29:...`); we always
    // need it as the outbound reply target, regardless of how identity
    // is resolved.
    let Some(from) = activity.from.as_ref() else {
        record_rejection(
            adapter,
            "-",
            "-",
            TritonError::Validation("activity missing from.id".into()),
        );
        return Err((StatusCode::BAD_REQUEST, "missing from.id").into_response());
    };

    let (sub, scopes, tenant) = match &adapter.identity {
        IdentityMode::SenderTable(table) => match table.get(&from.id) {
            Some(c) => (c.sub.clone(), c.scopes.clone(), c.tenant.clone()),
            None => {
                record_rejection(
                    adapter,
                    "-",
                    "-",
                    TritonError::Auth(format!("unknown sender {}", from.id)),
                );
                return Err((StatusCode::UNAUTHORIZED, "unknown sender").into_response());
            }
        },
        IdentityMode::Azure(cfg) => {
            // The AAD identity fields are unsigned body metadata,
            // trusted only because the request is connector-
            // authenticated AND arrived over the Teams channel. A
            // valid Bot Framework token for this bot on another
            // channel must NOT inject an Entra-shaped principal.
            if activity.channel_id.as_deref() != Some("msteams") {
                record_rejection(
                    adapter,
                    "-",
                    "-",
                    TritonError::Auth(format!(
                        "azure identity requires channelId=msteams; got {:?}",
                        activity.channel_id
                    )),
                );
                return Err((StatusCode::UNAUTHORIZED, "wrong channel").into_response());
            }
            // sub = from.aadObjectId. Refuse rather than fall back to
            // the channel id: a message with no AAD object id can't
            // yield an Entra principal.
            let Some(sub) = from.aad_object_id.as_ref().filter(|s| !s.is_empty()) else {
                record_rejection(
                    adapter,
                    "-",
                    "-",
                    TritonError::Auth(
                        "azure identity requires from.aadObjectId on the activity".into(),
                    ),
                );
                return Err((StatusCode::UNAUTHORIZED, "missing aadObjectId").into_response());
            };
            // tenant = channelData.tenant.id.
            let Some(tenant) = activity
                .channel_data
                .as_ref()
                .and_then(|c| c.tenant.as_ref())
                .map(|t| t.id.as_str())
                .filter(|s| !s.is_empty())
            else {
                record_rejection(
                    adapter,
                    sub,
                    "-",
                    TritonError::Auth(
                        "azure identity requires channelData.tenant.id on the activity".into(),
                    ),
                );
                return Err((StatusCode::UNAUTHORIZED, "missing tenant").into_response());
            };
            // Cross-tenant isolation: the inbound tenant MUST be on
            // the allowlist (guaranteed non-empty at build time).
            if !cfg.allowed_tenants.iter().any(|t| t == tenant) {
                record_rejection(
                    adapter,
                    sub,
                    tenant,
                    TritonError::Auth(format!("tenant {tenant} not on allowed_tenants")),
                );
                return Err((StatusCode::UNAUTHORIZED, "tenant not allowed").into_response());
            }
            (sub.clone(), cfg.scopes.clone(), tenant.to_string())
        }
    };

    Ok(ResolvedSender {
        from_id: from.id.clone(),
        sub,
        scopes,
        tenant,
    })
}

/// NFR-P-3 second tier: per-tenant fair-share. `Some(response)` is a
/// ready 429 (already audited); `None` means the token was taken.
fn check_tenant_limit(adapter: &Arc<MsTeamsAdapter>, sub: &str, tenant: &str) -> Option<Response> {
    if let Err(retry_after) = adapter.per_tenant_limit.try_take(tenant) {
        record_rejection(
            adapter,
            sub,
            tenant,
            TritonError::RateLimited(format!(
                "tenant `{}` rate limit hit on adapter `{}`; retry in {:.2}s",
                tenant, adapter.name, retry_after
            )),
        );
        let secs = retry_after.ceil().max(1.0) as u64;
        return Some(
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", secs.to_string())],
                "tenant rate limited",
            )
                .into_response(),
        );
    }
    None
}

fn make_principal(sub: &str, scopes: &[String], tenant: &str) -> Principal {
    Principal {
        sub: sub.to_string(),
        scopes: scopes.to_vec(),
        groups: Vec::new(),
        tenant: tenant.to_string(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    }
}

async fn dispatch_message(
    adapter: &Arc<MsTeamsAdapter>,
    verified: &VerifiedClaims,
    activity: &Activity,
    text: &str,
) -> Response {
    let sender = match resolve_sender(adapter, activity) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Some(resp) = check_tenant_limit(adapter, &sender.sub, &sender.tenant) {
        return resp;
    }
    let (conversation_id, recipient_id) = match convo_and_recipient(adapter, activity, &sender) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Strip the Teams `<at>@bot</at>` mention prefix the platform
    // wraps around mentions in group chats. The text after the
    // closing `</at>` (with whitespace trimmed) is what we route as
    // the command.
    let stripped = strip_mention_prefix(text);
    let (tool_name, args) = route_command(stripped, &adapter.inbound_tool);

    dispatch_and_post_reply(
        adapter,
        verified,
        &tool_name,
        args,
        &sender,
        &conversation_id,
        &recipient_id,
    )
    .await
}

/// Handle a card callback: verify the signed correlation token, merge
/// the user's card inputs onto the token's base args, and re-dispatch
/// the recovered `(tool, args)` with the resolved principal.
///
/// * `Execute` (universal action) returns the re-dispatched surface as
///   a refreshed Adaptive Card in the HTTP response (in-place refresh).
/// * `Submit` POSTs the reply back as a new Activity, like a message.
async fn handle_callback(
    adapter: &Arc<MsTeamsAdapter>,
    verified: &VerifiedClaims,
    activity: &Activity,
    value: &Value,
    kind: CallbackKind,
) -> Response {
    let sender = match resolve_sender(adapter, activity) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Some(resp) = check_tenant_limit(adapter, &sender.sub, &sender.tenant) {
        return resp;
    }

    // Pull the signed token + gathered card inputs out of the callback.
    let Some((token, inputs)) = extract_callback(value, kind) else {
        record_rejection(
            adapter,
            &sender.sub,
            &sender.tenant,
            TritonError::Validation("card callback missing correlation token".into()),
        );
        return (StatusCode::BAD_REQUEST, "missing action").into_response();
    };

    // Verify the HMAC BEFORE trusting the tool/args. A forged or
    // tampered token — even on an authenticated webhook — is refused
    // and audited as `error:auth`, never re-dispatched.
    let (tool_name, mut args) = match triton_correlation::decode_with_cap(
        &token,
        &adapter.correlation_key,
        surface_mapper::MSTEAMS_CORRELATION_CAP,
    ) {
        Ok(p) => p,
        Err(_) => {
            record_rejection(
                adapter,
                &sender.sub,
                &sender.tenant,
                TritonError::Auth("card callback correlation token invalid".into()),
            );
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };
    // Merge the user-supplied Selection/Form values onto the token's
    // signed base args. The token fixed the TOOL (and any preset
    // args); the inputs are user query params. Skip empty values —
    // Teams gathers EVERY input on the card with ANY action, so a
    // preset button also submits the (blank) sibling inputs; an empty
    // merge would clobber the button's own preset args.
    merge_inputs(&mut args, inputs);

    match kind {
        CallbackKind::Execute => {
            dispatch_and_refresh_card(adapter, &tool_name, args, &sender).await
        }
        CallbackKind::Submit => {
            let (conversation_id, recipient_id) =
                match convo_and_recipient(adapter, activity, &sender) {
                    Ok(v) => v,
                    Err(resp) => return resp,
                };
            dispatch_and_post_reply(
                adapter,
                verified,
                &tool_name,
                args,
                &sender,
                &conversation_id,
                &recipient_id,
            )
            .await
        }
    }
}

/// The conversation id + the inbound bot id (`recipient.id`) needed to
/// address an outbound reply Activity. `Err` is a ready 400 (audited).
#[allow(clippy::result_large_err)]
fn convo_and_recipient(
    adapter: &Arc<MsTeamsAdapter>,
    activity: &Activity,
    sender: &ResolvedSender,
) -> Result<(String, String), Response> {
    let Some(conversation) = activity.conversation.as_ref() else {
        record_rejection(
            adapter,
            &sender.sub,
            &sender.tenant,
            TritonError::Validation("activity missing conversation.id".into()),
        );
        return Err((StatusCode::BAD_REQUEST, "missing conversation.id").into_response());
    };
    let Some(recipient) = activity.recipient.as_ref() else {
        record_rejection(
            adapter,
            &sender.sub,
            &sender.tenant,
            TritonError::Validation("activity missing recipient.id".into()),
        );
        return Err((StatusCode::BAD_REQUEST, "missing recipient.id").into_response());
    };
    Ok((conversation.id.clone(), recipient.id.clone()))
}

/// Dispatch `(tool, args)` and POST the rendered reply back through the
/// bot connector. Used by the message and `Action.Submit` paths.
async fn dispatch_and_post_reply(
    adapter: &Arc<MsTeamsAdapter>,
    verified: &VerifiedClaims,
    tool_name: &str,
    args: Value,
    sender: &ResolvedSender,
    conversation_id: &str,
    recipient_id: &str,
) -> Response {
    let principal = make_principal(&sender.sub, &sender.scopes, &sender.tenant);
    let principal_for_post = principal.clone();
    let started = std::time::Instant::now();
    let result = adapter
        .dispatcher
        .invoke(tool_name, args, principal, PROTOCOL)
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(dispatch) => {
            // `recipient.id` is the bot (reply `from`); `from.id` is the
            // user (reply `recipient`).
            let body = build_reply_body(
                adapter,
                recipient_id,
                conversation_id,
                &sender.from_id,
                &dispatch.result,
            );
            post_reply(
                adapter,
                verified,
                tool_name,
                &principal_for_post,
                conversation_id,
                body,
                latency_ms,
            )
            .await;
            StatusCode::OK.into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "msteams dispatch failed");
            // Permanent app-layer failures get acked 200 so the Bot
            // Connector doesn't retry indefinitely; same pattern as
            // Telegram + Discord.
            StatusCode::OK.into_response()
        }
    }
}

/// Dispatch `(tool, args)` and return the rendered surface as a
/// refreshed Adaptive Card in the invoke HTTP response (the
/// `Action.Execute` in-place drill-down). No outbound POST.
async fn dispatch_and_refresh_card(
    adapter: &Arc<MsTeamsAdapter>,
    tool_name: &str,
    args: Value,
    sender: &ResolvedSender,
) -> Response {
    let principal = make_principal(&sender.sub, &sender.scopes, &sender.tenant);
    let principal_for_post = principal.clone();
    let started = std::time::Instant::now();
    let result = adapter
        .dispatcher
        .invoke(tool_name, args, principal, PROTOCOL)
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(dispatch) => {
            let response_body = match render_card_content(adapter, &dispatch.result) {
                Some(card) => surface_mapper::invoke_card_response(card),
                None => surface_mapper::invoke_message_response(
                    &text_reply_message(&dispatch.result).text,
                ),
            };
            // Audit the inline card reply as a successful post so the
            // pivot shows the callback produced a reply.
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                &principal_for_post,
                latency_ms,
                Ok((200, PostOutcome::Posted, None)),
            );
            (StatusCode::OK, axum::Json(response_body)).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "msteams callback dispatch failed");
            // Return a valid (empty) invoke response so the client
            // doesn't retry the universal action.
            (
                StatusCode::OK,
                axum::Json(surface_mapper::invoke_message_response("(no content)")),
            )
                .into_response()
        }
    }
}

/// Turn a dispatch result into an outbound reply Activity body: an
/// Adaptive Card when the surface carries interactive controls or a
/// dashboard, otherwise the plain-text Activity.
fn build_reply_body(
    adapter: &MsTeamsAdapter,
    bot_id: &str,
    conversation_id: &str,
    recipient_id: &str,
    result: &Value,
) -> Value {
    if let Some(card) = render_card_content(adapter, result) {
        surface_mapper::build_card_activity_body(bot_id, conversation_id, recipient_id, card)
    } else {
        let msg = text_reply_message(result);
        surface_mapper::build_activity_body(bot_id, conversation_id, recipient_id, &msg)
    }
}

/// Build the Adaptive Card `content` for a result, or `None` when the
/// surface has no interactive controls or dashboard (caller then sends
/// a plain-text reply). Each interactive control's `(tool, base_args)`
/// is signed here — the adapter holds the correlation key.
fn render_card_content(adapter: &MsTeamsAdapter, result: &Value) -> Option<Value> {
    let specs = surface_mapper::interactive_from_result(result);
    let dashboard = surface_mapper::dashboard_from_result(result);
    if specs.is_empty() && dashboard.is_none() {
        return None;
    }
    let text = match surface_mapper::try_render_surface(result) {
        Some(Ok(r)) => r.text,
        _ => String::new(),
    };
    let signed: Vec<(surface_mapper::InteractiveSpec, String)> = specs
        .into_iter()
        .filter_map(|spec| {
            match triton_correlation::encode_with_cap(
                spec.tool(),
                &spec.base_args(),
                &adapter.correlation_key,
                surface_mapper::MSTEAMS_CORRELATION_CAP,
            ) {
                Ok(token) => Some((spec, token)),
                Err(e) => {
                    tracing::warn!(
                        tool = spec.tool(),
                        error = %e,
                        "msteams: dropping interactive control (correlation token too large)"
                    );
                    None
                }
            }
        })
        .collect();
    // Every interactive control dropped and no dashboard → nothing to
    // put on a card; fall back to text.
    if signed.is_empty() && dashboard.is_none() {
        return None;
    }
    Some(surface_mapper::build_adaptive_card(
        &text,
        dashboard.as_ref(),
        &signed,
    ))
}

/// Render a non-interactive result to the plain-text [`RenderedMessage`]
/// (surface text, empty-surface sentinel, or clamped bare text).
fn text_reply_message(result: &Value) -> RenderedMessage {
    match surface_mapper::try_render_surface(result) {
        Some(Ok(r)) => r,
        Some(Err(_)) => RenderedMessage::text_only("(no content)".to_string()),
        None => RenderedMessage::text_only(surface_mapper::clamp_plain_text(&bare_text(result))),
    }
}

/// Pull the signed correlation token and the gathered card inputs out
/// of a callback `value`. For `Execute` the payload sits under
/// `value.action.data`; for `Submit` it is the `value` object itself.
/// The `ct` key is the token; every other scalar is a user input.
fn extract_callback(value: &Value, kind: CallbackKind) -> Option<(String, Vec<(String, String)>)> {
    let data = match kind {
        CallbackKind::Execute => value.get("action").and_then(|a| a.get("data"))?,
        CallbackKind::Submit => value,
    };
    let obj = data.as_object()?;
    let token = obj
        .get(surface_mapper::TOKEN_DATA_KEY)?
        .as_str()?
        .to_string();
    let inputs = obj
        .iter()
        .filter(|(k, _)| k.as_str() != surface_mapper::TOKEN_DATA_KEY)
        .filter_map(|(k, v)| input_as_string(v).map(|s| (k.clone(), s)))
        .collect();
    Some((token, inputs))
}

/// Coerce an Adaptive Card input value to a string arg. Text/choice
/// inputs arrive as strings, numbers as JSON numbers, toggles as
/// bools; anything else is ignored.
fn input_as_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Merge non-empty card inputs onto the token's signed base args.
fn merge_inputs(args: &mut Value, inputs: Vec<(String, String)>) {
    let non_empty: Vec<(String, String)> =
        inputs.into_iter().filter(|(_, v)| !v.is_empty()).collect();
    if non_empty.is_empty() {
        return;
    }
    let map = match args {
        Value::Object(m) => m,
        other => {
            *other = Value::Object(Default::default());
            other.as_object_mut().unwrap()
        }
    };
    for (k, v) in non_empty {
        map.insert(k, Value::String(v));
    }
}

/// `text` cleaned of Teams' `<at>@bot</at>` mention prefix.
fn strip_mention_prefix(text: &str) -> &str {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("<at>")
        && let Some(close_idx) = rest.find("</at>")
    {
        return rest[close_idx + "</at>".len()..].trim_start();
    }
    trimmed
}

fn route_command(text: &str, default_tool: &str) -> (String, Value) {
    if let Some(rest) = text.strip_prefix('/') {
        let (tool, subject) = rest.split_once(' ').unwrap_or((rest, ""));
        match tool {
            "narrate" => {
                return (
                    "narrate".to_string(),
                    serde_json::json!({ "subject": subject }),
                );
            }
            "echo" => {
                return (
                    "echo".to_string(),
                    serde_json::json!({ "message": subject }),
                );
            }
            _ => {}
        }
    }
    (
        default_tool.to_string(),
        serde_json::json!({ "message": text }),
    )
}

async fn post_reply(
    adapter: &MsTeamsAdapter,
    verified: &VerifiedClaims,
    tool_name: &str,
    principal: &Principal,
    conversation_id: &str,
    body: Value,
    dispatch_latency_ms: u64,
) {
    // The serviceUrl came from inside a JWT we verified — it's
    // platform-asserted (NFR-S-4 "trusted-by-derivation"). We
    // build the activities URL by joining serviceUrl + the
    // conversation path; the connector documents serviceUrl as
    // ending with a trailing slash but we tolerate either.
    let base = verified.service_url.trim_end_matches('/');
    let url = format!("{}/v3/conversations/{}/activities", base, conversation_id);

    let post_started = std::time::Instant::now();
    let access_token = match adapter.token_client.access_token().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("msteams token fetch failed: {e}");
            let provider = TritonError::Provider(format!("msteams token: {e}"));
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                dispatch_latency_ms + post_started.elapsed().as_millis() as u64,
                Err((&provider, 0, PostOutcome::Retry, None)),
            );
            return;
        }
    };
    let resp = adapter
        .http
        .post(&url)
        .bearer_auth(&access_token)
        .json(&body)
        .send()
        .await;
    let latency_ms = post_started.elapsed().as_millis() as u64;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            if (200..300).contains(&status) {
                adapter.dispatcher.record_post(
                    tool_name,
                    PROTOCOL,
                    principal,
                    latency_ms,
                    Ok((status, PostOutcome::Posted, None)),
                );
            } else {
                let label = if status >= 500 || status == 429 {
                    PostOutcome::Retry
                } else {
                    PostOutcome::Dropped
                };
                let provider =
                    TritonError::Provider(format!("msteams activities POST status {status}"));
                adapter.dispatcher.record_post(
                    tool_name,
                    PROTOCOL,
                    principal,
                    latency_ms,
                    Err((&provider, status, label, None)),
                );
            }
        }
        Err(e) => {
            tracing::warn!("msteams activities POST failed: {e}");
            let provider = TritonError::Provider(format!("msteams transport: {e}"));
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Err((&provider, 0, PostOutcome::Retry, None)),
            );
        }
    }
}

fn bare_text(v: &Value) -> String {
    if let Some(obj) = v.as_object()
        && obj.len() == 1
        && let Some(s) = obj.values().next().and_then(|v| v.as_str())
    {
        return s.to_string();
    }
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    serde_json::to_string(v).unwrap_or_else(|_| "<unrenderable>".to_string())
}

fn record_rejection(adapter: &MsTeamsAdapter, sub: &str, tenant: &str, e: TritonError) {
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

    #[test]
    fn strip_mention_prefix_removes_at_wrap() {
        assert_eq!(strip_mention_prefix("<at>@bot</at> hello"), "hello");
        assert_eq!(
            strip_mention_prefix("<at>@bot</at> /echo hi there"),
            "/echo hi there"
        );
        // No prefix: pass through unchanged.
        assert_eq!(strip_mention_prefix("plain message"), "plain message");
        // Leading whitespace tolerated.
        assert_eq!(strip_mention_prefix("   <at>@b</at>  hi"), "hi");
    }

    #[test]
    fn route_command_echo_default_and_explicit() {
        let (t, args) = route_command("hello world", "echo");
        assert_eq!(t, "echo");
        assert_eq!(args["message"], "hello world");
        let (t, args) = route_command("/echo only this", "echo");
        assert_eq!(t, "echo");
        assert_eq!(args["message"], "only this");
        // The plain-text fallback honours the configured tool.
        let (t, _) = route_command("hello world", "assistant");
        assert_eq!(t, "assistant");
    }
}
