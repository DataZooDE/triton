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

pub mod conversation_card;
pub mod jwt_verifier;
pub mod md_card;
pub mod surface_mapper;
pub mod token_client;

pub use surface_mapper::RenderedMessage;

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Value, json};
use triton_core::{
    Dispatcher, OutboundCourier, OutboundRequest, PostOutcome, Principal, TritonError,
};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, OutboundKind, SignatureScheme};
use triton_secrets::{ResolveError, SecretResolver};

use jwt_verifier::{JwtVerifier, VerifiedClaims};
use token_client::TokenClient;

pub const PROTOCOL: &str = "messenger:msteams";

/// #289: re-exported from `triton-chat-identity`, which owns the
/// FR-I-7 seam.
pub use triton_chat_identity::SenderClaims;

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

/// Bot Framework channels where the caller, not the platform, chooses
/// `from.id` — so a body-derived principal is meaningless there.
///
/// Direct Line issues tokens to a web page; unless the embedding site
/// mints them server-side under enhanced authentication (a `dl_` prefix
/// on the user id), the id in the Activity is whatever the client sent.
/// `webchat` and `emulator` sit on the same machinery, and Copilot
/// Studio's test canvas is Direct Line underneath.
/// Lowercase; compared case-insensitively. `directlinespeech` is the
/// same Direct Line machinery with a speech front end, and `emulator`
/// and `test` are developer surfaces where the id is whatever the tool
/// sends.
const CLIENT_CHOSEN_ID_CHANNELS: &[&str] = &[
    "directline",
    "directlinespeech",
    "webchat",
    "emulator",
    "test",
];

fn default_allowed_channel_ids() -> Vec<String> {
    vec!["msteams".to_string()]
}

/// How this adapter resolves an inbound sender to a `Principal`.
enum IdentityMode {
    /// `from.id` (the AAD object id encoded as `29:...`) keyed into an
    /// operator-enumerated table.
    SenderTable(triton_chat_identity::SenderTable),
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
    correlation_key: triton_correlation::KeyRing,
    identity: IdentityMode,
    /// Channels this adapter serves, lowercased at build time.
    ///
    /// Adapter-level rather than per-identity-mode (#250 F11): the
    /// question "did I agree to serve this channel?" is asked of the
    /// transport, and the answer cannot depend on how identity is
    /// resolved afterwards. For `azure` it comes from
    /// `azure_identity.allowed_channel_ids`, so existing manifests keep
    /// their meaning; for every other mode from an optional
    /// `identity.allowed_channel_ids`, defaulting to Teams only.
    allowed_channel_ids: Vec<String>,
    /// Manifest `tool`: where plain inbound text dispatches (default
    /// `echo`). Commands (`/narrate` etc.) keep their special routes.
    inbound_tool: String,
    dispatcher: Arc<Dispatcher>,
    verifier: JwtVerifier,
    token_client: TokenClient,
    http: reqwest::Client,
    /// The email of an Entra member, from the Bot Connector's
    /// get-member, per (tid, aadObjectId), with the time it was fetched.
    member_emails: MemberEmails,
    /// F8b: long-running turns run as tasks (persisted, resumable). Set
    /// by the host with [`MsTeamsAdapter::with_tasks`]; `None` keeps the
    /// direct dispatch stream.
    tasks: Option<triton_tasks::Runner>,
    /// Task ids this process has claimed to deliver (boot sweep).
    delivering: std::sync::Mutex<std::collections::HashSet<String>>,
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
    /// Extra `serviceUrl` hosts the out-of-band courier will accept (beyond the
    /// canonical Microsoft suffixes) — mirrors the verifier's inbound extras, so
    /// a test fixture's `127.0.0.1` mock connector is reachable proactively too.
    /// Agent-multiplexing router (host-provided, #315). When `Some`, an inbound
    /// message and a `__use_agent` chooser click route through it to pick WHICH
    /// agent handles the turn, overriding `inbound_tool`. `None` ⇒ legacy
    /// single-tool `route_command(text, &inbound_tool)`.
    router: Option<Arc<dyn triton_chat_routing::AgentRouter>>,
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
        // #289: one call, one statement of the rule. `azure` stays
        // adapter-owned — its Entra config has no analogue in the other
        // seven adapters, so pulling it into the shared crate would be
        // the premature abstraction CLAUDE.md §4 warns about.
        triton_chat_identity::require_supported_kind(
            "msteams",
            &adapter.identity.kind,
            &[IdentityKind::SenderTable, IdentityKind::Azure],
        )
        .map_err(BuildError::Identity)?;

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
                // Optional `outbound.tenant_id`: a single-tenant bot's
                // secret is only valid at its own tenant's endpoint.
                let tenant_id = match adapter.outbound.credentials.get("tenant_id") {
                    Some(field) => Some(
                        resolver
                            .resolve(field)
                            .await
                            .map_err(|e| BuildError::Resolve("outbound.tenant_id", e))?,
                    )
                    .filter(|t| !t.trim().is_empty()),
                    None => None,
                };
                OutboundCredential::Secret { secret, tenant_id }
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
                // #289: validates every entry's `sub` and `tenant` at boot.
                IdentityMode::SenderTable(
                    triton_chat_identity::SenderTable::parse(&table_json)
                        .map_err(BuildError::Identity)?,
                )
                // Channels come from `identity.allowed_channel_ids` for
                // this mode; resolved below, after the match.
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
                let mut cfg: AzureConfig = serde_json::from_str(&cfg_json)
                    .map_err(|e| BuildError::TableParse(e.to_string()))?;
                // Fail closed: an empty allowlist is not cross-tenant
                // isolation. A single-tenant deployment lists its one
                // tenant explicitly.
                // #250: channels whose `from.id` is chosen by the CLIENT
                // cannot carry an Entra-shaped principal, because that
                // principal is read entirely from the Activity body. On
                // Direct Line-family channels Microsoft's connector will
                // mint a valid bot token for an anonymous user with a
                // self-chosen id, so `azure` there is sender-id-as-
                // password with a public password. Refuse the
                // combination at boot rather than trust an assumption
                // nobody has verified — serving those channels needs an
                // identity mode that does not read the principal off the
                // body (`sender_table` or `upstream`).
                // Normalised once, here: comparing case-sensitively would
                // make the gate depend on Microsoft's serialisation
                // matching the operator's spelling — the class of
                // unverified assumption this whole change exists to
                // remove. `"DirectLine"` must be refused exactly like
                // `"directline"`.
                cfg.allowed_channel_ids
                    .iter_mut()
                    .for_each(|c| *c = c.trim().to_ascii_lowercase());
                if cfg.allowed_channel_ids.is_empty() {
                    return Err(BuildError::Unsupported(
                        "identity.azure_identity `allowed_channel_ids` is present but empty; \
                         that refuses every Activity while looking configured. Omit the field \
                         for the Teams-only default, or name the channels this adapter serves."
                            .into(),
                    ));
                }
                if let Some(bad) = cfg
                    .allowed_channel_ids
                    .iter()
                    .find(|c| CLIENT_CHOSEN_ID_CHANNELS.contains(&c.as_str()))
                {
                    return Err(BuildError::Unsupported(format!(
                        "identity.kind `azure` cannot serve channel `{bad}`: its \
                         `from.id` is client-chosen, so the Entra fields this \
                         strategy reads from the Activity body prove nothing. \
                         Use `upstream` for that channel — NOT `sender_table`, \
                         which this message used to suggest: its key is \
                         `from.id`, which is the very field the client \
                         chooses there (#250 F11)."
                    )));
                }
                // #250: the tenant this strategy derives comes from
                // `channelData.tenant.id` — unsigned body metadata. With
                // ONE allowed tenant that is harmless: the only value
                // that passes is the only value it could have been, so
                // the check is equivalent to pinning. With two or more
                // the body field becomes a privilege SELECTOR — a caller
                // able to present an Activity chooses which tenant the
                // downstream Escurel token is scoped to, and nothing in
                // the transport can contradict them (the Bot Framework
                // connector token carries no `tid`; realizations §7).
                //
                // No mechanism Microsoft ships makes that assertion
                // trustworthy, so the configuration is refused rather
                // than served insecurely. Multi-tenant has two supported
                // shapes, both of which keep the tenant out of the body:
                // `identity.kind: upstream`, where a resolver decides
                // it, or one bot registration per tenant, which puts it
                // in the credential.
                match cfg.allowed_tenants.len() {
                    0 => {
                        return Err(BuildError::Unsupported(
                            "azure identity requires a non-empty `allowed_tenants` list \
                             (fail-closed cross-tenant isolation)"
                                .into(),
                        ));
                    }
                    1 => {}
                    n => {
                        return Err(BuildError::Unsupported(format!(
                            "azure identity lists {n} `allowed_tenants`, but it derives the \
                             tenant from `channelData.tenant.id` — unsigned body metadata. \
                             With one tenant that check is equivalent to pinning; with more \
                             it lets the caller SELECT which tenant's data their token is \
                             scoped to, and the Bot Framework token carries no `tid` to \
                             contradict them. Use `identity.kind: upstream` (a resolver \
                             decides the tenant) or one bot registration per tenant."
                        )));
                    }
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

        // #250 F11: one channel allowlist for the adapter, whatever the
        // identity mode.
        //
        // `azure` already declares its channels inside `azure_identity`,
        // so it keeps that source and no existing manifest changes
        // meaning. Every other mode reads an optional
        // `identity.allowed_channel_ids` (a JSON array) and otherwise
        // gets the Teams-only default — the same default `azure` has had
        // since #250, and fail-closed: a deployment serving another
        // channel must now say so, rather than serving it ungated.
        let allowed_channel_ids: Vec<String> = match &identity {
            IdentityMode::Azure(cfg) => cfg.allowed_channel_ids.clone(),
            _ => {
                let declared = match adapter.identity.credentials.get("allowed_channel_ids") {
                    Some(field) => {
                        let json = resolver
                            .resolve(field)
                            .await
                            .map_err(|e| BuildError::Resolve("identity.allowed_channel_ids", e))?;
                        serde_json::from_str::<Vec<String>>(&json)
                            .map_err(|e| BuildError::TableParse(e.to_string()))?
                    }
                    None => default_allowed_channel_ids(),
                };
                let declared: Vec<String> = declared
                    .into_iter()
                    .map(|c| c.trim().to_ascii_lowercase())
                    .collect();
                if declared.is_empty() {
                    return Err(BuildError::Unsupported(
                        "identity.allowed_channel_ids is present but empty; that refuses \
                         every Activity while looking configured. Omit it for the \
                         Teams-only default, or name the channels this adapter serves."
                            .into(),
                    ));
                }
                // A client-chosen `from.id` is not an identity, and a
                // sender table keyed on it is sender-id-as-password with
                // a public password (#250 F11, measured). Refused for the
                // same reason `azure` refuses it, and stated separately
                // because the old `azure` message recommended exactly
                // this combination.
                if let Some(bad) = declared
                    .iter()
                    .find(|c| CLIENT_CHOSEN_ID_CHANNELS.contains(&c.as_str()))
                {
                    return Err(BuildError::Unsupported(format!(
                        "this adapter cannot serve channel `{bad}`: its `from.id` is \
                         chosen by the CLIENT, and every identity mode here keys on \
                         that field, so a mapped sender id is guessable rather than \
                         proven. Serving it needs an identity mode that does not read \
                         the principal off the Activity body."
                    )));
                }
                declared
            }
        };

        // #287: a comma-separated RING — signed with the first key,
        // verified against all — so the key can be rotated without
        // invalidating every button already in a conversation.
        let correlation_key = triton_correlation::KeyRing::parse(
            &resolver
                .resolve(&adapter.correlation_key)
                .await
                .map_err(|e| BuildError::Resolve("correlation_key", e))?,
        )
        .map_err(BuildError::CorrelationKey)?;

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
        // Kept for the OUT-OF-BAND courier's serviceUrl allow-list check: an
        // inbound reply trusts the serviceUrl by JWT derivation, but a proactive
        // send validates the stored serviceUrl host against the same suffixes
        // (plus any test-fixture extras) the verifier uses.
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
        let token_client =
            outbound_token_client(client_id, outbound_credential, overrides.token_url)?;
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
            allowed_channel_ids,
            inbound_tool: adapter.tool.clone(),
            dispatcher,
            verifier,
            token_client,
            http,
            member_emails: MemberEmails::default(),
            tasks: None,
            delivering: Default::default(),
            rate_limit,
            per_tenant_limit,
            courier,
            canonical_path,
            router: None,
        })
    }

    /// Drive courier turns through the task runner (F8b): an asynchronous
    /// upstream task is stored with its delivery target, resumed when its
    /// stream breaks or is cut, bounded by `TRITON_TASK_MAX_SECS`, and
    /// resumed after a restart by [`MsTeamsAdapter::resume_stored_tasks`].
    pub fn with_tasks(mut self, runner: triton_tasks::Runner) -> Self {
        self.tasks = Some(runner);
        self
    }

    /// Finish every stored Teams task of THIS adapter (its name and bot app
    /// id) whose answer was not confirmed delivered — still running, or
    /// finished while no process delivered it. Call once at boot, after the
    /// adapter is wrapped in an `Arc`; a second call claims nothing new.
    pub fn resume_stored_tasks(self: &Arc<Self>) {
        let Some(runner) = &self.tasks else {
            return;
        };
        for rec in runner.store.list_undelivered(PROTOCOL) {
            let Some(d) = rec
                .delivery
                .clone()
                .and_then(|v| serde_json::from_value::<Delivery>(v).ok())
            else {
                continue;
            };
            if d.adapter != self.name || d.bot_app_id != self.audience {
                continue; // another bot's conversation
            }
            // One delivery claim per task in this process.
            if !self.delivering.lock().unwrap().insert(rec.id.clone()) {
                continue;
            }
            tracing::info!(task = %rec.id, "msteams: finishing a stored task");
            let adapter = self.clone();
            tokio::spawn(async move { resume_task(adapter, rec, d).await });
        }
    }

    /// Attach the agent-multiplexing router (host-provided). Call before
    /// wrapping the adapter in an `Arc`. Absent ⇒ legacy single-tool routing.
    pub fn with_router(mut self, router: Arc<dyn triton_chat_routing::AgentRouter>) -> Self {
        self.router = Some(router);
        self
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
    /// A static secret; `tenant_id` set for a single-tenant bot.
    Secret {
        secret: String,
        tenant_id: Option<String>,
    },
    Federated {
        token_file: String,
        tenant_id: String,
    },
}

/// The outbound token client for one credential mode. `token_url` is the
/// local-only test override, which wins in every mode.
fn outbound_token_client(
    client_id: String,
    credential: OutboundCredential,
    token_url: Option<String>,
) -> Result<TokenClient, BuildError> {
    Ok(match (credential, token_url) {
        (OutboundCredential::Secret { secret, .. }, Some(url)) => {
            TokenClient::with_token_url(client_id, secret, url)
        }
        (
            OutboundCredential::Secret {
                secret,
                tenant_id: None,
            },
            None,
        ) => TokenClient::new(client_id, secret),
        (
            OutboundCredential::Secret {
                secret,
                tenant_id: Some(tenant_id),
            },
            None,
        ) => TokenClient::with_secret_for_tenant(client_id, &tenant_id, secret)
            .map_err(|e| BuildError::Unsupported(e.to_string()))?,
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
    })
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// #289: FR-I-7 resolution now lives in `triton-chat-identity`;
    /// its refusals surface here unchanged.
    #[error("identity: {0}")]
    Identity(#[source] triton_chat_identity::IdentityError),
    /// #287: the resolved `correlation_key` secret is a
    /// comma-separated ring; nothing usable survived parsing it.
    #[error("correlation_key: {0}")]
    CorrelationKey(#[source] triton_correlation::KeyRingError),
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
    /// On a card action invoke: the activity id of the card that was
    /// clicked, which the answer replaces in place.
    #[serde(default, rename = "replyToId")]
    reply_to_id: Option<String>,
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
            service_url_attested: true,
        },
        None => match activity.service_url.as_deref() {
            Some(from_body) if adapter.verifier.service_url_allowed(from_body) => VerifiedClaims {
                service_url: Some(from_body.to_string()),
                // Body-supplied: allowlisted as a reply TARGET, but it
                // attests nothing about which channel delivered this.
                service_url_attested: false,
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
        // A source "Open" button (task/fetch): render the cited document and
        // return it as a task-module dialog card. Fast (a peacock render, not
        // an LLM turn), so it rides the invoke's own HTTP response.
        Some("invoke") if activity.name.as_deref() == Some("task/fetch") => {
            handle_task_fetch(&adapter, &verified, &activity).await
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

/// The `serviceUrl` host family Microsoft uses for a Bot Framework
/// channel, for the channels there is documented evidence for.
///
/// `None` means "no documented mapping", and the caller must then
/// corroborate NOTHING. Inventing an entry for a channel we have not
/// verified — `pva` (Copilot Studio) is the live example — would refuse
/// real traffic on a guess, which is the over-reach that made this
/// finding a deferral in the first place.
///
/// Matched on the HOST family, never the path. Teams is regional
/// (`/teams/`, `/amer/`, `/emea/`, `/in/`, …) and Microsoft adds regions;
/// pinning the path would turn each new one into an outage.
fn expected_service_url_family(channel_id: &str) -> Option<&'static str> {
    match channel_id {
        // Teams: https://smba.trafficmanager.net/<region>/
        "msteams" => Some(".trafficmanager.net"),
        // Direct Line and Web Chat: https://directline.botframework.com/,
        // https://webchat.botframework.com/
        "directline" | "webchat" | "emulator" => Some(".botframework.com"),
        _ => None,
    }
}

/// Does `service_url`'s host sit in `family` (on a DNS-label boundary)?
fn service_url_in_family(service_url: &str, family: &str) -> bool {
    let Ok(parsed) = url::Url::parse(service_url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let apex = family.strip_prefix('.').unwrap_or(family);
    host == apex || host.ends_with(family)
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
    /// The Entra identity Bot Framework vouched for (`azure`
    /// identity): provider `entra`, tenant = `channelData.tenant.id`. The
    /// activity carries no email.
    idp: Option<triton_core::IdpIdentity>,
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
    verified: &VerifiedClaims,
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

    // #250 crew F11 — the channel trust decision is made HERE, above the
    // identity-mode match, for EVERY mode.
    //
    // It used to live inside the `azure` arm only, so a `sender_table`
    // deployment made no channel decision at all: an Activity declaring
    // `channelId: "directline"` carrying a mapped Teams sender id
    // dispatched as that principal, status 200 (measured, not argued —
    // `a_sender_table_makes_no_channel_trust_decision`). `from.id` is
    // body metadata too, and on a channel where the CLIENT chooses it,
    // a table lookup is sender-id-as-password with a public password.
    //
    // That also corrects the advice the old boot refusal gave: it told
    // operators to serve client-chosen-id channels with `sender_table`
    // "or `upstream`", which was wrong for `sender_table` for exactly
    // this reason.
    // Folded ONCE, and every comparison below uses this value.
    //
    // `allowed_channel_ids` is lowercased at build time, so the gate
    // has to fold the inbound too — the gate must not turn on
    // Microsoft's casing. The corroboration further down then has to
    // fold the SAME way, and a crew review found that it did not: it
    // matched the raw `channelId` against exact lowercase literals,
    // so `"MSTEAMS"` passed the gate, returned `None` from
    // `expected_service_url_family`, and skipped the corroboration
    // entirely. One character of case reopened the path the
    // corroboration exists to close.
    //
    // Two comparisons that must agree on a folded value cannot each
    // fold their own way. Fold once, here.
    let channel = activity
        .channel_id
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let channel = channel.as_str();
    if !adapter
        .allowed_channel_ids
        .iter()
        .any(|c| c.eq_ignore_ascii_case(channel))
    {
        record_rejection(
            adapter,
            "-",
            "-",
            TritonError::Auth(format!(
                "channelId {channel:?} is not on this adapter's \
                     allowed_channel_ids {:?}",
                adapter.allowed_channel_ids
            )),
        );
        return Err((StatusCode::UNAUTHORIZED, "wrong channel").into_response());
    }
    // #250 crew F3: the declared channel must match the transport
    // that actually delivered this.
    //
    // Everything the gate above read is UNSIGNED body metadata, so
    // a deployment declaring Teams only is still reachable over any
    // channel Microsoft will mint a token for: deliver over Direct
    // Line — where `from.id` is client-chosen, which is why `azure`
    // refuses to boot on a declared Direct Line channel — and write
    // `"channelId": "msteams"` in the body. `from.aadObjectId` and
    // `channelData.tenant.id` then become the principal.
    //
    // `serviceUrl` is the one field describing the transport rather
    // than the sender's claim about it, and on a multi-tenant bot
    // Microsoft SIGNS it into the connector token. Microsoft assigns
    // those hosts by channel family, so a signed Direct Line host
    // beside a body claiming `msteams` is a contradiction.
    //
    // Three deliberate limits, each a case where a check would look
    // stronger than it is:
    //
    //  * Only when the value is ATTESTED. A single-tenant bot's
    //    Entra token carries no `serviceurl` claim, so its reply
    //    target is body-supplied too — corroborating one body field
    //    against another proves nothing.
    //  * Only for channels with a documented host family
    //    (`expected_service_url_family`). No guessing.
    //  * NOT for the tenant. That half of F3 is not deferred, it is
    //    impossible: Microsoft assigns the host by REGION and every
    //    tenant in a region shares one, so a host can never identify
    //    a tenant.
    // Say ONCE, on real traffic, when the corroboration does not
    // apply.
    //
    // A crew review named the risk precisely: the code, the tests
    // and realizations.md all read as though the forged-channel
    // hole is closed, but the check needs an ATTESTED serviceUrl
    // and a single-tenant Entra token carries no `serviceurl`
    // claim at all — which is the shape agent-lab actually runs.
    // So on that deployment the control never fires, and nothing
    // said so.
    //
    // A boot-time line could not tell the truth here: the binary
    // cannot know which token shape will arrive until one does.
    // This fires on the first Activity that skips the check, which
    // is the moment the fact becomes knowable. Once per process —
    // an operator needs to know THAT it is inactive, not once per
    // request.
    if !verified.service_url_attested {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
                println!(
                    r#"{{"kind":"log","level":"warn","msg":"msteams: channel/serviceUrl corroboration INACTIVE — this Activity carried no signed `serviceurl` claim (single-tenant Entra bots carry none), so `channelId` is trusted from the unsigned body alone (#250/#319)","channel":"{}"}}"#,
                    adapter.name.escape_default(),
                );
            });
    }
    if verified.service_url_attested
            && let Some(url) = verified.service_url.as_deref()
            // A fixture/local stand-in host is in no channel family.
            && !adapter.verifier.is_extra_service_url_host(url)
            && let Some(family) = expected_service_url_family(channel)
            && !service_url_in_family(url, family)
    {
        record_rejection(
            adapter,
            "-",
            "-",
            TritonError::Auth(format!(
                "channelId {channel:?} contradicts the signed \
                     serviceUrl host, which is not in {family:?} — the \
                     Activity was delivered on a different channel than \
                     its body claims"
            )),
        );
        return Err((StatusCode::UNAUTHORIZED, "channel mismatch").into_response());
    }

    let (sub, scopes, tenant) = match &adapter.identity {
        IdentityMode::SenderTable(table) => match table.resolve(&from.id) {
            Some(r) => {
                let (sub, scopes, _groups, tenant) = r.into_parts();
                (sub, scopes, tenant)
            }
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
            //
            // #306 crew F3 asked for this assertion to be CORROBORATED
            // against the verified `serviceUrl` host, turning a body
            // claim into a transport-backed one.
            //
            // The CHANNEL half of that is now implemented below, against
            // Microsoft's documented host families. The TENANT half is
            // not, and is not pending either — it is impossible.
            // Microsoft assigns the `serviceUrl` host by REGION
            // (`/amer/`, `/emea/`, `/in/`), and every tenant in a region
            // shares one, so a host can never identify a tenant. An
            // earlier version of this comment implied the tenant check
            // awaited data collection; it does not, and saying so was
            // itself a small hazard — a deferral that reads as scheduled
            // gets scheduled.

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
            // Cross-tenant isolation: the inbound tenant MUST match the
            // single configured one (the list is guaranteed to hold
            // exactly one entry at build time — see `from_manifest`), so
            // this is an equality check against configuration, not a
            // selection the caller gets to make.
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
    let idp = matches!(adapter.identity, IdentityMode::Azure(_)).then(|| {
        triton_core::IdpIdentity::new("entra")
            .tenant(Some(tenant.clone()))
            .issuer("https://api.botframework.com")
    });

    Ok(ResolvedSender {
        from_id: from.id.clone(),
        sub,
        scopes,
        tenant,
        idp,
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

/// Cache of member emails: (tid, aadObjectId) → (email, fetched at).
type MemberEmails = std::sync::Mutex<
    std::collections::HashMap<(String, String), (Option<String>, std::time::Instant)>,
>;

/// How long a looked-up email (or its absence) is reused.
const MEMBER_EMAIL_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
const MEMBER_EMAIL_MISS_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// Bound on the cache; past it, the oldest half is dropped.
const MEMBER_EMAIL_CAP: usize = 4096;

/// A Teams activity names the Entra user (`aadObjectId`, tid) but
/// not their email, which the upstream binds members by. Ask the Bot
/// Connector once — `GET {serviceUrl}/v3/conversations/{id}/members/{aadObjectId}`
/// (a `TeamsChannelAccount`: `email`, else `userPrincipalName`) — and
/// cache the answer per (tid, aadObjectId). Best effort: a failed lookup
/// leaves the email absent and never blocks the turn.
async fn attach_member_email(
    adapter: &Arc<MsTeamsAdapter>,
    verified: &VerifiedClaims,
    activity: &Activity,
    sender: &mut ResolvedSender,
) {
    let Some(idp) = sender.idp.as_mut() else {
        return; // only the Entra (`azure`) identity has a member to ask for
    };
    let Some(tid) = idp.tenant.clone() else {
        return;
    };
    let key = (tid, sender.sub.clone());
    let cached = adapter
        .member_emails
        .lock()
        .unwrap()
        .get(&key)
        .filter(|(email, at)| {
            at.elapsed()
                < if email.is_some() {
                    MEMBER_EMAIL_TTL
                } else {
                    MEMBER_EMAIL_MISS_TTL
                }
        })
        .map(|(email, _)| email.clone());
    let email = match cached {
        Some(email) => email,
        None => {
            let conversation = activity
                .conversation
                .as_ref()
                .map(|c| c.id.clone())
                .unwrap_or_default();
            let email = fetch_member_email(adapter, verified, &conversation, &sender.sub).await;
            let mut cache = adapter.member_emails.lock().unwrap();
            if cache.len() >= MEMBER_EMAIL_CAP {
                let mut by_age: Vec<_> =
                    cache.iter().map(|(k, (_, at))| (k.clone(), *at)).collect();
                by_age.sort_by_key(|(_, at)| *at);
                for (k, _) in by_age.into_iter().take(MEMBER_EMAIL_CAP / 2) {
                    cache.remove(&k);
                }
            }
            cache.insert(key, (email.clone(), std::time::Instant::now()));
            email
        }
    };
    *idp = idp.clone().email(email);
}

async fn fetch_member_email(
    adapter: &MsTeamsAdapter,
    verified: &VerifiedClaims,
    conversation_id: &str,
    member_id: &str,
) -> Option<String> {
    if conversation_id.is_empty() {
        return None;
    }
    let mut url = url::Url::parse(verified.reply_base()).ok()?;
    url.path_segments_mut().ok()?.pop_if_empty().extend([
        "v3",
        "conversations",
        conversation_id,
        "members",
        member_id,
    ]);
    let token = adapter.token_client.access_token().await.ok()?;
    let resp = adapter
        .http
        .get(url.to_string())
        .bearer_auth(&token)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), "msteams get-member: no member");
        return None;
    }
    let member: Value = resp.json().await.ok()?;
    member
        .get("email")
        .or_else(|| member.get("userPrincipalName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|e| e.contains('@') && e.len() <= 128)
        .map(str::to_owned)
}

/// The principal of a resolved sender, with its identity provider.
fn sender_principal(sender: &ResolvedSender) -> Principal {
    let mut p = make_principal(&sender.sub, &sender.scopes, &sender.tenant);
    p.idp = sender.idp.clone();
    p
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
        conversation_ref: None,
        idp: None,
    }
}

async fn dispatch_message(
    adapter: &Arc<MsTeamsAdapter>,
    verified: &VerifiedClaims,
    activity: &Activity,
    text: &str,
) -> Response {
    let mut sender = match resolve_sender(adapter, activity, verified) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    attach_member_email(adapter, verified, activity, &mut sender).await;
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

    let conversation_type = activity
        .conversation
        .as_ref()
        .and_then(|c| c.conversation_type.clone());

    // Agent multiplexing (host-provided router, #315). Route by text to pick
    // the agent; a chooser / info / deny reply is POSTed here, a Dispatch runs
    // the picked agent. `None` router ⇒ legacy single-tool routing below.
    let (tool_name, args) = if let Some(router) = adapter.router.clone() {
        let key =
            triton_chat_routing::ConvKey::msteams(conversation_id.clone(), "", sender.sub.clone());
        let ctx = triton_chat_routing::RouteCtx {
            key,
            text: stripped,
            tenant: &sender.tenant,
            caller_sub: &sender.sub,
            pick: None,
            conversation_ref: Some(conversation_reference_json(
                verified.reply_base(),
                &conversation_id,
                &recipient_id,
                &sender.from_id,
                &sender.tenant,
                &adapter.correlation_key,
            )),
        };
        match router.route(ctx).await {
            triton_chat_routing::RouteOutcome::Dispatch { agent_id, text } => {
                if text.trim().is_empty() {
                    let principal = sender_principal(&sender);
                    let info = format!(
                        "Switched to **{agent_id}**. You can now ask questions directly, or type `/whoami` to inspect active bindings."
                    );
                    let msg = surface_mapper::RenderedMessage::text_only(info);
                    let body = surface_mapper::build_activity_body(
                        &recipient_id,
                        &conversation_id,
                        &sender.from_id,
                        &msg,
                    );
                    post_reply(
                        adapter,
                        verified,
                        triton_chat_routing::USE_AGENT_TOOL,
                        &principal,
                        &conversation_id,
                        body,
                        0,
                    )
                    .await;
                    return StatusCode::OK.into_response();
                }
                route_command(&text, &agent_id)
            }
            triton_chat_routing::RouteOutcome::Chooser {
                candidates,
                pending_text,
                ..
            } => {
                let principal = sender_principal(&sender);
                let chrome = fetch_chrome(adapter, &principal).await;
                let signed: Vec<(String, String)> = candidates
                    .iter()
                    .filter_map(|a| {
                        let payload = serde_json::json!({ "id": a.id, "msg": pending_text });
                        // Bound like every other card token (#250/#287):
                        // the chooser lands in a channel every member can
                        // see, so it is a capability for the CLICKER.
                        triton_correlation::encode_bound(
                            triton_chat_routing::USE_AGENT_TOOL,
                            &payload,
                            adapter.correlation_key.signing(),
                            surface_mapper::MSTEAMS_CORRELATION_CAP,
                            triton_correlation::Binding {
                                platform: "msteams",
                                tenant: &sender.tenant,
                                sender: &sender.from_id,
                            },
                            Some(CARD_TOKEN_TTL_SECS),
                        )
                        .ok()
                        .map(|tok| (a.display.clone(), tok))
                    })
                    .collect();
                let prompt = "Which agent should handle this? Pick one below, or type \
                              `/use <name>` any time (`/agents` to list, `/whoami` to check).";
                let card = surface_mapper::build_agent_chooser(prompt, &signed, &chrome);
                let body = surface_mapper::build_card_activity_body(
                    &recipient_id,
                    &conversation_id,
                    &sender.from_id,
                    card,
                );
                post_reply(
                    adapter,
                    verified,
                    triton_chat_routing::USE_AGENT_TOOL,
                    &principal,
                    &conversation_id,
                    body,
                    0,
                )
                .await;
                return StatusCode::OK.into_response();
            }
            triton_chat_routing::RouteOutcome::Info { text }
            | triton_chat_routing::RouteOutcome::Deny { message: text } => {
                let principal = sender_principal(&sender);
                let msg = surface_mapper::RenderedMessage::text_only(text);
                let body = surface_mapper::build_activity_body(
                    &recipient_id,
                    &conversation_id,
                    &sender.from_id,
                    &msg,
                );
                post_reply(
                    adapter,
                    verified,
                    triton_chat_routing::USE_AGENT_TOOL,
                    &principal,
                    &conversation_id,
                    body,
                    0,
                )
                .await;
                return StatusCode::OK.into_response();
            }
        }
    } else {
        route_command(stripped, &adapter.inbound_tool)
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

/// Deep-search a `render_report` result for the document render's structured
/// content: the object carrying an `instances` or `document` key. Robust to
/// the exact nesting (top level, or under `structuredContent`).
fn find_document_structured(v: &Value) -> Option<&Value> {
    match v {
        Value::Object(m) => {
            if m.contains_key("instances") || m.contains_key("document") {
                return Some(v);
            }
            m.values().find_map(find_document_structured)
        }
        Value::Array(a) => a.iter().find_map(find_document_structured),
        _ => None,
    }
}

/// `task/fetch` (a source "Open" button): decode the signed open-doc token,
/// render the peacock `document` report for its `(skill, id)`, and return the
/// document as a `task/continue` dialog card. The webhook JWT is already
/// verified; the HMAC token authorises the specific (skill, id).
async fn handle_task_fetch(
    adapter: &Arc<MsTeamsAdapter>,
    verified: &VerifiedClaims,
    activity: &Activity,
) -> Response {
    let mut sender = match resolve_sender(adapter, activity, verified) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    attach_member_email(adapter, verified, activity, &mut sender).await;
    if let Some(resp) = check_tenant_limit(adapter, &sender.sub, &sender.tenant) {
        return resp;
    }
    let token = activity
        .value
        .as_ref()
        .and_then(|v| v.get("data"))
        .and_then(|d| d.get(surface_mapper::TOKEN_DATA_KEY))
        .and_then(Value::as_str);
    let Some(token) = token else {
        return (StatusCode::BAD_REQUEST, "missing action").into_response();
    };
    // #250/#287: the OpenDoc token was minted bound to (tenant, sender)
    // in `render_card_content`; verify it against the SENDER of this
    // `task/fetch`, not merely against the key.
    let (tool_name, args) = match triton_correlation::decode_bound_any(
        token,
        &adapter.correlation_key,
        surface_mapper::MSTEAMS_CORRELATION_CAP,
        triton_correlation::Binding {
            platform: "msteams",
            tenant: &sender.tenant,
            sender: &sender.from_id,
        },
    ) {
        Ok(p) => p,
        Err(_) => {
            record_rejection(
                adapter,
                &sender.sub,
                &sender.tenant,
                TritonError::Auth("task/fetch correlation token invalid".into()),
            );
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };
    // 2d: an Embed's "open" button → a URL task module (the host must be in
    // the Teams app manifest's `validDomains`; the inline fallback card is
    // already in the conversation for hosts that cannot open it).
    if tool_name == conversation_card::OPEN_EMBED_TOOL {
        let url = args.get("u").and_then(Value::as_str).unwrap_or_default();
        let title = args.get("t").and_then(Value::as_str).unwrap_or_default();
        if !triton_core::links::is_allowed_link(url) {
            return (StatusCode::BAD_REQUEST, "unexpected action").into_response();
        }
        return (
            StatusCode::OK,
            axum::Json(conversation_card::url_task_response(title, url)),
        )
            .into_response();
    }
    let skill = args
        .get("skill")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = args.get("id").and_then(Value::as_str).unwrap_or_default();
    if tool_name != surface_mapper::OPEN_DOC_TOOL || skill.is_empty() || id.is_empty() {
        return (StatusCode::BAD_REQUEST, "unexpected action").into_response();
    }

    let title = format!("{skill} \u{b7} {id}");
    let principal = sender_principal(&sender);
    let principal_for_post = principal.clone();
    let started = std::time::Instant::now();
    let result = adapter
        .dispatcher
        .invoke(
            "render_report",
            json!({ "report_id": "document", "params": { "skill": skill, "id": id } }),
            principal,
            PROTOCOL,
        )
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    let card = match &result {
        Ok(dispatch) => {
            let structured = find_document_structured(&dispatch.result)
                .cloned()
                .unwrap_or(Value::Null);
            surface_mapper::build_document_dialog_card(&structured)
        }
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "msteams task/fetch render failed");
            // A minimal card so the dialog shows a message instead of failing.
            json!({
                "type": "AdaptiveCard",
                "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                "version": "1.4",
                "body": [ { "type": "TextBlock", "wrap": true,
                    "text": "This document is unavailable right now." } ],
            })
        }
    };
    adapter.dispatcher.record_post(
        surface_mapper::OPEN_DOC_TOOL,
        PROTOCOL,
        &principal_for_post,
        latency_ms,
        Ok((200, PostOutcome::Posted, None)),
    );
    (
        StatusCode::OK,
        axum::Json(surface_mapper::task_continue_response(&title, card)),
    )
        .into_response()
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
    let mut sender = match resolve_sender(adapter, activity, verified) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    attach_member_email(adapter, verified, activity, &mut sender).await;
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
    // #250/#287: verified against the SENDER's tenant AND the SENDER.
    // An `Action.Submit.data` token travels in a card that every member
    // of a Teams channel can see, so without the sender in the
    // derivation the card is a capability held by the whole tenant.
    let (mut tool_name, mut args) = match triton_correlation::decode_bound_any(
        &token,
        &adapter.correlation_key,
        surface_mapper::MSTEAMS_CORRELATION_CAP,
        triton_correlation::Binding {
            platform: "msteams",
            tenant: &sender.tenant,
            sender: &sender.from_id,
        },
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
    //
    // A stateless send-message action (chip, row, template submit) is the
    // exception: its message/template comes ONLY from the signed token, and
    // the card inputs fill the template's placeholders from their OWN map.
    // Merging them into the args first would let a field named `m`/`tpl` —
    // or a key a client adds to `data` — replace the signed message (F1).
    // The click is a NEW user message with the complete text, routed exactly
    // like typed input, never a continuation of the card's turn.
    if tool_name == REFRESH_TOOL {
        return refresh_card(adapter, &sender, &args).await;
    }
    if tool_name == triton_correlation::message::SEND_MESSAGE_TOOL {
        let inputs: std::collections::BTreeMap<String, String> = inputs.into_iter().collect();
        let text = match triton_correlation::message::resolve(&args, |k| inputs.get(k).cloned()) {
            Ok(t) => t,
            Err(e) => {
                // Name the missing fields without their card namespace
                // (`t2.status` → `status`).
                let e = match e {
                    triton_correlation::message::MessageError::Missing(names) => {
                        triton_correlation::message::MessageError::Missing(
                            names
                                .into_iter()
                                .map(|n| n.rsplit_once('.').map_or(n.clone(), |(_, b)| b.into()))
                                .collect(),
                        )
                    }
                    other => other,
                };
                return (
                    StatusCode::OK,
                    axum::Json(surface_mapper::invoke_message_response(&e.to_string())),
                )
                    .into_response();
            }
        };
        match route_typed_text(adapter, verified, activity, &sender, &text).await {
            Ok((t, a)) => {
                tool_name = t;
                args = a;
            }
            Err(reply) => {
                return (
                    StatusCode::OK,
                    axum::Json(surface_mapper::invoke_message_response(&reply)),
                )
                    .into_response();
            }
        }
    } else {
        merge_inputs(&mut args, inputs);
    }

    // Agent multiplexing (#315): a `__use_agent` chooser click confirms a pick.
    // The router binds the agent and replays the buffered message; the picked
    // agent then runs through the normal Execute path below. A denied pick
    // answers the invoke directly.
    if tool_name == triton_chat_routing::USE_AGENT_TOOL
        && let Some(router) = adapter.router.clone()
    {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let msg = args
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let conversation_id = activity
            .conversation
            .as_ref()
            .map(|c| c.id.clone())
            .unwrap_or_default();
        let bot_id = activity
            .recipient
            .as_ref()
            .map(|r| r.id.clone())
            .unwrap_or_default();
        let conversation_ref = Some(conversation_reference_json(
            verified.reply_base(),
            &conversation_id,
            &bot_id,
            &sender.from_id,
            &sender.tenant,
            &adapter.correlation_key,
        ));
        let key = triton_chat_routing::ConvKey::msteams(conversation_id, "", sender.sub.clone());
        let ctx = triton_chat_routing::RouteCtx {
            key,
            text: &msg,
            tenant: &sender.tenant,
            caller_sub: &sender.sub,
            pick: Some((id, msg.clone())),
            conversation_ref,
        };
        match router.route(ctx).await {
            triton_chat_routing::RouteOutcome::Dispatch { agent_id, text } => {
                let (t, a) = route_command(&text, &agent_id);
                tool_name = t;
                args = a;
            }
            triton_chat_routing::RouteOutcome::Info { text }
            | triton_chat_routing::RouteOutcome::Deny { message: text } => {
                return (
                    StatusCode::OK,
                    axum::Json(surface_mapper::invoke_message_response(&text)),
                )
                    .into_response();
            }
            // A pick that resolves to another chooser is degenerate; surface it
            // as a hint rather than dispatching the sentinel tool.
            triton_chat_routing::RouteOutcome::Chooser { .. } => {
                return (
                    StatusCode::OK,
                    axum::Json(surface_mapper::invoke_message_response(
                        "That agent isn't available — type `/agents` to see the list.",
                    )),
                )
                    .into_response();
            }
        }
    }

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
                // One card per request: the clicked card itself turns into
                // the working card (the invoke's card response replaces it
                // in place), and the answer later replaces that same card
                // (`updateActivity` on the clicked activity). No banner, no
                // second card. Without the clicked card's id (defensive) the
                // answer is posted as before.
                let card = activity.reply_to_id.clone().map(|id| {
                    let mut card = TaskCard::new(None);
                    card.activity_id = Some(id);
                    card
                });
                let response = match &card {
                    Some(card) => surface_mapper::invoke_card_response(card.content()),
                    None => surface_mapper::invoke_message_response(
                        "⏳ Working on it — the answer will follow here.",
                    ),
                };
                spawn_courier(
                    adapter,
                    verified,
                    tool_name,
                    args,
                    sender,
                    conversation_id,
                    recipient_id,
                    conversation_type,
                    card,
                );
                return (StatusCode::OK, axum::Json(response)).into_response();
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
            None,
        );
        return StatusCode::OK.into_response();
    }
    // #250: msteams offers `sender_table` and `azure` only, and both
    // derive the subject FROM `from.id` — so recording it again would
    // add no forensic value, only more personal data in the log. The
    // helper is here for when `upstream` lands, where the resolver
    // replaces the asserted identity and the raw id becomes the only
    // way to tell an impersonation from the victim's own session.
    let mut principal = sender_principal(sender);
    // T3 (async-ops): carry the Teams conversationReference into the agent's
    // turn so a tool like `start_operation` can persist it and deliver a result
    // back to this conversation later (the in-process embedded dispatch passes
    // the Principal straight to the tool).
    principal.conversation_ref = Some(conversation_reference_json(
        verified.reply_base(),
        conversation_id,
        recipient_id,
        &sender.from_id,
        &sender.tenant,
        &adapter.correlation_key,
    ));
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
                &principal_for_post,
                &chrome,
                &sender.tenant,
            )
            .await;
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
    // #250: msteams offers `sender_table` and `azure` only, and both
    // derive the subject FROM `from.id` — so recording it again would
    // add no forensic value, only more personal data in the log. The
    // helper is here for when `upstream` lands, where the resolver
    // replaces the asserted identity and the raw id becomes the only
    // way to tell an impersonation from the victim's own session.
    let principal = sender_principal(sender);
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
            let chart = report_vega_chart(
                adapter,
                &dispatch.result,
                &principal_for_post,
                image_url.as_deref(),
            )
            .await;
            let response_body = match render_card_content(
                adapter,
                &dispatch.result,
                image_url.as_deref(),
                chart.as_ref(),
                &chrome,
                &sender.tenant,
                &sender.from_id,
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
        adapter.correlation_key.signing(),
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
        triton_correlation::decode_with_cap_any(&token, &adapter.correlation_key, IMG_TOKEN_CAP)
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
async fn build_reply_body(
    adapter: &MsTeamsAdapter,
    bot_id: &str,
    conversation_id: &str,
    recipient_id: &str,
    result: &Value,
    image_hint: Option<String>,
    principal: &triton_core::Principal,
    chrome: &surface_mapper::CardChrome,
    tenant: &str,
) -> Value {
    // `image_hint` covers the direct render_report invocation (the
    // "Open report:" Execute), whose RESULT carries a PNG but no Report
    // component to lift a spec from — the caller minted the URL from
    // the invoked args instead.
    let image_url = image_hint.or_else(|| reply_image_url(adapter, result, tenant));
    // Native interactive chart (Teams) from peacock's Vega spec, falling back
    // to the PNG image.
    let chart = report_vega_chart(adapter, result, principal, image_url.as_deref()).await;
    if let Some(card) = render_card_content(
        adapter,
        result,
        image_url.as_deref(),
        chart.as_ref(),
        chrome,
        tenant,
        recipient_id,
    ) {
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
#[allow(clippy::too_many_arguments)]
fn render_card_content(
    adapter: &MsTeamsAdapter,
    result: &Value,
    image_url: Option<&str>,
    chart: Option<&Value>,
    chrome: &surface_mapper::CardChrome,
    tenant: &str,
    // #287: the card's recipient. Folded into the derived signing key
    // beside the tenant, so only they can submit its actions.
    sender: &str,
) -> Option<Value> {
    // Every control (legacy tool buttons and the conversation components'
    // complete-message actions alike) is signed with the same bound token
    // (#250/#287); the composition itself is key-free.
    let mut sign = |tool: &str, args: &Value| -> Option<String> {
        triton_correlation::encode_bound(
            tool,
            args,
            adapter.correlation_key.signing(),
            surface_mapper::MSTEAMS_CORRELATION_CAP,
            triton_correlation::Binding {
                platform: "msteams",
                tenant,
                sender,
            },
            Some(CARD_TOKEN_TTL_SECS),
        )
        .map_err(|e| {
            tracing::warn!(
                tool,
                error = %e,
                "msteams: dropping interactive control (correlation token too large)"
            );
        })
        .ok()
    };
    conversation_card::card_for_result(result, image_url, chart, chrome, &mut sign)
}

/// Deep-search a `render_report` result for a Vega-Lite spec (peacock exposes
/// them at `_meta.vega_specs`; also handles a `{kind:vega,spec}` component and
/// JSON-string-wrapped MCP content).
fn find_vega_spec(v: &Value) -> Option<Value> {
    match v {
        Value::Object(m) => {
            if m.get("kind").and_then(Value::as_str) == Some("vega")
                && let Some(spec) = m.get("spec").filter(|s| s.is_object())
            {
                return Some(spec.clone());
            }
            if let Some(spec) = m
                .get("vega_specs")
                .and_then(Value::as_array)
                .and_then(|a| a.iter().find(|s| s.is_object()))
            {
                return Some(spec.clone());
            }
            m.values().find_map(find_vega_spec)
        }
        Value::Array(a) => a.iter().find_map(find_vega_spec),
        Value::String(s) if s.trim_start().starts_with(['{', '[']) => {
            serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|p| find_vega_spec(&p))
        }
        _ => None,
    }
}

/// Build a native Teams Adaptive Card chart element for a result's inline
/// report: dispatch `render_report` to peacock, extract its Vega-Lite spec, and
/// map it to a `Chart.*` element (with the PNG as `fallback`). `None` when there
/// is no report, peacock is unreachable, or the report has no chart — the card
/// then shows the static image.
async fn report_vega_chart(
    adapter: &MsTeamsAdapter,
    result: &Value,
    principal: &triton_core::Principal,
    fallback_image_url: Option<&str>,
) -> Option<Value> {
    let (report_id, args) = surface_mapper::report_from_result(result)?;
    let mut rargs = if args.is_object() {
        args
    } else {
        serde_json::json!({})
    };
    rargs["report_id"] = serde_json::json!(report_id);
    let rep = adapter
        .dispatcher
        .invoke("render_report", rargs, principal.clone(), PROTOCOL)
        .await
        .ok()?;
    let spec = find_vega_spec(&rep.result)?;
    surface_mapper::chart_from_vega(&spec, fallback_image_url)
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

/// Route a click's complete message as if the user had typed it: through
/// the agent router when one is configured (a binding picks the agent),
/// else the adapter's single inbound tool. `Err` carries a text reply for
/// a router outcome that is not a dispatch (chooser, info, deny).
async fn route_typed_text(
    adapter: &Arc<MsTeamsAdapter>,
    verified: &VerifiedClaims,
    activity: &Activity,
    sender: &ResolvedSender,
    text: &str,
) -> Result<(String, Value), String> {
    let Some(router) = adapter.router.clone() else {
        return Ok(route_command(text, &adapter.inbound_tool));
    };
    let conversation_id = activity
        .conversation
        .as_ref()
        .map(|c| c.id.clone())
        .unwrap_or_default();
    let bot_id = activity
        .recipient
        .as_ref()
        .map(|r| r.id.clone())
        .unwrap_or_default();
    let ctx = triton_chat_routing::RouteCtx {
        key: triton_chat_routing::ConvKey::msteams(conversation_id.clone(), "", sender.sub.clone()),
        text,
        tenant: &sender.tenant,
        caller_sub: &sender.sub,
        pick: None,
        conversation_ref: Some(conversation_reference_json(
            verified.reply_base(),
            &conversation_id,
            &bot_id,
            &sender.from_id,
            &sender.tenant,
            &adapter.correlation_key,
        )),
    };
    match router.route(ctx).await {
        triton_chat_routing::RouteOutcome::Dispatch { agent_id, text } => {
            Ok(route_command(&text, &agent_id))
        }
        triton_chat_routing::RouteOutcome::Info { text }
        | triton_chat_routing::RouteOutcome::Deny { message: text } => Err(text),
        triton_chat_routing::RouteOutcome::Chooser { .. } => {
            Err("Type `/agents` to pick an agent, then send the request again.".to_string())
        }
    }
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
            "help" => {
                return ("help".to_string(), serde_json::json!({}));
            }
            "feedback" => {
                return (
                    "feedback".to_string(),
                    serde_json::json!({ "text": subject }),
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
    in_place: Option<TaskCard>,
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
            in_place,
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
    in_place: Option<TaskCard>,
) {
    // #250: msteams offers `sender_table` and `azure` only, and both
    // derive the subject FROM `from.id` — so recording it again would
    // add no forensic value, only more personal data in the log. The
    // helper is here for when `upstream` lands, where the resolver
    // replaces the asserted identity and the raw id becomes the only
    // way to tell an impersonation from the victim's own session.
    let mut principal = sender_principal(&sender);
    // T3 (async-ops): carry the Teams conversationReference into the agent's
    // turn so a tool like `start_operation` can persist it and deliver the
    // (terminal + progress) result back to THIS conversation later. The inline
    // path (dispatch_and_post_reply) already sets this, but the ASYNC courier is
    // what production runs (TRITON_MSTEAMS_ASYNC) and it DROPPED it — so a
    // Teams-started operation was pull-only and never pushed its result back.
    principal.conversation_ref = Some(conversation_reference_json(
        verified.reply_base(),
        &conversation_id,
        &recipient_id,
        &sender.from_id,
        &sender.tenant,
        &adapter.correlation_key,
    ));
    let principal_for_post = principal.clone();
    // See dispatch_and_post_reply: direct render_report invocations get
    // their chart URL minted from the invoked args, pre-dispatch.
    let image_hint = (tool_name == "render_report")
        .then(|| report_image_url(&adapter, &args, &sender.tenant))
        .flatten();
    let personal = conversation_type.as_deref() == Some("personal");

    // Progress. Personal: open a stream with an informative update.
    // Group: keep a typing ticker alive while the dispatch runs. A card
    // click already turned its card into the working card (in place):
    // that card is the progress, so neither runs.
    let mut stream_id: Option<String> = None;
    let mut typing_ticker: Option<tokio::task::JoinHandle<()>> = None;
    // The working card of an asynchronous upstream task, if any.
    let in_place_card = in_place.is_some();
    let mut task_card: Option<TaskCard> = in_place;
    if personal && !in_place_card {
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
    } else if !in_place_card {
        let adapter2 = adapter.clone();
        let verified2 = verified.clone();
        let conversation2 = conversation_id.clone();
        let body = serde_json::json!({
            "type": "typing",
            "from": { "id": recipient_id },
            "conversation": { "id": conversation_id },
            "recipient": { "id": sender.from_id },
        });
        // The first typing activity goes out before the dispatch starts,
        // so it is ordered before anything the turn posts (the
        // working card of an asynchronous task can follow at once).
        if let Err(e) = post_activity(&adapter2, &verified2, &conversation2, &body).await {
            tracing::debug!(error = %e, "msteams courier: typing activity failed");
        }
        typing_ticker = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(TYPING_INTERVAL).await;
                if let Err(e) = post_activity(&adapter2, &verified2, &conversation2, &body).await {
                    tracing::debug!(error = %e, "msteams courier: typing activity failed");
                }
            }
        }));
    }

    // Streaming dispatch (#635 P5): a tool that opts in emits Token
    // deltas we forward as CUMULATIVE `streaming` chunks (≤1/s, the
    // platform throttle) between the opener and the final; a buffered
    // tool yields one terminal Done and this degenerates to exactly
    // the old flow. `a2ui: None` — the courier renders the raw result.
    let started = std::time::Instant::now();
    let trace_id = principal.trace_id.clone();
    // The working card refreshes itself when a viewer opens it (Universal
    // Actions `refresh`), from the task store — so it needs one.
    let refresh = adapter
        .tasks
        .as_ref()
        .and_then(|_| refresh_token(&adapter, &trace_id, &sender.tenant, &sender.from_id))
        .map(|t| (t, sender.from_id.clone()));
    if let Some(card) = task_card.as_mut() {
        card.refresh = refresh.clone();
    }
    // F8b: with a task runner the turn is a task — stored once the
    // upstream goes asynchronous (with where its answer goes), resumed when
    // its stream breaks or is cut, bounded by TRITON_TASK_MAX_SECS.
    let dispatched = match &adapter.tasks {
        Some(runner) => {
            let mut rec = triton_tasks::TaskRecord::new(
                &trace_id,
                &conversation_id,
                &tool_name,
                Some(triton_tasks::owner_key(&principal)),
            );
            rec.delivery = Some(
                Delivery {
                    service_url: verified.reply_base().to_string(),
                    conversation_id: conversation_id.clone(),
                    adapter: adapter.name.clone(),
                    bot_app_id: adapter.audience.clone(),
                    bot_id: recipient_id.clone(),
                    user_id: sender.from_id.clone(),
                    tenant: sender.tenant.clone(),
                    sub: sender.sub.clone(),
                    scopes: sender.scopes.clone(),
                    idp: sender.idp.clone(),
                    card_activity_id: task_card.as_ref().and_then(|c| c.activity_id.clone()),
                    delivered: false,
                }
                .to_json(),
            );
            runner
                .start(
                    rec,
                    triton_tasks::Start::Dispatch(args),
                    principal,
                    PROTOCOL,
                    Default::default(),
                    triton_tasks::Persist::OnAsync,
                )
                .map(|rx| {
                    triton_tasks::as_stream_events(rx, runner.store.clone(), trace_id.clone())
                })
                .map_err(|e| TritonError::Tool(e.to_string()))
        }
        None => {
            adapter
                .dispatcher
                .invoke_streaming(&tool_name, args, principal, PROTOCOL, None)
                .await
        }
    };
    let result: Result<Value, TritonError> = match dispatched {
        Err(e) => Err(e),
        Ok(mut stream) => {
            let mut acc = String::new();
            let mut seq: u64 = 1; // the informative opener was 1
            let mut last_chunk = std::time::Instant::now();
            let mut terminal: Option<Result<Value, TritonError>> = None;
            loop {
                let ev = match next_card_turn(&mut stream, task_card.as_ref()).await {
                    CardTurn::Event(Some(ev)) => ev,
                    CardTurn::Event(None) => break,
                    CardTurn::Flush => {
                        if let Some(card) = task_card.as_mut()
                            && let Some(id) = card.activity_id.clone()
                            && card.due()
                        {
                            let body =
                                card.activity(&recipient_id, &conversation_id, &sender.from_id);
                            if let Err(e) =
                                put_activity(&adapter, &verified, &conversation_id, &id, &body)
                                    .await
                            {
                                tracing::debug!(error = %e, "msteams courier: card update failed");
                            }
                        }
                        continue;
                    }
                };
                // Mock 1l: the upstream went asynchronous — one
                // working card replaces the typing indicator, and progress
                // updates that card in place.
                match &ev {
                    triton_core::stream::StreamEvent::Task(v) if task_card.is_none() => {
                        if let Some(t) = typing_ticker.take() {
                            t.abort();
                        }
                        let mut card = TaskCard::new(None);
                        card.task(v);
                        // The post below shows the receipt; nothing is held.
                        card.pending = false;
                        card.refresh = refresh.clone();
                        let mut body =
                            card.activity(&recipient_id, &conversation_id, &sender.from_id);
                        // A personal chat's open stream is closed by the
                        // card (attachments are legal only on a final).
                        let closed_stream = stream_id.take();
                        if let Some(sid) = &closed_stream
                            && let Some(obj) = body.as_object_mut()
                        {
                            obj.insert(
                                "entities".into(),
                                serde_json::json!([{ "type": "streaminfo", "streamId": sid, "streamType": "final" }]),
                            );
                        }
                        // Teams answers a stream's final message `202 {}`:
                        // the card is the streamed message, whose activity
                        // id is the stream id (streaming-ux, "Response
                        // codes"). A refused update later still falls back
                        // to posting the answer.
                        let posted = post_activity(&adapter, &verified, &conversation_id, &body)
                            .await
                            .map(|(status, id)| (status, id.or(closed_stream)));
                        match posted {
                            Ok((status, Some(id))) if (200..300).contains(&status) => {
                                // F8b: remember the card, so a restarted
                                // Triton answers into it.
                                if let Some(runner) = &adapter.tasks {
                                    runner.store.update_running(&trace_id, |r| {
                                        if let Some(d) = r.delivery.as_mut() {
                                            d["card_activity_id"] = Value::String(id.clone());
                                        }
                                    });
                                }
                                card.activity_id = Some(id);
                            }
                            other => {
                                tracing::debug!(?other, "msteams courier: working card not posted");
                            }
                        }
                        task_card = Some(card);
                        continue;
                    }
                    // A clicked card is already the working card: the
                    // upstream's receipt and note name it now.
                    triton_core::stream::StreamEvent::Task(v) => {
                        if let Some(card) = task_card.as_mut() {
                            card.task(v);
                            if let Some(id) = card.activity_id.clone()
                                && card.due()
                            {
                                let body =
                                    card.activity(&recipient_id, &conversation_id, &sender.from_id);
                                if let Err(e) =
                                    put_activity(&adapter, &verified, &conversation_id, &id, &body)
                                        .await
                                {
                                    tracing::debug!(error = %e, "msteams courier: card update failed");
                                }
                            }
                        }
                        continue;
                    }
                    triton_core::stream::StreamEvent::Progress(v) => {
                        if let Some(card) = task_card.as_mut() {
                            card.progress(v);
                            if let Some(id) = card.activity_id.clone()
                                && card.due()
                            {
                                let body =
                                    card.activity(&recipient_id, &conversation_id, &sender.from_id);
                                if let Err(e) =
                                    put_activity(&adapter, &verified, &conversation_id, &id, &body)
                                        .await
                                {
                                    tracing::debug!(error = %e, "msteams courier: card update failed");
                                }
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
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
                    triton_core::stream::StreamEvent::Tool(_)
                    | triton_core::stream::StreamEvent::Task(_)
                    | triton_core::stream::StreamEvent::Progress(_) => {}
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
    let target = Target {
        service_url: verified.reply_base().to_string(),
        conversation_id: conversation_id.clone(),
        bot_id: recipient_id.clone(),
        user_id: sender.from_id.clone(),
        tenant: sender.tenant.clone(),
    };
    let delivered = deliver_terminal(
        &adapter,
        &target,
        task_card.as_ref().and_then(|c| c.activity_id.clone()),
        stream_id,
        result,
        &tool_name,
        &principal_for_post,
        image_hint,
        dispatch_latency_ms,
    )
    .await;
    if delivered && let Some(runner) = &adapter.tasks {
        mark_delivered(runner, &trace_id);
    }
}

/// Where a terminal answer goes.
struct Target {
    service_url: String,
    conversation_id: String,
    bot_id: String,
    user_id: String,
    tenant: String,
}

/// The one message a user sees for a failed turn, per error class —
/// never the upstream's or the transport's own text.
fn failure_notice(e: &TritonError) -> &'static str {
    triton_tasks::error_notice(e)
}

/// Deliver a turn's terminal outcome — shared by the live courier and a
/// task finished after a restart (review F5). The answer (or the
/// sanitized failure notice) replaces the working card when there is one
/// (`updateActivity`, an ambiguous transport error retried once), else
/// closes the stream or is posted; a refused update falls back to a new
/// message. One audited post line. Returns whether the user got it (2xx).
#[allow(clippy::too_many_arguments)]
async fn deliver_terminal(
    adapter: &MsTeamsAdapter,
    target: &Target,
    card_id: Option<String>,
    stream_id: Option<String>,
    outcome: Result<Value, TritonError>,
    tool_name: &str,
    principal: &Principal,
    image_hint: Option<String>,
    dispatch_latency_ms: u64,
) -> bool {
    let (mut body, failed) = match outcome {
        Ok(result_value) => {
            let chrome = fetch_chrome(adapter, principal).await;
            let body = build_reply_body(
                adapter,
                &target.bot_id,
                &target.conversation_id,
                &target.user_id,
                &result_value,
                image_hint,
                principal,
                &chrome,
                &target.tenant,
            )
            .await;
            (body, false)
        }
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "msteams courier dispatch failed");
            // The failure is the audited outcome (Dropped + error_response);
            // the notice is best-effort, so the user isn't left on a dead
            // indicator or a "working" card.
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                dispatch_latency_ms,
                Err((&e, 0, PostOutcome::Dropped, Some("error_response"))),
            );
            let notice = serde_json::json!({
                "type": "message",
                "from": { "id": target.bot_id },
                "conversation": { "id": target.conversation_id },
                "recipient": { "id": target.user_id },
                "text": failure_notice(&e),
                "textFormat": "plain",
            });
            (notice, true)
        }
    };
    let audit = |status: u16, latency_ms: u64, detail: Option<&'static str>| {
        if failed {
            return;
        }
        if (200..300).contains(&status) {
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Ok((status, PostOutcome::Posted, detail)),
            );
        } else {
            let label = if status == 0 || status >= 500 || status == 429 {
                PostOutcome::Retry
            } else {
                PostOutcome::Dropped
            };
            let provider = TritonError::Provider(format!("msteams delivery status {status}"));
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Err((&provider, status, label, None)),
            );
        }
    };
    let started = std::time::Instant::now();
    // An asynchronous task's answer replaces its working card; no
    // second message carries it.
    if let Some(id) = &card_id {
        for _ in 0..2 {
            match put_activity_to(
                adapter,
                &target.service_url,
                &target.conversation_id,
                id,
                &body,
            )
            .await
            {
                Ok(status) if (200..300).contains(&status) => {
                    audit(
                        status,
                        started.elapsed().as_millis() as u64,
                        Some("activity_updated"),
                    );
                    return true;
                }
                Ok(status) => {
                    tracing::warn!(status, "msteams: card update refused; posting the answer");
                    break;
                }
                Err(e) => tracing::warn!(error = %e, "msteams: card update failed"),
            }
        }
    }
    // Close an open stream with the final message (no streamSequence on a
    // final). Attachments are legal here — only here.
    if let Some(sid) = &stream_id
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert(
            "entities".to_string(),
            serde_json::json!([ { "type": "streaminfo", "streamId": sid, "streamType": "final" } ]),
        );
    }
    let mut outcome =
        post_activity_to(adapter, &target.service_url, &target.conversation_id, &body).await;
    if stream_id.is_some() && !matches!(&outcome, Ok((status, _)) if (200..300).contains(status)) {
        // A refused stream close (expired stream, ContentStreamNotAllowed)
        // must not cost the answer: retry once as a plain message.
        tracing::warn!("msteams courier: streamed final refused; retrying as a plain message");
        if let Some(obj) = body.as_object_mut() {
            obj.remove("entities");
        }
        outcome =
            post_activity_to(adapter, &target.service_url, &target.conversation_id, &body).await;
    }
    let latency_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok((status, _)) => {
            audit(status, latency_ms, None);
            (200..300).contains(&status)
        }
        Err(e) => {
            tracing::warn!("msteams courier activities POST failed: {e}");
            audit(0, latency_ms, None);
            false
        }
    }
}

/// Record that a task's answer reached the user (review F4): the boot
/// sweep finishes only tasks whose answer was not confirmed.
fn mark_delivered(runner: &triton_tasks::Runner, task_id: &str) {
    runner.store.update(task_id, |r| {
        if let Some(d) = r.delivery.as_mut() {
            d["delivered"] = Value::Bool(true);
        }
    });
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
    // Inbound reply: the serviceUrl is trusted-by-derivation (it rode inside the
    // JWT the verifier just checked).
    post_activity_to(adapter, verified.reply_base(), conversation_id, body).await
}

/// POST an activity to an explicit `service_url` — the out-of-band core the
/// inbound [`post_activity`] and the proactive [`OutboundCourier`] both use.
/// The caller is responsible for having validated `service_url` (inbound: JWT
/// derivation; outbound: [`jwt_verifier::service_url_host_allowed_with_extras`]).
async fn post_activity_to(
    adapter: &MsTeamsAdapter,
    service_url: &str,
    conversation_id: &str,
    body: &Value,
) -> Result<(u16, Option<String>), String> {
    // Built through the URL parser, not by string interpolation.
    //
    // `conversation_id` becomes a PATH SEGMENT. Interpolating it and then
    // screening for `/`, `..`, `?` and `#` screens the wrong alphabet:
    // `%2e%2e`, `%2F` and `\` all survive that filter and are turned into
    // real segments by the parser afterwards. `push` percent-encodes the
    // value as one segment, so there is no filter to get wrong.
    let mut url = url::Url::parse(service_url).map_err(|e| format!("msteams serviceUrl: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "msteams serviceUrl cannot be a base".to_string())?
        .pop_if_empty()
        .extend(["v3", "conversations", conversation_id, "activities"]);
    let url = url.to_string();
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

/// `updateActivity`: replace activity `activity_id` in the conversation
/// (`PUT /v3/conversations/{id}/activities/{activityId}`). Returns
/// the HTTP status. Same serviceUrl trust and URL building as
/// [`post_activity_to`].
async fn put_activity(
    adapter: &MsTeamsAdapter,
    verified: &VerifiedClaims,
    conversation_id: &str,
    activity_id: &str,
    body: &Value,
) -> Result<u16, String> {
    put_activity_to(
        adapter,
        verified.reply_base(),
        conversation_id,
        activity_id,
        body,
    )
    .await
}

/// [`put_activity`] to an explicit, already-trusted `service_url` (a
/// resumed task's stored one, which came from a verified inbound JWT).
async fn put_activity_to(
    adapter: &MsTeamsAdapter,
    service_url: &str,
    conversation_id: &str,
    activity_id: &str,
    body: &Value,
) -> Result<u16, String> {
    let mut url = url::Url::parse(service_url).map_err(|e| format!("msteams serviceUrl: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "msteams serviceUrl cannot be a base".to_string())?
        .pop_if_empty()
        .extend([
            "v3",
            "conversations",
            conversation_id,
            "activities",
            activity_id,
        ]);
    let access_token = adapter
        .token_client
        .access_token()
        .await
        .map_err(|e| format!("msteams token: {e}"))?;
    let mut body = body.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.remove("entities");
        obj.insert("id".into(), Value::String(activity_id.to_string()));
    }
    let resp = adapter
        .http
        .put(url.to_string())
        .bearer_auth(&access_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp.status().as_u16())
}

/// The working card of an asynchronous upstream task (mock 1l): a header
/// "REQUEST · WORKING · m:ss", the upstream's receipt as the title, and
/// the latest step. Updated in place at most every [`CARD_UPDATE_EVERY`];
/// a step that arrives inside the window is held and sent when the window
/// closes (a trailing flush), so the latest step always shows.
struct TaskCard {
    activity_id: Option<String>,
    started: std::time::Instant,
    last_update: std::time::Instant,
    /// A progress event arrived that no update has shown yet.
    pending: bool,
    receipt: Option<String>,
    /// The real steps so far, in order: all but the last are done.
    steps: Vec<String>,
    /// The upstream's line under the steps ("you can leave this chat…").
    note: Option<String>,
    /// Signed refresh token: a viewer's Teams client asks for the current
    /// state of the card (`refresh`, Universal Actions) when it shows it.
    refresh: Option<(String, String)>,
}

const CARD_UPDATE_EVERY: std::time::Duration = std::time::Duration::from_millis(1000);
/// The running timer in the card's header is pushed at this cadence even
/// when no new step arrives, so a long run never looks stuck at `0:00`.
const CARD_TICK_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
/// Sentinel tool of a working card's `refresh` action: `{t: task id}`.
const REFRESH_TOOL: &str = "__refresh_task";

impl TaskCard {
    fn new(receipt: Option<String>) -> Self {
        let now = std::time::Instant::now();
        Self {
            activity_id: None,
            started: now,
            // F8: the first step is due at once; the throttle applies
            // between updates, not before the first one.
            last_update: now.checked_sub(CARD_UPDATE_EVERY).unwrap_or(now),
            pending: false,
            receipt,
            steps: Vec::new(),
            note: None,
            refresh: None,
        }
    }

    /// The upstream went asynchronous: its receipt names the card, its
    /// note goes under the steps.
    fn task(&mut self, v: &Value) {
        if let Some(r) = v
            .get("receipt")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            self.receipt = Some(surface_mapper::clamp_plain_text(r));
        }
        if let Some(n) = v
            .get("note")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
        {
            self.note = Some(surface_mapper::clamp_plain_text(n));
        }
        self.pending = true;
    }

    /// The card of a task resumed after a restart: its age, and the
    /// activity it lives in.
    fn resumed(receipt: Option<String>, age: std::time::Duration, activity_id: String) -> Self {
        let mut card = Self::new(receipt);
        if let Some(started) = std::time::Instant::now().checked_sub(age) {
            card.started = started;
        }
        card.activity_id = Some(activity_id);
        card
    }

    fn progress(&mut self, v: &Value) {
        if let Some(t) = v
            .get("text")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
        {
            let t = surface_mapper::clamp_plain_text(t);
            if self.steps.last() != Some(&t) {
                self.steps.push(t);
            }
            // A bounded card: the latest steps tell the story.
            if self.steps.len() > 6 {
                self.steps.remove(0);
            }
        }
        self.pending = true;
    }

    /// Throttle: Teams rate-limits edits; one per second is plenty. `true`
    /// when an update is to be sent now; the held step is then shown.
    fn due(&mut self) -> bool {
        if self.last_update.elapsed() >= CARD_UPDATE_EVERY {
            self.last_update = std::time::Instant::now();
            self.pending = false;
            true
        } else {
            false
        }
    }

    /// When the held step is to be flushed: the end of the throttle window,
    /// if a step is held and the card exists. `None` when nothing waits.
    fn flush_at(&self) -> Option<tokio::time::Instant> {
        self.activity_id.as_ref()?;
        let tick = self.last_update + CARD_TICK_EVERY;
        let at = if self.pending {
            (self.last_update + CARD_UPDATE_EVERY).min(tick)
        } else {
            tick
        };
        Some(tokio::time::Instant::from_std(at))
    }

    /// The card content (one anatomy): the title with the running timer
    /// beside it, the real steps (`✓` done, `…` the current one), the note.
    fn content(&self) -> Value {
        let secs = self.started.elapsed().as_secs();
        let title = self.receipt.as_deref().unwrap_or("Working on it");
        let mut body = conversation_card::header(
            title,
            None,
            Some(&format!("{}:{:02}", secs / 60, secs % 60)),
        );
        let n = self.steps.len();
        for (i, step) in self.steps.iter().enumerate() {
            let last = i + 1 == n;
            body.push(serde_json::json!({
                "type": "TextBlock", "wrap": true, "spacing": "Small",
                "text": format!("{} {step}", if last { "\u{2026}" } else { "\u{2713}" }),
                "isSubtle": last,
            }));
        }
        if let Some(note) = &self.note {
            body.push(
                serde_json::json!({ "type": "TextBlock", "wrap": true, "isSubtle": true,
                "size": "Small", "text": note }),
            );
        }
        let mut card = serde_json::json!({
            "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
            "type": "AdaptiveCard",
            "version": "1.5",
            "body": body,
        });
        if let Some((token, user)) = &self.refresh {
            card["refresh"] = serde_json::json!({
                "action": { "type": "Action.Execute", "verb": surface_mapper::ACTION_VERB,
                    "data": { surface_mapper::TOKEN_DATA_KEY: token } },
                "userIds": [user],
            });
        }
        card
    }

    fn activity(&self, bot: &str, conversation: &str, user: &str) -> Value {
        serde_json::json!({
            "type": "message",
            "from": { "id": bot },
            "conversation": { "id": conversation },
            "recipient": { "id": user },
            "attachments": [{
                "contentType": "application/vnd.microsoft.card.adaptive",
                "content": self.content(),
            }],
        })
    }
}

/// The signed `refresh` token of a working card: `(REFRESH_TOOL, {t})`,
/// bound like every card token to the tenant and the card's recipient.
fn refresh_token(
    adapter: &MsTeamsAdapter,
    task_id: &str,
    tenant: &str,
    sender: &str,
) -> Option<String> {
    triton_correlation::encode_bound(
        REFRESH_TOOL,
        &serde_json::json!({ "t": task_id }),
        adapter.correlation_key.signing(),
        surface_mapper::MSTEAMS_CORRELATION_CAP,
        triton_correlation::Binding {
            platform: "msteams",
            tenant,
            sender,
        },
        Some(CARD_TOKEN_TTL_SECS),
    )
    .ok()
}

/// Answer a working card's `refresh` invoke with the card as it is now:
/// still working (timer and steps from the task store), the answer, or the
/// failure notice. Only the task's owner gets its state; anyone else (and
/// an unknown task) gets the card unchanged in meaning: still working.
async fn refresh_card(
    adapter: &Arc<MsTeamsAdapter>,
    sender: &ResolvedSender,
    args: &Value,
) -> Response {
    let principal = sender_principal(sender);
    let rec = adapter
        .tasks
        .as_ref()
        .zip(args.get("t").and_then(Value::as_str))
        .and_then(|(runner, id)| runner.store.get(id))
        .filter(|r| r.owner.as_deref() == Some(triton_tasks::owner_key(&principal).as_str()));
    let working = |rec: Option<&triton_tasks::TaskRecord>| {
        let mut card = TaskCard::new(rec.and_then(|r| r.receipt.clone()));
        card.note = rec.and_then(|r| r.note.clone());
        if let Some(r) = rec {
            card.started = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_millis(
                    (triton_tasks::now_ms() - r.created_ms).max(0) as u64,
                ))
                .unwrap_or(card.started);
            if let Some(p) = &r.progress {
                card.progress(&serde_json::json!({ "text": p.text }));
            }
        }
        card.content()
    };
    let card = match &rec {
        Some(r) if r.state == triton_tasks::TaskState::Completed => {
            let result = r.result.clone().unwrap_or_else(|| {
                serde_json::json!({ "surface": { "text": r.result_text.clone().unwrap_or_default() } })
            });
            let chrome = fetch_chrome(adapter, &principal).await;
            render_card_content(
                adapter,
                &result,
                None,
                None,
                &chrome,
                &sender.tenant,
                &sender.from_id,
            )
            .unwrap_or_else(|| text_card(&text_reply_message(&result).text))
        }
        Some(r) if r.state == triton_tasks::TaskState::Failed => {
            text_card(failure_notice(&triton_tasks::stored_error(r)))
        }
        other => working(other.as_ref()),
    };
    (
        StatusCode::OK,
        axum::Json(surface_mapper::invoke_card_response(card)),
    )
        .into_response()
}

/// A one-line card (a refreshed plain answer or a failure notice).
fn text_card(text: &str) -> Value {
    serde_json::json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard", "version": "1.5",
        "body": [ { "type": "TextBlock", "wrap": true, "text": text } ],
    })
}

/// The next thing a task loop acts on: a stream event (or its end), or the
/// working card's trailing flush.
enum CardTurn {
    Event(Option<triton_core::stream::StreamEvent>),
    Flush,
}

/// Wait for the next stream event, or for the card's trailing flush when a
/// step is held — whichever comes first.
async fn next_card_turn<S>(stream: &mut S, card: Option<&TaskCard>) -> CardTurn
where
    S: futures::Stream<Item = triton_core::stream::StreamEvent> + Unpin,
{
    use futures::StreamExt as _;
    match card.and_then(TaskCard::flush_at) {
        // Biased: an event that is already there (the answer above all) goes
        // before a due flush or timer tick.
        Some(at) => tokio::select! {
            biased;
            ev = stream.next() => CardTurn::Event(ev),
            () = tokio::time::sleep_until(at) => CardTurn::Flush,
        },
        None => CardTurn::Event(stream.next().await),
    }
}

/// Where a Teams task's answer goes (F8b), stored on its task record. All
/// fields were server-observed on the verified inbound turn. The task DB
/// therefore names conversations and senders, and its service URLs receive
/// the bot's bearer — it is credential-bearing (realizations §7).
#[derive(serde::Serialize, serde::Deserialize)]
struct Delivery {
    /// The manifest adapter that owns the conversation, and its bot.
    adapter: String,
    bot_app_id: String,
    service_url: String,
    conversation_id: String,
    bot_id: String,
    user_id: String,
    tenant: String,
    sub: String,
    scopes: Vec<String>,
    idp: Option<triton_core::IdpIdentity>,
    card_activity_id: Option<String>,
    /// The answer reached the user (a 2xx update or post).
    #[serde(default)]
    delivered: bool,
}

impl Delivery {
    fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// The inbound path's checks, applied again before a stored Teams task is
/// resumed (review F3): the service URL must pass the same host
/// allow-list, the sender must still be admitted by the CURRENT identity
/// configuration (with its current scopes), and the rebuilt principal must
/// own the task. Returns that principal, or why not.
fn revalidate(
    adapter: &MsTeamsAdapter,
    rec: &triton_tasks::TaskRecord,
    d: &Delivery,
) -> Result<Principal, &'static str> {
    if !adapter.verifier.service_url_allowed(&d.service_url) {
        return Err("service url not allowed");
    }
    let (sub, scopes, tenant) = match &adapter.identity {
        IdentityMode::SenderTable(table) => {
            let Some(r) = table.resolve(&d.user_id) else {
                return Err("sender no longer in the sender table");
            };
            let (sub, scopes, _groups, tenant) = r.into_parts();
            (sub, scopes, tenant)
        }
        IdentityMode::Azure(cfg) => {
            if !cfg.allowed_tenants.iter().any(|t| t == &d.tenant) {
                return Err("tenant no longer allowed");
            }
            (d.sub.clone(), cfg.scopes.clone(), d.tenant.clone())
        }
    };
    if sub != d.sub || tenant != d.tenant {
        return Err("sender identity changed");
    }
    let mut principal = make_principal(&sub, &scopes, &tenant);
    principal.idp = d.idp.clone();
    principal.trace_id = rec.id.clone();
    if rec.owner.as_deref() != Some(triton_tasks::owner_key(&principal).as_str()) {
        return Err("not the task's owner");
    }
    principal.conversation_ref = Some(conversation_reference_json(
        &d.service_url,
        &d.conversation_id,
        &d.bot_id,
        &d.user_id,
        &d.tenant,
        &adapter.correlation_key,
    ));
    Ok(principal)
}

/// Finish a stored Teams task this process claimed at boot (F8b): resume
/// it if it is still running, then deliver its outcome — or, if it ended
/// while no process delivered it, the stored outcome — into its card,
/// through the same [`deliver_terminal`] as the live courier.
async fn resume_task(adapter: Arc<MsTeamsAdapter>, rec: triton_tasks::TaskRecord, d: Delivery) {
    let Some(runner) = adapter.tasks.clone() else {
        return;
    };
    let principal = match revalidate(&adapter, &rec, &d) {
        Ok(p) => p,
        Err(reason) => {
            tracing::warn!(task = %rec.id, reason, "msteams: resume refused");
            // Fixed message, nothing posted: the stored target is not
            // trusted any more.
            runner.fail(&rec.id, "forbidden", "the task could not be resumed");
            mark_delivered(&runner, &rec.id);
            return;
        }
    };
    let age =
        std::time::Duration::from_millis((triton_tasks::now_ms() - rec.created_ms).max(0) as u64);
    let mut card = d.card_activity_id.clone().map(|id| {
        let mut card = TaskCard::resumed(rec.receipt.clone(), age, id);
        card.note = rec.note.clone();
        card
    });
    let mut terminal: Option<Result<Value, TritonError>> = None;
    if !rec.state.is_terminal()
        && let Some(rx) = runner.ensure_running(&rec, &principal)
    {
        let mut events = triton_tasks::as_stream_events(rx, runner.store.clone(), rec.id.clone());
        loop {
            let ev = match next_card_turn(&mut events, card.as_ref()).await {
                CardTurn::Event(Some(ev)) => ev,
                CardTurn::Event(None) => break,
                CardTurn::Flush => {
                    if let Some(card) = card.as_mut()
                        && let Some(id) = card.activity_id.clone()
                        && card.due()
                    {
                        let body = card.activity(&d.bot_id, &d.conversation_id, &d.user_id);
                        let _ = put_activity_to(
                            &adapter,
                            &d.service_url,
                            &d.conversation_id,
                            &id,
                            &body,
                        )
                        .await;
                    }
                    continue;
                }
            };
            match ev {
                triton_core::stream::StreamEvent::Progress(v) => {
                    if let Some(card) = card.as_mut() {
                        card.progress(&v);
                        if let Some(id) = card.activity_id.clone()
                            && card.due()
                        {
                            let body = card.activity(&d.bot_id, &d.conversation_id, &d.user_id);
                            let _ = put_activity_to(
                                &adapter,
                                &d.service_url,
                                &d.conversation_id,
                                &id,
                                &body,
                            )
                            .await;
                        }
                    }
                }
                triton_core::stream::StreamEvent::Done(v) => terminal = Some(Ok(v)),
                triton_core::stream::StreamEvent::Error { error, .. } => {
                    terminal = Some(Err(error));
                }
                _ => {}
            }
        }
    }
    // Not driven here (finished, expired or interrupted while down, or the
    // stream ended without a terminal event): the stored outcome stands.
    let outcome = match terminal {
        Some(t) => t,
        None => match runner.store.get(&rec.id) {
            Some(r) if r.state == triton_tasks::TaskState::Completed => {
                Ok(r.result.clone().unwrap_or_else(|| {
                    serde_json::json!({ "surface": { "text": r.result_text.clone().unwrap_or_default() } })
                }))
            }
            Some(r) if r.state == triton_tasks::TaskState::Failed => {
                Err(triton_tasks::stored_error(&r))
            }
            _ => return,
        },
    };
    let target = Target {
        service_url: d.service_url.clone(),
        conversation_id: d.conversation_id.clone(),
        bot_id: d.bot_id.clone(),
        user_id: d.user_id.clone(),
        tenant: d.tenant.clone(),
    };
    let delivered = deliver_terminal(
        &adapter,
        &target,
        card.and_then(|c| c.activity_id),
        None,
        outcome,
        &rec.tool,
        &principal,
        None,
        age.as_millis() as u64,
    )
    .await;
    if delivered {
        mark_delivered(&runner, &rec.id);
    }
}

/// Audit tool label for an agent-initiated (proactive) send — matches the
/// convention the other chat couriers use (`"outbound"`).
const OUTBOUND_TOOL: &str = "outbound";

/// Marker for the sealed conversationReference token.
const CONVERSATION_REF_MARKER: &str = "__conversation_ref";
/// Generous: five short ids and a URL.
const CONVERSATION_REF_CAP: usize = 4096;

/// Serialize an inbound Teams `conversationReference` in the flattened shape the
/// out-of-band courier consumes ([`TeamsConversationRef`]) — so the host can
/// persist it against a long-running operation and later deliver the result
/// back to this exact conversation. `bot_id` is the inbound `recipient` (the
/// bot, → outbound `from`); the user is the inbound `from` (→ outbound
/// `recipient`); `service_url` is the JWT-derived trusted reply base.
fn conversation_reference_json(
    service_url: &str,
    conversation_id: &str,
    bot_id: &str,
    user_id: &str,
    tenant: &str,
    key: &triton_correlation::KeyRing,
) -> Value {
    // SEALED. Every field here is server-observed on the inbound turn: the
    // tenant Teams asserted, the conversation it arrived on, the recipient,
    // and a `serviceUrl` already checked against the host allow-list. The
    // seal is what keeps them true when the reference comes back hours
    // later from a caller.
    //
    // Unsealed, the delivery side could only compare caller-written fields
    // with each other — `authorize` checked the reference's tenant against
    // the CALLER's tenant, and the caller wrote both. A caller could name
    // its own tenant beside another tenant's conversation and pass
    // (measured: 202 where 403 was required).
    let payload = json!({
        "service_url": service_url,
        "conversation_id": conversation_id,
        "bot_id": bot_id,
        "user_id": user_id,
        "tenant_id": tenant,
    });
    let sealed = triton_correlation::encode_with_cap(
        CONVERSATION_REF_MARKER,
        &payload,
        key.signing(),
        CONVERSATION_REF_CAP,
    )
    .unwrap_or_default();
    json!({
        // Self-describing: the delivery side (the agent's outbound-callback
        // receiver) routes to the matching triton adapter by this channel.
        "channel": "msteams",
        "ref": sealed,
    })
}

/// A persisted Teams `conversationReference`: everything an OUT-OF-BAND send
/// needs to reach the same conversation later. Captured on an inbound turn (by
/// the host) and handed back in [`OutboundRequest::reference`]. Field names are
/// the flattened, snake_case shape the host stores — not the raw Bot Framework
/// camelCase envelope — so the persisted contract is explicit.
#[derive(Debug, Clone, Deserialize)]
struct TeamsConversationRef {
    /// Per-tenant Bot Framework host (the reply base, e.g.
    /// `https://smba.trafficmanager.net/emea/`).
    service_url: String,
    /// The conversation/thread id (`conversation.id`).
    conversation_id: String,
    /// The bot's own id — becomes `from` on the outbound activity.
    #[serde(default)]
    bot_id: String,
    /// The recipient user's id — becomes `recipient` on the outbound activity.
    #[serde(default)]
    user_id: String,
    /// The Entra tenant that owns the conversation — bound against the caller's
    /// tenant in `authorize` (#113).
    ///
    /// REQUIRED, and not an `Option`. It used to be optional, with absent
    /// meaning "no binding asserted" — but the reference is caller-supplied
    /// JSON, so that let the caller decide whether to be bound: omit one
    /// field and an `outbound:send` holder could deliver into any
    /// conversation id it knew. A control the subject can switch off is not
    /// a control (crew review of #327).
    ///
    /// Costs a genuine reference nothing: `conversation_reference_json`
    /// always writes the real tenant.
    tenant_id: String,
}

impl MsTeamsAdapter {
    /// Open the sealed Teams `conversationReference`.
    ///
    /// ONE check, because there is only one thing to check: did we mint
    /// this? The seal is an HMAC over the fields observed on the inbound
    /// turn, so a valid one makes the tenant, the conversation, the
    /// recipient and the reply host server-asserted — and an invalid one
    /// makes every field worthless at once.
    ///
    /// That is why this replaced a list of per-field validations. Those
    /// could only compare caller-written values with each other: the old
    /// `authorize` checked the reference's tenant against the CALLER's
    /// tenant, and the caller wrote both, so a caller could name its own
    /// tenant beside another tenant's conversation and pass. Requiring
    /// the field removed one route and left the binding self-asserted —
    /// a required field is not a bound one.
    fn open_sealed_reference(
        &self,
        req: &OutboundRequest,
    ) -> Result<TeamsConversationRef, TritonError> {
        let sealed = req
            .reference
            .as_ref()
            .and_then(|r| r.get("ref"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TritonError::Validation(
                    "msteams outbound requires a sealed `reference.ref` minted by this \
                     adapter on the inbound turn"
                        .into(),
                )
            })?;
        let (marker, payload) = triton_correlation::decode_with_cap(
            sealed,
            self.correlation_key.signing(),
            CONVERSATION_REF_CAP,
        )
        .map_err(|_| {
            TritonError::Forbidden("msteams conversationReference seal is invalid".into())
        })?;
        if marker != CONVERSATION_REF_MARKER {
            return Err(TritonError::Forbidden(
                "sealed token is not a conversationReference".into(),
            ));
        }
        serde_json::from_value(payload)
            .map_err(|e| TritonError::Validation(format!("malformed sealed reference: {e}")))
    }
}

#[async_trait]
impl OutboundCourier for MsTeamsAdapter {
    fn protocol(&self) -> &'static str {
        PROTOCOL
    }

    /// #113 recipient/tenant binding for a proactive Teams send. The stored
    /// `serviceUrl` host MUST still be Microsoft's (or a configured extra) —
    /// re-checked before egress even though a captured ref was already
    /// allow-listed — and, when the ref names a tenant, it MUST equal the
    /// caller's: an agent may only message its own tenant's conversations.
    /// May this caller push into this conversation?
    ///
    /// Two facts, and both now come from a source the caller cannot
    /// write: the seal supplies the conversation's tenant, and the token
    /// supplies the caller's. The host allow-list check that used to live
    /// here is gone — the `serviceUrl` inside a seal was already checked
    /// on the inbound turn, and re-checking a value we signed ourselves
    /// only suggested it might not be ours.
    async fn authorize(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
    ) -> Result<(), TritonError> {
        let r = self.open_sealed_reference(req)?;
        if r.tenant_id != principal.tenant {
            return Err(TritonError::Forbidden(format!(
                "conversation belongs to tenant `{}`, caller is `{}`",
                r.tenant_id, principal.tenant
            )));
        }
        // The outbound contract's `to` is redundant for Teams — the
        // recipient is sealed. Refuse a disagreement rather than silently
        // preferring one: a caller that names a different recipient has
        // misunderstood the contract, and guessing which they meant is
        // how a message reaches the wrong person.
        if !req.to.is_empty() && req.to != r.user_id {
            return Err(TritonError::Forbidden(format!(
                "`to` names `{}` but the sealed conversation's recipient is `{}`",
                req.to, r.user_id
            )));
        }
        Ok(())
    }

    /// Render the agent's result and POST it to the stored conversation via the
    /// Bot Connector, reusing the same token + surface-mapping path as an inbound
    /// reply. Audit rides the dispatcher's `record_post` (ADR-6), sharing the
    /// caller's `trace_id`.
    async fn deliver(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
    ) -> Result<(), TritonError> {
        // `OutboundCourier` is a PUBLIC trait: `deliver` is reachable
        // without the endpoint that calls `authorize` first. Re-running the
        // checks here costs one parse and removes the possibility that a
        // future caller — or a reordering inside the endpoint — sends
        // without them (crew review of #327).
        self.authorize(req, principal).await?;
        let r = self.open_sealed_reference(req)?;
        // The SAME renderer the inbound reply path uses, not
        // `text_reply_message`.
        //
        // This feature exists to deliver a FINISHED long-running operation
        // back to the conversation that started it, and the archetypal
        // such result asks for approval. `text_reply_message` drops every
        // interactive component, so that prompt arrived as prose with
        // nothing to click — and a render error became the literal text
        // `(no content)` posted with `result: ok` (crew review of #327).
        //
        // `build_reply_body` also binds each control's correlation token
        // to (tenant, recipient), which is what stops a proactively
        // delivered card being a capability for whoever can see it
        // (#250/#287). The recipient here is the stored `user_id`.
        let chrome = fetch_chrome(self, principal).await;
        let body = build_reply_body(
            self,
            &r.bot_id,
            &r.conversation_id,
            &r.user_id,
            &req.result,
            None,
            principal,
            &chrome,
            &r.tenant_id,
        )
        .await;
        let started = std::time::Instant::now();
        let outcome = post_activity_to(self, &r.service_url, &r.conversation_id, &body).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok((status, _id)) if (200..300).contains(&status) => {
                self.dispatcher.record_post_to(
                    OUTBOUND_TOOL,
                    PROTOCOL,
                    principal,
                    latency_ms,
                    Ok((status, PostOutcome::Posted, None)),
                    Some(&r.conversation_id),
                );
                Ok(())
            }
            Ok((status, _)) => {
                // Classified like the inbound reply path, which this used
                // to contradict: only 5xx and 429 are worth retrying.
                //
                // Teams answers 403/404 when the bot has been removed from
                // a conversation, and for PROACTIVE delivery a stale
                // `conversationReference` is the expected steady state
                // rather than an exception — the whole point of the feature
                // is sending to a conversation that ended long ago. Marking
                // those `Retry` meant retrying the one failure that cannot
                // succeed, forever.
                let label = if status >= 500 || status == 429 {
                    PostOutcome::Retry
                } else {
                    PostOutcome::Dropped
                };
                let err = TritonError::Provider(format!("msteams connector returned {status}"));
                self.dispatcher.record_post_to(
                    OUTBOUND_TOOL,
                    PROTOCOL,
                    principal,
                    latency_ms,
                    Err((&err, status, label, None)),
                    Some(&r.conversation_id),
                );
                Err(err)
            }
            Err(e) => {
                let err = TritonError::Provider(format!("msteams connector: {e}"));
                self.dispatcher.record_post_to(
                    OUTBOUND_TOOL,
                    PROTOCOL,
                    principal,
                    latency_ms,
                    Err((&err, 0, PostOutcome::Retry, None)),
                    Some(&r.conversation_id),
                );
                Err(err)
            }
        }
    }
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

    /// The reference the host persists is SEALED, and opens only with the
    /// adapter's own key.
    ///
    /// It used to be a flat JSON object the courier re-validated field by
    /// field. Those validations could only compare caller-written values
    /// with each other, so they bound nothing; the seal replaced them.
    #[test]
    fn conversation_reference_is_sealed_and_opens_with_our_key() {
        let key = triton_correlation::KeyRing::single(b"a-test-correlation-key").unwrap();
        let r = conversation_reference_json(
            "https://smba.trafficmanager.net/emea/",
            "a:conv-1",
            "28:bot-1",
            "29:user-alice",
            "tenant-acme",
            &key,
        );
        assert_eq!(r["channel"], "msteams");
        // The fields are INSIDE the seal, not beside it — a caller must
        // not be able to read or rewrite them.
        assert!(
            r.get("tenant_id").is_none(),
            "fields must not sit beside the seal"
        );
        assert!(r.get("service_url").is_none());

        let sealed = r["ref"].as_str().expect("sealed token");
        let (marker, payload) =
            triton_correlation::decode_with_cap(sealed, key.signing(), CONVERSATION_REF_CAP)
                .expect("opens with our key");
        assert_eq!(marker, CONVERSATION_REF_MARKER);
        assert_eq!(payload["conversation_id"], "a:conv-1");
        assert_eq!(payload["bot_id"], "28:bot-1", "bot id → outbound `from`");
        assert_eq!(payload["user_id"], "29:user-alice");
        assert_eq!(payload["tenant_id"], "tenant-acme");

        // Another key does not open it.
        let other = triton_correlation::KeyRing::single(b"a-different-correlation-key").unwrap();
        assert!(
            triton_correlation::decode_with_cap(sealed, other.signing(), CONVERSATION_REF_CAP)
                .is_err(),
            "a seal must not open with a key that did not mint it"
        );
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

    #[test]
    fn route_command_help_and_feedback() {
        let (t, args) = route_command("/help", "assistant");
        assert_eq!(t, "help");
        assert_eq!(args, serde_json::json!({}));

        let (t, args) = route_command("/feedback great job", "assistant");
        assert_eq!(t, "feedback");
        assert_eq!(args, serde_json::json!({ "text": "great job" }));
    }
}

#[cfg(test)]
mod outbound_endpoint_tests {
    use super::*;

    #[test]
    fn a_secret_with_a_tenant_uses_the_tenant_endpoint() {
        let c = outbound_token_client(
            "bot-1".into(),
            OutboundCredential::Secret {
                secret: "shhh".into(),
                tenant_id: Some("28c0071d-815c-4ace-a3b5-9a28bde005fd".into()),
            },
            None,
        )
        .unwrap_or_else(|e| panic!("builds: {e}"));
        assert_eq!(
            c.token_url(),
            "https://login.microsoftonline.com/28c0071d-815c-4ace-a3b5-9a28bde005fd/oauth2/v2.0/token"
        );
    }

    #[test]
    fn a_secret_without_a_tenant_keeps_the_multi_tenant_endpoint() {
        let c = outbound_token_client(
            "bot-1".into(),
            OutboundCredential::Secret {
                secret: "shhh".into(),
                tenant_id: None,
            },
            None,
        )
        .unwrap_or_else(|e| panic!("builds: {e}"));
        assert_eq!(
            c.token_url(),
            "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token"
        );
    }

    #[test]
    fn the_test_override_still_wins_for_a_tenant_secret() {
        let c = outbound_token_client(
            "bot-1".into(),
            OutboundCredential::Secret {
                secret: "shhh".into(),
                tenant_id: Some("tenant-1".into()),
            },
            Some("http://127.0.0.1:1/token".into()),
        )
        .unwrap_or_else(|e| panic!("builds: {e}"));
        assert_eq!(c.token_url(), "http://127.0.0.1:1/token");
    }
}
