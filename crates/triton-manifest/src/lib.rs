//! v0.2 deployment manifest (`adapter.yaml`).
//!
//! The YAML schema is documented in `doc/requirements.md` §3.1.v0.2
//! and the architecture doc §8.1. This crate owns:
//!
//! * **Parsing** via [`Manifest::load`]. Strict enums make every
//!   closed-set discriminator (`kind`, `signature`, `identity`,
//!   `degrade.*`) refuse unknown values at deserialise time
//!   (M-MANIFEST-1 / FR-L-4 — no silent acceptance).
//! * **Cross-cutting checks** via [`Manifest::validate`]:
//!     - M-COVERAGE-1 / FR-L-5: every tool's `surface_components`
//!       is covered by every chat-channel adapter's `degrade` table.
//!     - M-SECRETS-1 / FR-L-6 / NFR-S-5: every credential field is a
//!       `vault://<path>#<field>` ref, an `env://<VARNAME>` ref, or —
//!       admitted only in dev mode with a runtime warning — a literal.
//!       Production refuses literals; both ref shapes are accepted.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Env {
    /// Local dev — admits literal credentials (with warnings).
    Dev,
    /// Substrate nonprod / prod — refuses literal credentials.
    Production,
}

/// Root manifest. Loaded from disk at boot, never mutated at runtime.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    pub version: String,
    pub adapters: BTreeMap<String, Adapter>,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolDecl>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Adapter {
    pub kind: AdapterKind,
    /// The tool plain chat text dispatches to; commands (`/narrate`
    /// etc.) keep their special routes. Defaults to the in-process
    /// `echo` tool so existing manifests are untouched. Naming an
    /// upstream agent here (Consul `agent:<name>` or a
    /// `TRITON_STATIC_UPSTREAMS` entry) routes every plain inbound
    /// message to that agent.
    #[serde(default = "default_inbound_tool")]
    pub tool: String,
    pub inbound: Inbound,
    pub outbound: Outbound,
    pub identity: Identity,
    pub degrade: BTreeMap<ComponentKind, DegradeRule>,
    pub rate_limit: RateLimit,
    /// Per-adapter 32-byte HMAC key (Vault reference in prod).
    pub correlation_key: SecretField,
    /// #94: WhatsApp Cloud API message templates, keyed by the
    /// category the upstream agent hints. Template **selection** lives
    /// in Triton (it owns the platform surface + credentials); the
    /// agent only supplies the category + body variables. Empty for
    /// adapters that don't model templates.
    #[serde(default)]
    pub templates: BTreeMap<TemplateCategory, TemplateDecl>,
}

/// WhatsApp Cloud API template category (Meta's closed set). The agent
/// hints one of these; Triton maps it to a Meta-approved template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TemplateCategory {
    Utility,
    Marketing,
    Authentication,
}

/// A Meta-approved template the operator declares in `adapter.yaml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TemplateDecl {
    /// The template name as registered with Meta / the aggregator.
    pub name: String,
    /// BCP-47 language code (e.g. `en`, `de`). Defaults to `en`.
    #[serde(default = "default_template_language")]
    pub language: String,
}

fn default_template_language() -> String {
    "en".to_string()
}

fn default_inbound_tool() -> String {
    "echo".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Inbound {
    pub kind: InboundKind,
    pub signature: SignatureScheme,
    /// Webhook secret / HMAC secret / Ed25519 public key — every
    /// signature scheme needs a key; closed-set restricts what
    /// kind, but the field name varies by scheme. We store all
    /// inbound credentials in one flat map and require that the
    /// scheme-specific key is present at parse time only for the
    /// schemes that need it (PR 12 sticks to declarative storage).
    #[serde(flatten)]
    pub credentials: BTreeMap<String, SecretField>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Outbound {
    pub kind: OutboundKind,
    #[serde(flatten)]
    pub credentials: BTreeMap<String, SecretField>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Identity {
    pub kind: IdentityKind,
    #[serde(flatten)]
    pub credentials: BTreeMap<String, SecretField>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateLimit {
    pub messages_per_sec: u32,
    pub burst: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ToolDecl {
    #[serde(default)]
    pub surface_components: Vec<ComponentKind>,
}

// ---------- closed sets ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AdapterKind {
    Telegram,
    /// Baileys-style persistent WhatsApp **Web** socket (community
    /// bridge daemon; `inbound.kind: socket`). The canonical dev/nonprod
    /// WhatsApp transport.
    WhatsappWeb,
    /// WhatsApp **Cloud API** (Meta Graph / EU aggregator;
    /// `inbound.kind: webhook`, `signature: hmac256`). The B2B-compliant
    /// transport: verified Business number + message templates (#94).
    WhatsappCloud,
    Signal,
    MsTeams,
    Discord,
    GoogleChat,
    /// Outbound-only email channel: a transactional-email HTTP API courier.
    /// No inbound webhook (email intake is an escurel event, handled
    /// elsewhere), so it declares `signature: trusted_socket` (the trust
    /// boundary is the outbound API key + the substrate egress allowlist).
    Email,
    /// #191: WhatsApp via Twilio's Business Solution Provider path
    /// (`inbound.kind: webhook`, `signature: twilio_signature`) — a
    /// parallel, independently-selectable transport alongside
    /// [`Self::WhatsappCloud`] (direct Meta Graph API) and
    /// [`Self::WhatsappWeb`] (Baileys socket bridge). Operators choose
    /// per manifest entry; this does not supersede the other two.
    TwilioWhatsapp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InboundKind {
    Webhook,
    Socket,
    LongPoll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutboundKind {
    RestApi,
    Socket,
    BotConnector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SignatureScheme {
    SecretToken,
    Hmac256,
    BotFrameworkJwt,
    Ed25519,
    GoogleOidcJwt,
    /// PR 34: signald has no message-signing envelope — the trust
    /// boundary is the network path (the daemon is reachable only on
    /// the tailnet). The adapter still resolves credentials at boot
    /// (`signald_addr`, `account`) so a misconfigured deploy fails
    /// closed; runtime relies on NFR-S-4 egress allowlist instead of
    /// a per-message signature scheme.
    TrustedSocket,
    /// #191: Twilio's `X-Twilio-Signature` header — HMAC-SHA1 over the
    /// full request URL with every `application/x-www-form-urlencoded`
    /// POST param sorted by key and appended as `key+value` (no
    /// delimiter), base64-encoded. Distinct from [`Self::Hmac256`]
    /// (which signs the raw JSON body) because Twilio's algorithm signs
    /// the URL + form params, not the body bytes. The verification
    /// itself lives in `triton-chat-twilio` (mirrors every other scheme:
    /// the adapter crate owns the crypto, this crate only owns the
    /// closed-set discriminator + required-credential wiring).
    TwilioSignature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IdentityKind {
    SenderTable,
    Azure,
    SelfEnrol,
    Upstream,
}

/// Categories of UI components a tool can declare in its
/// `surface_components`. Every chat-channel adapter's `degrade`
/// table MUST carry a rule for each category any tool uses
/// (M-COVERAGE-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ComponentKind {
    Text,
    Narration,
    Media,
    Buttons,
    Selections,
    Forms,
    Dashboard,
    /// Click-to-open references to the documents a turn wrote (chat
    /// adapters degrade them to a plain label list).
    Sources,
    /// An inline report reference: image-hosting adapters (Google Chat)
    /// expand it to the upstream-rendered chart; others drop it.
    Report,
}

impl std::fmt::Display for ComponentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ComponentKind::Text => "text",
            ComponentKind::Narration => "narration",
            ComponentKind::Media => "media",
            ComponentKind::Buttons => "buttons",
            ComponentKind::Selections => "selections",
            ComponentKind::Forms => "forms",
            ComponentKind::Dashboard => "dashboard",
            ComponentKind::Sources => "sources",
            ComponentKind::Report => "report",
        };
        f.write_str(s)
    }
}

/// How a `ComponentKind` is rendered on a given chat platform.
/// Closed set per architecture §8.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DegradeRule {
    Passthrough,
    ChunkedText,
    InlineKeyboard,
    ComponentsV2,
    AdaptiveCard,
    AdaptiveCardColumnSet,
    NumberedPrompts,
    RasterisedPng,
    CardV2,
}

/// A credential value: an `env://` reference (the production-safe shape),
/// a `vault://` reference (DECOMMISSIONED — see below), or a literal
/// string (dev-only). Parse-time we only classify the shape — production
/// refusal of literals happens in [`Manifest::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretField {
    /// Vault reference of the form `vault://<path>#<field>`. Vault was
    /// **decommissioned** with the move off the HashiCorp stack to Kamal,
    /// so this variant no longer resolves: it is still *parsed* (both path
    /// and field non-empty, per spec §3.1.v0.2) only so a stale manifest
    /// gets a clear "Vault decommissioned — migrate to `env://`" boot error
    /// rather than a confusing parse failure. Boot fails closed when the
    /// resolver hits it (`triton-secrets::ResolveError::VaultDecommissioned`).
    Vault {
        path: String,
        field: String,
    },
    /// Environment-variable reference of the form `env://<VARNAME>`.
    /// Resolved from the process environment at boot — the
    /// production-safe shape on a substrate that injects secrets as
    /// container env (GCP Secret Manager → kamal → env) rather than
    /// running Vault. Like a Vault path, the variable NAME is not
    /// secret (only the resolved value is).
    Env {
        var: String,
    },
    Literal(String),
}

impl Serialize for SecretField {
    /// Redacting JSON form for the operator-visible `/v1/manifest`
    /// endpoint. Vault refs are reproduced verbatim — the path is
    /// not secret, only the resolved value is. Literal values are
    /// masked so a curious operator with /v1/manifest access never
    /// sees the in-band credential (NFR-S-5 spirit even though dev
    /// mode admits literals).
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            SecretField::Vault { path, field } => {
                s.serialize_str(&format!("vault://{path}#{field}"))
            }
            SecretField::Env { var } => s.serialize_str(&format!("env://{var}")),
            SecretField::Literal(v) => {
                s.serialize_str(&format!("<literal:{} chars>", v.chars().count()))
            }
        }
    }
}

impl<'de> Deserialize<'de> for SecretField {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if let Some(rest) = s.strip_prefix("vault://") {
            let (path, field) = rest.split_once('#').ok_or_else(|| {
                serde::de::Error::custom(
                    "vault ref must be `vault://<path>#<field>` (missing `#` separator)",
                )
            })?;
            if path.is_empty() {
                return Err(serde::de::Error::custom(
                    "vault ref `<path>` MUST be non-empty",
                ));
            }
            if field.is_empty() {
                return Err(serde::de::Error::custom(
                    "vault ref `<field>` MUST be non-empty after `#`",
                ));
            }
            Ok(SecretField::Vault {
                path: path.to_string(),
                field: field.to_string(),
            })
        } else if let Some(var) = s.strip_prefix("env://") {
            if var.is_empty() {
                return Err(serde::de::Error::custom(
                    "env ref `env://<VARNAME>` MUST name a non-empty variable",
                ));
            }
            Ok(SecretField::Env {
                var: var.to_string(),
            })
        } else {
            Ok(SecretField::Literal(s))
        }
    }
}

#[cfg(test)]
mod component_kind_tests {
    use super::ComponentKind;

    /// The kinds an agent's `surface_components` may declare include the
    /// newer `sources` (click-to-open document references) and `report`
    /// (inline chart) — a fragment listing them must parse, and their wire
    /// names round-trip through Display.
    #[test]
    fn sources_and_report_parse_and_display() {
        let kinds: Vec<ComponentKind> =
            serde_yaml_ng::from_str("[text, dashboard, buttons, sources, report]")
                .expect("the template fragment's component list parses");
        assert_eq!(kinds.len(), 5);
        assert_eq!(kinds[3].to_string(), "sources");
        assert_eq!(kinds[4].to_string(), "report");
    }
}

#[cfg(test)]
mod secret_field_tests {
    use super::SecretField;

    fn parse(s: &str) -> Result<SecretField, serde_yaml_ng::Error> {
        serde_yaml_ng::from_str(s)
    }

    #[test]
    fn env_ref_parses_to_env_variant() {
        assert_eq!(
            parse("env://TRITON_WA_APP_SECRET").unwrap(),
            SecretField::Env {
                var: "TRITON_WA_APP_SECRET".into()
            }
        );
    }

    #[test]
    fn empty_env_ref_is_rejected() {
        assert!(
            parse("env://").is_err(),
            "env:// with no var must fail parse"
        );
    }

    #[test]
    fn vault_and_literal_shapes_still_parse() {
        assert_eq!(
            parse("vault://kv/data/apps/x#field").unwrap(),
            SecretField::Vault {
                path: "kv/data/apps/x".into(),
                field: "field".into()
            }
        );
        assert_eq!(
            parse("just-a-literal").unwrap(),
            SecretField::Literal("just-a-literal".into())
        );
    }

    #[test]
    fn env_ref_serialises_verbatim() {
        // The var NAME is not secret (like a vault path), so it is
        // reproduced for the operator-visible /v1/manifest — unlike a
        // literal, which is masked.
        let yaml = serde_yaml_ng::to_string(&SecretField::Env { var: "FOO".into() }).unwrap();
        assert_eq!(yaml.trim(), "env://FOO");
    }
}

// ---------- errors ----------

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },
    /// FR-L-5 / M-COVERAGE-1: a tool declares a surface component
    /// the adapter's `degrade` table doesn't cover.
    #[error(
        "coverage gap: tool `{tool}` declares `{component}` but adapter `{adapter}` \
         has no degrade rule for it (M-COVERAGE-1, FR-L-5)"
    )]
    CoverageGap {
        tool: String,
        component: ComponentKind,
        adapter: String,
    },
    /// FR-L-6 / NFR-S-5 / M-SECRETS-1: literal credential in prod.
    #[error(
        "literal credential at `{path}` is forbidden in production — \
         use an `env://<VARNAME>` ref injected by the substrate (GCP Secret \
         Manager → kamal `.kamal/secrets`) (M-SECRETS-1, FR-L-6)"
    )]
    LiteralCredentialInProd { path: String },
    /// FR-L-4: only documented schema versions are accepted.
    #[error("unsupported manifest version `{actual}`; supported: {supported:?}")]
    UnsupportedVersion {
        actual: String,
        supported: &'static [&'static str],
    },
    /// FR-L-4: a closed-set scheme requires a specific credential
    /// key that isn't present (e.g. `signature: secret_token`
    /// without a `secret` field).
    #[error(
        "adapter `{adapter}` declares `{scheme}` but the required credential field \
         `{field}` is missing (FR-L-4)"
    )]
    MissingSchemeCredential {
        adapter: String,
        scheme: String,
        field: String,
    },
    /// A zero `messages_per_sec` or `burst` would make the token
    /// bucket reject every inbound (a permanently-closed adapter) and
    /// divide-by-zero the retry-after math. Almost always a typo, so
    /// fail the manifest rather than boot a dead adapter.
    #[error(
        "adapter `{adapter}` has a zero rate_limit ({field} = 0): the token bucket \
         would refuse every message. Set a positive value (FR-L-4)"
    )]
    ZeroRateLimit { adapter: String, field: String },
}

// ---------- API ----------

/// Manifest schema versions Triton accepts. Add entries here
/// (never overwrite) as the schema evolves; FR-L-4 demands the
/// boot-time gate refuse anything else.
pub const SUPPORTED_VERSIONS: &[&str] = &["0.2"];

impl Manifest {
    /// Parse a YAML file. Closed-set kind/signature/identity/degrade
    /// values refuse unknowns here via serde — that's the
    /// M-MANIFEST-1 boot-time gate.
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let bytes = std::fs::read(path).map_err(|e| ManifestError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let parsed: Self = serde_yaml_ng::from_slice(&bytes).map_err(|e| ManifestError::Parse {
            path: path.display().to_string(),
            source: e,
        })?;
        if !SUPPORTED_VERSIONS.contains(&parsed.version.as_str()) {
            return Err(ManifestError::UnsupportedVersion {
                actual: parsed.version.clone(),
                supported: SUPPORTED_VERSIONS,
            });
        }
        Ok(parsed)
    }

    /// Cross-cutting checks. On success returns a list of warnings
    /// (e.g. literal credentials in dev). On failure returns the
    /// first violation that would refuse production boot.
    pub fn validate(&self, env: Env) -> Result<Vec<String>, ManifestError> {
        let mut warnings = Vec::new();

        // M-COVERAGE-1 / FR-L-5: every adapter's degrade table
        // covers every tool's declared surface_components.
        for (tool_name, tool) in &self.tools {
            for component in &tool.surface_components {
                for (adapter_name, adapter) in &self.adapters {
                    if !adapter.degrade.contains_key(component) {
                        return Err(ManifestError::CoverageGap {
                            tool: tool_name.clone(),
                            component: *component,
                            adapter: adapter_name.clone(),
                        });
                    }
                }
            }
        }

        // FR-L-4: every scheme requires specific credential keys
        // (e.g. `secret_token` → `secret`). The flattened-map
        // representation can't catch this at parse time; we
        // enforce it here.
        for (adapter_name, adapter) in &self.adapters {
            check_required_credentials(adapter, adapter_name)?;
        }

        // A zero rate limit permanently closes the adapter (the token
        // bucket never admits) — reject it rather than boot a dead
        // adapter. TokenBucket::try_take relies on this guard.
        for (adapter_name, adapter) in &self.adapters {
            let field = if adapter.rate_limit.messages_per_sec == 0 {
                Some("messages_per_sec")
            } else if adapter.rate_limit.burst == 0 {
                Some("burst")
            } else {
                None
            };
            if let Some(field) = field {
                return Err(ManifestError::ZeroRateLimit {
                    adapter: adapter_name.clone(),
                    field: field.to_string(),
                });
            }
        }

        // M-SECRETS-1 / FR-L-6 / NFR-S-5: every credential field
        // is either `vault://` or — in dev mode — a literal that
        // surfaces a warning.
        for (adapter_name, adapter) in &self.adapters {
            visit_secrets(adapter, adapter_name, env, &mut warnings)?;
        }

        Ok(warnings)
    }
}

fn check_required_credentials(adapter: &Adapter, name: &str) -> Result<(), ManifestError> {
    // Each closed-set value declares the credential field it
    // can't function without. Keep this table close to the enum
    // definitions so a new variant is impossible to ship without
    // also stating its required field.
    let inbound_required: &[(&str, &str)] = match adapter.inbound.signature {
        SignatureScheme::SecretToken => &[("secret_token", "secret")],
        SignatureScheme::Hmac256 => &[("hmac256", "secret")],
        SignatureScheme::Ed25519 => &[("ed25519", "public_key")],
        // Bot Framework: validate against Microsoft's published
        // JWKS. The adapter still needs the bot's Microsoft App ID
        // (matches the JWT's `aud` claim) — without it any
        // Microsoft-signed Bot Framework JWT would verify, so
        // `inbound.audience` is mandatory (PR 35).
        SignatureScheme::BotFrameworkJwt => &[("bot_framework_jwt", "audience")],
        // Google Chat: validate against Google's published certs
        // (JWKS). No shared secret, but the adapter MUST be told
        // which `aud` value to require — the bot's project number
        // — or any Google-signed JWT for any bot would verify
        // (PR 33).
        SignatureScheme::GoogleOidcJwt => &[("google_oidc_jwt", "audience")],
        // `trusted_socket` means "no per-message signature; trust the
        // socket/session". The required inbound credentials depend on
        // the adapter:
        //   * Signal (PR 34): the signald daemon address + the bot's
        //     Signal phone number, resolved at boot so a Vault-ref
        //     typo fails closed (M-SECRETS-1 / FR-L-4 / FR-L-6).
        //   * Discord Gateway: auth is the bot token presented in
        //     IDENTIFY, which lives in `outbound.token` — no inbound
        //     credential is required.
        SignatureScheme::TrustedSocket => match adapter.kind {
            AdapterKind::Signal => &[
                ("trusted_socket", "signald_addr"),
                ("trusted_socket", "account"),
            ],
            _ => &[],
        },
        // #191: the Auth Token doubles as both the HMAC-SHA1 signing key
        // (inbound) and the HTTP Basic password (outbound) — Twilio issues
        // only one secret per account. `secret` matches the naming other
        // HMAC-family schemes already use.
        SignatureScheme::TwilioSignature => &[("twilio_signature", "secret")],
    };
    for (scheme, field) in inbound_required {
        if !adapter.inbound.credentials.contains_key(*field) {
            return Err(ManifestError::MissingSchemeCredential {
                adapter: name.to_string(),
                scheme: format!("inbound.signature={scheme}"),
                field: (*field).to_string(),
            });
        }
    }

    let outbound_required: &[(&str, &str)] = match adapter.outbound.kind {
        OutboundKind::RestApi => &[("rest_api", "token")],
        OutboundKind::Socket => &[], // session-locality, no token
        // Bot Framework outbound mints OAuth2 access tokens from
        // `login.microsoftonline.com/botframework.com/oauth2/v2.0/
        // token` via the client_credentials grant — needs the
        // bot's `client_id` + `client_secret` resolved at boot
        // (PR 35). Mirrors the WhatsApp `phone_number_id`-style
        // declared-required wiring.
        OutboundKind::BotConnector => &[
            ("bot_connector", "client_id"),
            ("bot_connector", "client_secret"),
        ],
    };
    for (scheme, field) in outbound_required {
        if !adapter.outbound.credentials.contains_key(*field) {
            return Err(ManifestError::MissingSchemeCredential {
                adapter: name.to_string(),
                scheme: format!("outbound.kind={scheme}"),
                field: (*field).to_string(),
            });
        }
    }

    // #94: WhatsApp Cloud API addresses the per-bot sender by
    // `phone_number_id` embedded in the outbound URL path
    // (`/v18.0/{phone_number_id}/messages`). Without it the courier
    // has no target; the manifest must carry it next to `token`.
    // Telegram puts its routing id in the bot token itself, and the
    // WhatsApp **Web bridge** (`kind: whatsapp_web`) replies over the
    // bridge socket, not the Graph API — so this rule fires only for
    // `kind: whatsapp_cloud`.
    if adapter.kind == AdapterKind::WhatsappCloud
        && !adapter.outbound.credentials.contains_key("phone_number_id")
    {
        return Err(ManifestError::MissingSchemeCredential {
            adapter: name.to_string(),
            scheme: "kind=whatsapp_cloud".to_string(),
            field: "phone_number_id".to_string(),
        });
    }

    // #191: Twilio's Messaging API addresses the account by `AccountSid`
    // embedded in the outbound URL path
    // (`/2010-04-01/Accounts/{account_sid}/Messages.json`) AND uses it as
    // the HTTP Basic auth username (`token` is the password). Without it
    // the courier has no account to post to or authenticate as.
    if adapter.kind == AdapterKind::TwilioWhatsapp
        && !adapter.outbound.credentials.contains_key("account_sid")
    {
        return Err(ManifestError::MissingSchemeCredential {
            adapter: name.to_string(),
            scheme: "kind=twilio_whatsapp".to_string(),
            field: "account_sid".to_string(),
        });
    }

    // #191: Twilio signs the exact externally-visible URL it POSTed to —
    // axum's own view of the request URI cannot be trusted to reproduce
    // that behind the substrate's reverse proxy (12-factor VII). The
    // operator must configure it explicitly rather than have the adapter
    // guess.
    if adapter.kind == AdapterKind::TwilioWhatsapp
        && !adapter.inbound.credentials.contains_key("public_url")
    {
        return Err(ManifestError::MissingSchemeCredential {
            adapter: name.to_string(),
            scheme: "kind=twilio_whatsapp".to_string(),
            field: "public_url".to_string(),
        });
    }

    let identity_required = match adapter.identity.kind {
        IdentityKind::SenderTable => Some(("sender_table", "table")),
        IdentityKind::SelfEnrol => Some(("self_enrol", "fallback_table")),
        IdentityKind::Azure => Some(("azure", "azure_identity")),
        IdentityKind::Upstream => Some(("upstream", "resolver_tool")),
    };
    if let Some((scheme, field)) = identity_required
        && !adapter.identity.credentials.contains_key(field)
    {
        return Err(ManifestError::MissingSchemeCredential {
            adapter: name.to_string(),
            scheme: format!("identity.kind={scheme}"),
            field: field.to_string(),
        });
    }
    Ok(())
}

fn visit_secrets(
    adapter: &Adapter,
    name: &str,
    env: Env,
    warnings: &mut Vec<String>,
) -> Result<(), ManifestError> {
    let mut paths: Vec<(String, &SecretField)> = Vec::new();
    for (k, v) in &adapter.inbound.credentials {
        // #191: `public_url` (twilio_signature) is the adapter's own
        // externally-visible webhook URL, not a secret — like
        // `resolver_tool` below, exempt it so a production manifest can
        // state it as a literal.
        if k == "public_url" {
            continue;
        }
        paths.push((format!("adapters.{name}.inbound.{k}"), v));
    }
    for (k, v) in &adapter.outbound.credentials {
        paths.push((format!("adapters.{name}.outbound.{k}"), v));
    }
    for (k, v) in &adapter.identity.credentials {
        // `resolver_tool` (identity.kind: upstream) is the NAME of a public
        // upstream tool (declared in TRITON_STATIC_UPSTREAMS), not a secret —
        // exempt it from M-SECRETS-1 so a PRODUCTION manifest can delegate
        // identity resolution with a literal tool name. The other identity
        // credential fields (sender_table `table`, azure `azure_identity`,
        // self_enrol `fallback_table`) carry sender→claims data and stay
        // secrets.
        if k == "resolver_tool" {
            continue;
        }
        paths.push((format!("adapters.{name}.identity.{k}"), v));
    }
    paths.push((
        format!("adapters.{name}.correlation_key"),
        &adapter.correlation_key,
    ));
    for (path, field) in paths {
        if let SecretField::Literal(_) = field {
            match env {
                Env::Production => return Err(ManifestError::LiteralCredentialInProd { path }),
                Env::Dev => warnings.push(format!("dev manifest carries literal credential at {path} (production deploy would refuse)")),
            }
        }
    }
    Ok(())
}
