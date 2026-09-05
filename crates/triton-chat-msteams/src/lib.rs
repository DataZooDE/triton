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
    /// Bot Framework `channelId` values permitted to assert an
    /// Entra-shaped principal. Defaults to `["msteams"]`, which is
    /// exactly the behaviour before this field existed — an operator
    /// who says nothing keeps a Teams-only adapter.
    ///
    /// The gate itself is NOT optional and is not a formality: the AAD
    /// fields are unsigned body metadata, trusted only because the
    /// request is connector-authenticated AND arrived over a channel
    /// this deployment chose to trust. A valid Bot Framework token for
    /// this bot on some other channel must not inject an Entra-shaped
    /// principal. Making the set explicit is what lets one deployment
    /// serve Copilot Studio (`pva`), WebChat or M365 Copilot Chat
    /// without deleting the gate.
    ///
    /// Not derived from JWKS `endorsements` — Microsoft's own binding
    /// for `channelId` — because endorsements exist only on the Bot
    /// Framework keyset, and a single-tenant bot (the type Microsoft
    /// now requires for new registrations) is signed by the Entra
    /// anchor, which publishes none.
    #[serde(default = "default_allowed_channel_ids")]
    pub allowed_channel_ids: Vec<String>,
}

fn default_allowed_channel_ids() -> Vec<String> {
    vec!["msteams".to_string()]
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

/// Async-courier switch (mirrors googlechat's; #635 P4). No `api_base`:
/// Teams replies go to the JWT-asserted `serviceUrl`, never a
/// configurable host.
#[derive(Debug, Clone, Default)]
pub struct CourierConfig {
    /// `true` ⇒ the webhook acks 200 immediately and a spawned task
    /// dispatches + delivers out-of-band (streaming in 1:1 chats,
    /// typing loop + proactive message elsewhere). `false` (the
    /// derived default) ⇒ the historical inline path, unchanged.
    pub enabled: bool,
    /// When set, courier tasks are spawned on this tracker so the host
    /// can drain in-flight deliveries on shutdown (#635 follow-up).
    /// `None` keeps the detached-spawn behaviour.
    pub tracker: Option<tokio_util::task::TaskTracker>,
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
    courier: CourierConfig,
    /// triton#247: when true, this adapter also serves the canonical
    /// `POST /api/messages` Bot Framework path (in addition to
    /// `/{name}/webhook`), making it the host-agnostic Activity ingress
    /// an Azure Bot points at. Opt-in via `inbound.canonical_path` and
    /// single-claimant — the path is fixed and `Router::merge` panics on
    /// an overlap, so triton-bin refuses a second claimant.
    canonical_path: bool,
}

impl MsTeamsAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        overrides: AdapterOverrides,
        courier: CourierConfig,
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
        // Exactly one outbound credential mode. `client_secret` is
        // the original; `federated_token_file` (+ `tenant_id`) is the
        // no-static-secret path for a pod holding an Entra federated
        // credential. Declaring both is a configuration error worth
        // failing on rather than silently preferring one — an
        // operator who set both does not know which is in force.
        let secret_field = adapter.outbound.credentials.get("client_secret");
        let federated_field = adapter.outbound.credentials.get("federated_token_file");
        let outbound_credential = match (secret_field, federated_field) {
            (Some(_), Some(_)) => {
                return Err(BuildError::Unsupported(
                    "outbound.client_secret and outbound.federated_token_file are mutually \
                     exclusive; declare exactly one"
                        .into(),
                ));
            }
            (None, None) => {
                return Err(BuildError::MissingCredential(
                    "outbound.client_secret or outbound.federated_token_file",
                ));
            }
            (Some(field), None) => {
                let secret = resolver
                    .resolve(field)
                    .await
                    .map_err(|e| BuildError::Resolve("outbound.client_secret", e))?;
                OutboundCredential::Secret(secret)
            }
            (None, Some(field)) => {
                let token_file = resolver
                    .resolve(field)
                    .await
                    .map_err(|e| BuildError::Resolve("outbound.federated_token_file", e))?;
                let tenant_field = adapter
                    .outbound
                    .credentials
                    .get("tenant_id")
                    .ok_or(BuildError::MissingCredential("outbound.tenant_id"))?;
                let tenant_id = resolver
                    .resolve(tenant_field)
                    .await
                    .map_err(|e| BuildError::Resolve("outbound.tenant_id", e))?;
                OutboundCredential::Federated {
                    token_file,
                    tenant_id,
                }
            }
        };

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
        let mut verifier = JwtVerifier::new(openid_url, audience.clone())
            .with_extra_service_url_hosts(overrides.extra_service_url_hosts);

        // OPTIONAL `inbound.credentials.tenant_id`: the bot's HOME tenant,
        // present only for a SINGLE-TENANT registration
        // (`MsaAppType: SingleTenant`). Those inbound tokens are signed by
        // Entra with that tenant's keys, not by Bot Framework, so without
        // this the verifier rejects every request with a 401 that looks
        // exactly like a misconfigured bot. Adding it is additive — the
        // Bot Framework anchor stays, so one adapter serves both kinds.
        //
        // Deliberately NOT derived from `identity.azure_identity`'s
        // `allowed_tenants`: those are the tenants a SENDER may belong to,
        // which is a different question from which tenant SIGNED the
        // token, and conflating them would silently widen one of the two.
        if let Some(field) = adapter.inbound.credentials.get("tenant_id") {
            let tenant = resolver
                .resolve(field)
                .await
                .map_err(|e| BuildError::Resolve("inbound.tenant_id", e))?;
            if !tenant.trim().is_empty() {
                verifier = verifier
                    .with_single_tenant(&tenant)
                    .map_err(|e| BuildError::Unsupported(e.to_string()))?;
            }
        }
        let token_client = match (outbound_credential, overrides.token_url) {
            (OutboundCredential::Secret(secret), Some(url)) => {
                TokenClient::with_token_url(client_id, secret, url)
            }
            (OutboundCredential::Secret(secret), None) => TokenClient::new(client_id, secret),
            (OutboundCredential::Federated { token_file, .. }, Some(url)) => {
                TokenClient::with_federated_token_url(client_id, token_file, url)
            }
            (
                OutboundCredential::Federated {
                    token_file,
                    tenant_id,
                },
                None,
            ) => TokenClient::with_federated_credential(client_id, &tenant_id, token_file)
                .map_err(|e| BuildError::Unsupported(e.to_string()))?,
        };
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| BuildError::Unsupported(format!("courier http client: {e}")))?;

        // triton#247: opt-in canonical Bot Framework messaging path.
        // A plain config toggle (not a secret) that happens to ride the
        // flattened inbound-credentials map alongside `audience`.
        let canonical_path = match adapter.inbound.credentials.get("canonical_path") {
            Some(field) => {
                let raw = resolver
                    .resolve(field)
                    .await
                    .map_err(|e| BuildError::Resolve("inbound.canonical_path", e))?;
                matches!(raw.trim(), "true" | "1" | "yes")
            }
            None => false,
        };

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
            courier,
            canonical_path,
        })
    }

    /// Whether this adapter claims the canonical `/api/messages` path
    /// (triton#247). The host merges adapter routers, and the path is
    /// fixed, so the host must ensure at most one claimant.
    pub fn canonical_path(&self) -> bool {
        self.canonical_path
    }

    pub fn router(self: Arc<Self>) -> Router {
        let name = self.name.clone();
        let path = format!("/{name}/webhook");
        let img_path = format!("/{name}/img/{{token}}");
        let mut router = Router::new().route(&path, post(handle_webhook));
        // triton#247: opt-in canonical Bot Framework messaging path. The
        // adapter is the host-agnostic Activity ingress, so an Azure Bot
        // (Teams, M365 Copilot Chat, WebChat, Copilot Studio channels)
        // POSTs to /api/messages; `handle_webhook` is transport-generic
        // (reads the Authorization header + raw body, not the route name).
        // Mounted only when opted in: the path is fixed and axum's
        // `Router::merge` panics on an overlapping route, so at most one
        // adapter may claim it (triton-bin enforces this with a named
        // error rather than the panic).
        if self.canonical_path {
            router = router.route("/api/messages", post(handle_webhook));
        }
        router
            // Signed chart-image route: Teams fetches card images by URL,
            // so the rendered PNG must be publicly addressable — the HMAC
            // token (content hash under the correlation key) is the whole
            // auth story, same as googlechat's.
            .route(&img_path, axum::routing::get(serve_report_png))
            .with_state(self)
    }
}

/// Which outbound credential the manifest declared. Resolved before
/// the token client is built so the mutually-exclusive check reads
/// as one decision rather than being spread across construction.
enum OutboundCredential {
    Secret(String),
    Federated {
        token_file: String,
        tenant_id: String,
    },
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
    /// The reply target, as asserted by the REQUEST BODY.
    ///
    /// Only consulted for a single-tenant bot, whose Entra token carries
    /// no signed `serviceUrl`. Unsigned, therefore checked against the
    /// host allowlist before anything is POSTed to it.
    #[serde(default, rename = "serviceUrl")]
    service_url: Option<String>,
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
    /// `"personal"` / `"groupChat"` / `"channel"` on Teams. Streaming
    /// is legal ONLY in personal (1:1) chats, so absent/unknown is
    /// treated as a group — the conservative branch (typing loop +
    /// plain proactive message), which every conversation kind accepts.
    #[serde(default, rename = "conversationType")]
    conversation_type: Option<String>,
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
            // Log the REASON as well as auditing the rejection. The audit
            // record renders `result: error:auth` and drops the message,
            // so a wrong issuer, an unknown kid and an untrusted
            // serviceUrl were indistinguishable in production — on
            // 2026-08-29 that turned a one-line config bug into a
            // log-reading expedition.
            tracing::warn!(error = %e, "msteams inbound jwt rejected");
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

    // Resolve the reply target ONCE, here, so every downstream path sees
    // a value that has already been checked.
    //
    // Multi-tenant: the token carried a SIGNED serviceUrl and the
    // verifier already allowlisted it. Single-tenant: Entra's token has
    // no such claim, so it comes from the Activity BODY — unsigned — and
    // the host allowlist is the only thing preventing a caller from
    // aiming our reply (bearing a real Bot Connector token) at a host
    // they control. Refuse rather than fall back to anything.
    let verified = match verified.service_url.clone() {
        Some(signed) => VerifiedClaims {
            service_url: Some(signed),
        },
        None => match activity.service_url.as_deref() {
            Some(from_body) if adapter.verifier.service_url_allowed(from_body) => VerifiedClaims {
                service_url: Some(from_body.to_string()),
            },
            Some(bad) => {
                record_rejection(
                    &adapter,
                    "-",
                    "-",
                    TritonError::Auth(format!(
                        "activity serviceUrl `{bad}` is not a documented Microsoft host"
                    )),
                );
                return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
            }
            None => {
                record_rejection(
                    &adapter,
                    "-",
                    "-",
                    TritonError::Validation(
                        "no serviceUrl: absent from both the token and the activity body, \
                         so there is nowhere to send a reply"
                            .into(),
                    ),
                );
                return (StatusCode::BAD_REQUEST, "missing serviceUrl").into_response();
            }
        },
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
#[derive(Clone)]
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
            // authenticated AND arrived over a channel this deployment
            // declared. A valid Bot Framework token for this bot on any
            // other channel must NOT inject an Entra-shaped principal.
            // An absent or empty `channelId` matches nothing and is
            // therefore refused, never treated as a wildcard.
            let channel = activity.channel_id.as_deref().unwrap_or_default();
            if !cfg.allowed_channel_ids.iter().any(|c| c == channel) {
                record_rejection(
                    adapter,
                    "-",
                    "-",
                    TritonError::Auth(format!(
                        "channelId {channel:?} is not on this adapter's \
                         allowed_channel_ids {:?}",
                        cfg.allowed_channel_ids
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

/// `sender_ref` is the RAW platform id the principal was derived from
/// (Teams `from.id` / `aadObjectId`), recorded in the audit line beside
/// the resolved subject — see [`Principal::sender_ref`] (#250).
fn make_principal_with_sender(
    sub: &str,
    scopes: &[String],
    tenant: &str,
    sender_ref: Option<&str>,
) -> Principal {
    Principal {
        sender_ref: sender_ref.map(str::to_owned),
        ..make_principal(sub, scopes, tenant)
    }
}

fn make_principal(sub: &str, scopes: &[String], tenant: &str) -> Principal {
    Principal {
        sub: sub.to_string(),
        scopes: scopes.to_vec(),
        groups: Vec::new(),
        tenant: tenant.to_string(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
        sender_ref: None,
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

    let conversation_type = activity
        .conversation
        .as_ref()
        .and_then(|c| c.conversation_type.clone());
    dispatch_and_post_reply(
        adapter,
        verified,
        &tool_name,
        args,
        &sender,
        &conversation_id,
        &recipient_id,
        conversation_type,
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
    // #250: verified against the SENDER's tenant, so a token minted into
    // another tenant's conversation cannot be replayed here.
    let (tool_name, mut args) = match triton_correlation::decode_bound(
        &token,
        &adapter.correlation_key,
        surface_mapper::MSTEAMS_CORRELATION_CAP,
        &sender.tenant,
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

    let conversation_type = activity
        .conversation
        .as_ref()
        .and_then(|c| c.conversation_type.clone());
    match kind {
        CallbackKind::Execute => {
            // A data-question button click runs the same 20-25s dispatch
            // a typed question does, and the in-place card refresh MUST
            // ride the invoke's own HTTP response — which Bot Framework
            // abandons at ~15s, killing the refresh AND the dispatch
            // with it (the future is dropped). With the courier on, ack
            // the invoke immediately (the Universal Action contract
            // requires a response body) and deliver the real answer
            // out-of-band as a new message.
            // (When convo_and_recipient fails — defensive; a real Teams
            // invoke always carries a conversation — fall through to the
            // inline refresh, which still works.)
            if adapter.courier.enabled
                && let Ok((conversation_id, recipient_id)) =
                    convo_and_recipient(adapter, activity, &sender)
            {
                spawn_courier(
                    adapter,
                    verified,
                    tool_name,
                    args,
                    sender,
                    conversation_id,
                    recipient_id,
                    conversation_type,
                );
                return (
                    StatusCode::OK,
                    axum::Json(surface_mapper::invoke_message_response(
                        "⏳ Working on it — the answer will follow here.",
                    )),
                )
                    .into_response();
            }
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
                conversation_type,
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
#[allow(clippy::too_many_arguments)]
async fn dispatch_and_post_reply(
    adapter: &Arc<MsTeamsAdapter>,
    verified: &VerifiedClaims,
    tool_name: &str,
    args: Value,
    sender: &ResolvedSender,
    conversation_id: &str,
    recipient_id: &str,
    conversation_type: Option<String>,
) -> Response {
    // The courier seam: everything security-relevant (JWT verify, rate
    // limits, sender resolution, correlation-token check on Submit)
    // already happened in the caller. Ack 200 NOW — Bot Framework
    // abandons the connection at ~15s and hyper then DROPS this future,
    // which on the inline path killed the reply after the dispatch had
    // already succeeded (observed live: `dispatch ok 23.4s`, no post
    // record, ingress 499). A spawned task cannot be cancelled by the
    // client hanging up.
    if adapter.courier.enabled {
        spawn_courier(
            adapter,
            verified,
            tool_name.to_string(),
            args,
            sender.clone(),
            conversation_id.to_string(),
            recipient_id.to_string(),
            conversation_type,
        );
        return StatusCode::OK.into_response();
    }
    let principal = make_principal_with_sender(
        &sender.sub,
        &sender.scopes,
        &sender.tenant,
        Some(&sender.from_id),
    );
    let principal_for_post = principal.clone();
    // Direct render_report (the "Open report:" Execute): the chart URL
    // is minted from the INVOKED args — the result carries only a PNG.
    let image_hint = (tool_name == "render_report")
        .then(|| report_image_url(adapter, &args, &sender.tenant))
        .flatten();
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
            let chrome = fetch_chrome(adapter, &principal_for_post).await;
            let body = build_reply_body(
                adapter,
                recipient_id,
                conversation_id,
                &sender.from_id,
                &dispatch.result,
                image_hint,
                &chrome,
                &sender.tenant,
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
    let principal = make_principal_with_sender(
        &sender.sub,
        &sender.scopes,
        &sender.tenant,
        Some(&sender.from_id),
    );
    let principal_for_post = principal.clone();
    let image_hint = (tool_name == "render_report")
        .then(|| report_image_url(adapter, &args, &sender.tenant))
        .flatten();
    let started = std::time::Instant::now();
    let result = adapter
        .dispatcher
        .invoke(tool_name, args, principal, PROTOCOL)
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(dispatch) => {
            // The refresh path renders the chart too — the invoke card
            // may carry the image URL like any other reply card.
            let image_url =
                image_hint.or_else(|| reply_image_url(adapter, &dispatch.result, &sender.tenant));
            let chrome = fetch_chrome(adapter, &principal_for_post).await;
            let response_body = match render_card_content(
                adapter,
                &dispatch.result,
                image_url.as_deref(),
                &chrome,
                &sender.tenant,
            ) {
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

/// The card chrome (branded header) for this reply, from the report
/// upstream's `get_theme`. Peacock owns ALL theming — one CSS of `--pk-*`
/// tokens themes the chart PNG, the iframe AND this chrome — so the
/// adapter holds no theme config and asks per reply.
///
/// A deployment that registers no `get_theme` upstream gets the default
/// (unbranded) chrome and the card it always had; the failure is
/// debug-logged, never surfaced. Themes are resolved against the
/// principal's tenant on peacock's side, so the call carries the real
/// principal rather than a synthetic one.
async fn fetch_chrome(
    adapter: &MsTeamsAdapter,
    principal: &Principal,
) -> surface_mapper::CardChrome {
    match adapter
        .dispatcher
        .invoke(
            "get_theme",
            serde_json::json!({}),
            principal.clone(),
            PROTOCOL,
        )
        .await
    {
        Ok(t) => surface_mapper::CardChrome::from_get_theme(&t.result),
        Err(e) => {
            tracing::debug!(error = %e, "msteams: no get_theme upstream; unbranded card");
            surface_mapper::CardChrome::default()
        }
    }
}

/// How long a card action token stays clickable (#250).
///
/// Unbound tokens never expired, which with an 8-byte truncated HMAC
/// makes each one a permanent oracle until the correlation key rotates.
/// A week matches the chart-image links: long enough that a user
/// scrolling recent history still gets a working button, short enough
/// that an old card is not a forever-capability.
const CARD_TOKEN_TTL_SECS: u64 = 7 * 24 * 3600;

/// Marker `tool` slot of the signed chart-image tokens — namespaced
/// away from card-action tokens even under one key.
const RENDER_REPORT_IMG_MARKER: &str = "__msteams_report_img";
/// Image tokens carry the render_report args (report id + params) and an
/// expiry; the cap bounds hostile inputs before any HMAC work.
const IMG_TOKEN_CAP: usize = 1024;
/// Signed image URLs stay fetchable this long. Teams fetches within
/// seconds of the card landing, but a user scrolling old history days
/// later still gets the chart — while an ancient link eventually dies
/// instead of being a forever-capability.
const IMG_TOKEN_TTL_SECS: u64 = 7 * 24 * 3600;

/// The public HTTPS base chart-image URLs are minted under. Env-only
/// (`TRITON_MSTEAMS_PUBLIC_BASE`): the courier task owns no request
/// headers, and Teams requires a host IT can reach, not one we saw.
fn public_base() -> Option<String> {
    let base = std::env::var("TRITON_MSTEAMS_PUBLIC_BASE").ok()?;
    let base = base.trim().trim_end_matches('/').to_string();
    (!base.is_empty()).then_some(base)
}

/// Extract a rendered chart PNG from a `render_report` result
/// (peacock carries it as `png_base64`, searched anywhere for nesting
/// robustness).
fn upstream_png_bytes(result: &Value) -> Option<Vec<u8>> {
    fn find(v: &Value) -> Option<&str> {
        match v {
            Value::Object(m) => m
                .get("png_base64")
                .and_then(Value::as_str)
                .or_else(|| m.values().find_map(find)),
            Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    use base64::Engine as _;
    let b64 = find(result)?;
    base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .ok()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint the signed chart-image URL for one report spec. STATELESS by
/// design: the token carries the `render_report` args themselves (plus
/// an expiry), and the route re-renders on fetch — so any replica can
/// serve it. The first cut cached PNG bytes in process memory, and with
/// `replicaCount: 2` the fetch landed on the other pod's empty cache:
/// a 404 that looked exactly like a broken chart (#635, live).
fn report_image_url(adapter: &MsTeamsAdapter, rargs: &Value, tenant: &str) -> Option<String> {
    let base = public_base()?;
    // `t`: the tenant this link was minted for. Peacock keys `brand` off
    // the caller's tenant, and Triton forwards `principal.tenant` into
    // the JWT it mints for an upstream — so without it the chart PNG
    // resolves a DIFFERENT brand from the card chrome wrapped around it
    // (#200), and the render orphans from that tenant's audit trail. It
    // rides inside the HMAC-signed payload, so it is exactly as
    // trustworthy as the args beside it.
    let payload = serde_json::json!({
        "a": rargs, "exp": now_secs() + IMG_TOKEN_TTL_SECS, "t": tenant,
    });
    let token = triton_correlation::encode_with_cap(
        RENDER_REPORT_IMG_MARKER,
        &payload,
        &adapter.correlation_key,
        IMG_TOKEN_CAP,
    )
    .ok()?;
    Some(format!("{base}/{}/img/{token}", adapter.name))
}

/// The chart-image URL for this reply, when its surface carries an
/// inline `Report` component. No render happens at reply time — the
/// URL is a signed promise the img route fulfils on fetch.
fn reply_image_url(adapter: &MsTeamsAdapter, result: &Value, tenant: &str) -> Option<String> {
    let (report_id, args) = surface_mapper::report_from_result(result)?;
    let mut rargs = if args.is_object() {
        args
    } else {
        serde_json::json!({})
    };
    rargs["report_id"] = serde_json::json!(report_id);
    report_image_url(adapter, &rargs, tenant)
}

async fn serve_report_png(
    State(adapter): State<Arc<MsTeamsAdapter>>,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    let Ok((marker, payload)) =
        triton_correlation::decode_with_cap(&token, &adapter.correlation_key, IMG_TOKEN_CAP)
    else {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    };
    if marker != RENDER_REPORT_IMG_MARKER {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    if payload
        .get("exp")
        .and_then(Value::as_u64)
        .is_none_or(|exp| exp < now_secs())
    {
        return (StatusCode::GONE, "image link expired").into_response();
    }
    let Some(rargs) = payload.get("a").cloned() else {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    };
    // Render on fetch. The principal does not AUTHORIZE anything — the
    // signed token (args fixed at mint time) does that, and the render is
    // a read through peacock's own standing escurel identity. It carries
    // the tenant the link was minted for so the chart resolves the same
    // peacock brand as the card chrome around it, and so the render joins
    // that tenant's audit trail. The subject stays synthetic: a
    // long-lived URL should not embed a user id, and the audit line
    // should say plainly that this came from the image route.
    // A token minted before `t` existed (they live 7 days) has no tenant
    // and keeps the old placeholder rather than failing the fetch.
    let tenant = payload.get("t").and_then(Value::as_str).unwrap_or("-");
    let principal = make_principal("msteams-img", &["chat".to_string()], tenant);
    match adapter
        .dispatcher
        .invoke("render_report", rargs, principal, PROTOCOL)
        .await
    {
        Ok(rep) => match upstream_png_bytes(&rep.result) {
            Some(png) => (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "image/png")],
                png,
            )
                .into_response(),
            None => (StatusCode::BAD_GATEWAY, "render produced no image").into_response(),
        },
        Err(e) => {
            tracing::warn!(error = %e, "msteams img route: render_report failed");
            (StatusCode::BAD_GATEWAY, "render failed").into_response()
        }
    }
}

/// Turn a dispatch result into an outbound reply Activity body: an
/// Adaptive Card when the surface carries interactive controls or a
/// dashboard, otherwise the plain-text Activity.
#[allow(clippy::too_many_arguments)]
fn build_reply_body(
    adapter: &MsTeamsAdapter,
    bot_id: &str,
    conversation_id: &str,
    recipient_id: &str,
    result: &Value,
    image_hint: Option<String>,
    chrome: &surface_mapper::CardChrome,
    tenant: &str,
) -> Value {
    // `image_hint` covers the direct render_report invocation (the
    // "Open report:" Execute), whose RESULT carries a PNG but no Report
    // component to lift a spec from — the caller minted the URL from
    // the invoked args instead.
    let image_url = image_hint.or_else(|| reply_image_url(adapter, result, tenant));
    if let Some(card) = render_card_content(adapter, result, image_url.as_deref(), chrome, tenant) {
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
fn render_card_content(
    adapter: &MsTeamsAdapter,
    result: &Value,
    image_url: Option<&str>,
    chrome: &surface_mapper::CardChrome,
    tenant: &str,
) -> Option<Value> {
    let specs = surface_mapper::interactive_from_result(result);
    let dashboard = surface_mapper::dashboard_from_result(result);
    if specs.is_empty() && dashboard.is_none() && image_url.is_none() {
        return None;
    }
    let text = match surface_mapper::try_render_surface(result) {
        Some(Ok(r)) => r.text,
        _ => String::new(),
    };
    let signed: Vec<(surface_mapper::InteractiveSpec, String)> = specs
        .into_iter()
        .filter_map(|spec| {
            match triton_correlation::encode_bound(
                spec.tool(),
                &spec.base_args(),
                &adapter.correlation_key,
                surface_mapper::MSTEAMS_CORRELATION_CAP,
                tenant,
                CARD_TOKEN_TTL_SECS,
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
    // Every interactive control dropped, no dashboard, no image →
    // nothing to put on a card; fall back to text.
    if signed.is_empty() && dashboard.is_none() && image_url.is_none() {
        return None;
    }
    Some(surface_mapper::build_adaptive_card(
        &text,
        dashboard.as_ref(),
        &signed,
        image_url,
        chrome,
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

/// Spawn the out-of-band delivery task. Everything the task needs is
/// cloned out of the request now — `verified` (the allowlisted
/// serviceUrl) in particular is per-request state that must not be
/// borrowed across the spawn.
#[allow(clippy::too_many_arguments)]
fn spawn_courier(
    adapter: &Arc<MsTeamsAdapter>,
    verified: &VerifiedClaims,
    tool_name: String,
    args: Value,
    sender: ResolvedSender,
    conversation_id: String,
    recipient_id: String,
    conversation_type: Option<String>,
) {
    let tracker = adapter.courier.tracker.clone();
    let adapter = adapter.clone();
    let verified = verified.clone();
    let task = async move {
        courier_deliver(
            adapter,
            verified,
            tool_name,
            args,
            sender,
            conversation_id,
            recipient_id,
            conversation_type,
        )
        .await;
    };
    match tracker {
        Some(t) => {
            t.spawn(task);
        }
        None => {
            tokio::spawn(task);
        }
    }
}

/// Teams' typing indicator persists ~3s; the spec says senders MAY
/// re-send every 2s to prevent gaps. 2.5s splits the difference.
const TYPING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2500);

/// One courier turn (#635 P4): show progress immediately, dispatch
/// disconnect-safe, deliver the reply through the Connector.
///
/// * **1:1 chats** (`conversationType == "personal"`): the Teams
///   streaming shell — an informative `typing` activity ("Working on
///   it…") opens a stream (its returned activity id is the streamId),
///   and the final `message` closes it (`streamType: "final"`, no
///   sequence) carrying the full reply, Adaptive Card attachments
///   included (legal only on the final). A refused stream falls back
///   to a plain proactive message — the answer always lands.
/// * **Everything else** (group chats, channels, unknown): streaming
///   is rejected by the Connector, so a plain `typing` activity every
///   [`TYPING_INTERVAL`] keeps the indicator alive during the
///   dispatch, then one ordinary reply activity.
///
/// Audit: exactly one `phase: post` line per turn, carrying the real
/// HTTP status + latency of the call that delivered (or finally
/// failed). Progress activities are best-effort and unaudited.
#[allow(clippy::too_many_arguments)]
async fn courier_deliver(
    adapter: Arc<MsTeamsAdapter>,
    verified: VerifiedClaims,
    tool_name: String,
    args: Value,
    sender: ResolvedSender,
    conversation_id: String,
    recipient_id: String,
    conversation_type: Option<String>,
) {
    let principal = make_principal(&sender.sub, &sender.scopes, &sender.tenant);
    let principal_for_post = principal.clone();
    // See dispatch_and_post_reply: direct render_report invocations get
    // their chart URL minted from the invoked args, pre-dispatch.
    let image_hint = (tool_name == "render_report")
        .then(|| report_image_url(&adapter, &args, &sender.tenant))
        .flatten();
    let personal = conversation_type.as_deref() == Some("personal");

    // Progress. Personal: open a stream with an informative update.
    // Group: keep a typing ticker alive while the dispatch runs.
    let mut stream_id: Option<String> = None;
    let mut typing_ticker: Option<tokio::task::JoinHandle<()>> = None;
    if personal {
        let body = serde_json::json!({
            "type": "typing",
            "text": "Working on it…",
            "from": { "id": recipient_id },
            "conversation": { "id": conversation_id },
            "recipient": { "id": sender.from_id },
            "entities": [ {
                "type": "streaminfo",
                "streamType": "informative",
                "streamSequence": 1,
            } ],
        });
        match post_activity(&adapter, &verified, &conversation_id, &body).await {
            Ok((status, id)) if (200..300).contains(&status) => stream_id = id,
            Ok((status, _)) => {
                tracing::debug!(
                    status,
                    "msteams courier: stream open refused; plain delivery"
                );
            }
            Err(e) => {
                tracing::debug!(error = %e, "msteams courier: stream open failed; plain delivery");
            }
        }
    } else {
        let adapter2 = adapter.clone();
        let verified2 = verified.clone();
        let conversation2 = conversation_id.clone();
        let body = serde_json::json!({
            "type": "typing",
            "from": { "id": recipient_id },
            "conversation": { "id": conversation_id },
            "recipient": { "id": sender.from_id },
        });
        typing_ticker = Some(tokio::spawn(async move {
            loop {
                if let Err(e) = post_activity(&adapter2, &verified2, &conversation2, &body).await {
                    tracing::debug!(error = %e, "msteams courier: typing activity failed");
                }
                tokio::time::sleep(TYPING_INTERVAL).await;
            }
        }));
    }

    // Streaming dispatch (#635 P5): a tool that opts in emits Token
    // deltas we forward as CUMULATIVE `streaming` chunks (≤1/s, the
    // platform throttle) between the opener and the final; a buffered
    // tool yields one terminal Done and this degenerates to exactly
    // the old flow. `a2ui: None` — the courier renders the raw result.
    let started = std::time::Instant::now();
    let result: Result<Value, TritonError> = match adapter
        .dispatcher
        .invoke_streaming(&tool_name, args, principal, PROTOCOL, None)
        .await
    {
        Err(e) => Err(e),
        Ok(mut stream) => {
            use futures::StreamExt as _;
            let mut acc = String::new();
            let mut seq: u64 = 1; // the informative opener was 1
            let mut last_chunk = std::time::Instant::now();
            let mut terminal: Option<Result<Value, TritonError>> = None;
            while let Some(ev) = stream.next().await {
                match ev {
                    triton_core::stream::StreamEvent::Token(t) => {
                        acc.push_str(&t);
                        if let Some(sid) = &stream_id
                            && last_chunk.elapsed() >= std::time::Duration::from_millis(1000)
                            && !acc.trim().is_empty()
                        {
                            seq += 1;
                            let chunk = serde_json::json!({
                                "type": "typing",
                                "text": surface_mapper::clamp_plain_text(&acc),
                                "from": { "id": recipient_id },
                                "conversation": { "id": conversation_id },
                                "recipient": { "id": sender.from_id },
                                "entities": [ {
                                    "type": "streaminfo",
                                    "streamId": sid,
                                    "streamType": "streaming",
                                    "streamSequence": seq,
                                } ],
                            });
                            if let Err(e) =
                                post_activity(&adapter, &verified, &conversation_id, &chunk).await
                            {
                                tracing::debug!(error = %e, "msteams courier: streaming chunk failed");
                            }
                            last_chunk = std::time::Instant::now();
                        }
                    }
                    triton_core::stream::StreamEvent::Tool(_) => {}
                    triton_core::stream::StreamEvent::Done(v) => terminal = Some(Ok(v)),
                    triton_core::stream::StreamEvent::Error { error, .. } => {
                        terminal = Some(Err(error));
                    }
                }
            }
            terminal.unwrap_or_else(|| {
                Err(TritonError::Provider(
                    "stream ended without a terminal frame".into(),
                ))
            })
        }
    };
    let dispatch_latency_ms = started.elapsed().as_millis() as u64;
    if let Some(t) = typing_ticker {
        t.abort();
    }

    let mut body = match result {
        Ok(result_value) => {
            let chrome = fetch_chrome(&adapter, &principal_for_post).await;
            build_reply_body(
                &adapter,
                &recipient_id,
                &conversation_id,
                &sender.from_id,
                &result_value,
                image_hint,
                &chrome,
                &sender.tenant,
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "msteams courier dispatch failed");
            // One audited post line (Dropped + error_response), then a
            // best-effort visible notice so the user isn't staring at a
            // dead typing indicator.
            adapter.dispatcher.record_post(
                &tool_name,
                PROTOCOL,
                &principal_for_post,
                dispatch_latency_ms,
                Err((&e, 0, PostOutcome::Dropped, Some("error_response"))),
            );
            let notice = serde_json::json!({
                "type": "message",
                "from": { "id": recipient_id },
                "conversation": { "id": conversation_id },
                "recipient": { "id": sender.from_id },
                "text": format!("(error: {})", e.class()),
                "textFormat": "plain",
            });
            if let Err(err) = post_activity(&adapter, &verified, &conversation_id, &notice).await {
                tracing::warn!(error = %err, "msteams courier: error notice not delivered");
            }
            return;
        }
    };

    // Close the stream when one is open: the final message carries the
    // streaminfo terminator (no streamSequence on a final, per the
    // streaming contract). Attachments are legal here — only here.
    if let Some(sid) = &stream_id
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert(
            "entities".to_string(),
            serde_json::json!([ { "type": "streaminfo", "streamId": sid, "streamType": "final" } ]),
        );
    }

    let post_started = std::time::Instant::now();
    let mut outcome = post_activity(&adapter, &verified, &conversation_id, &body).await;
    if stream_id.is_some() && !matches!(&outcome, Ok((status, _)) if (200..300).contains(status)) {
        // A refused stream close (expired stream, ContentStreamNotAllowed)
        // must not cost the answer: retry once as a plain message and
        // audit THAT attempt.
        tracing::warn!("msteams courier: streamed final refused; retrying as a plain message");
        if let Some(obj) = body.as_object_mut() {
            obj.remove("entities");
        }
        outcome = post_activity(&adapter, &verified, &conversation_id, &body).await;
    }
    let latency_ms = post_started.elapsed().as_millis() as u64;
    match outcome {
        Ok((status, _)) if (200..300).contains(&status) => {
            adapter.dispatcher.record_post(
                &tool_name,
                PROTOCOL,
                &principal_for_post,
                latency_ms,
                Ok((status, PostOutcome::Posted, None)),
            );
        }
        Ok((status, _)) => {
            let label = if status >= 500 || status == 429 {
                PostOutcome::Retry
            } else {
                PostOutcome::Dropped
            };
            let provider =
                TritonError::Provider(format!("msteams activities POST status {status}"));
            adapter.dispatcher.record_post(
                &tool_name,
                PROTOCOL,
                &principal_for_post,
                latency_ms,
                Err((&provider, status, label, None)),
            );
        }
        Err(e) => {
            tracing::warn!("msteams courier activities POST failed: {e}");
            let provider = TritonError::Provider(format!("msteams transport: {e}"));
            adapter.dispatcher.record_post(
                &tool_name,
                PROTOCOL,
                &principal_for_post,
                latency_ms,
                Err((&provider, 0, PostOutcome::Retry, None)),
            );
        }
    }
}

/// POST one Activity to the conversation, un-audited: the primitive
/// under both the progress activities and the courier's final
/// delivery. Returns the HTTP status plus the created activity's `id`
/// from the response body — the streamId when the activity opened a
/// stream (the inline `post_reply` used to discard the body entirely).
async fn post_activity(
    adapter: &MsTeamsAdapter,
    verified: &VerifiedClaims,
    conversation_id: &str,
    body: &Value,
) -> Result<(u16, Option<String>), String> {
    let base = verified.reply_base().trim_end_matches('/');
    let url = format!("{}/v3/conversations/{}/activities", base, conversation_id);
    let access_token = adapter
        .token_client
        .access_token()
        .await
        .map_err(|e| format!("msteams token: {e}"))?;
    let resp = adapter
        .http
        .post(&url)
        .bearer_auth(&access_token)
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let id = resp
        .json::<Value>()
        .await
        .ok()
        .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_owned));
    Ok((status, id))
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
    let base = verified.reply_base().trim_end_matches('/');
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
