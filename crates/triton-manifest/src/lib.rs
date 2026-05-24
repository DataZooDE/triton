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
//!     - M-SECRETS-1 / FR-L-6 / NFR-S-5: every credential field is
//!       either `vault://<path>#<field>` or admitted only in dev
//!       mode with a runtime warning. Production refuses literals.

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
    pub inbound: Inbound,
    pub outbound: Outbound,
    pub identity: Identity,
    pub degrade: BTreeMap<ComponentKind, DegradeRule>,
    pub rate_limit: RateLimit,
    /// Per-adapter 32-byte HMAC key (Vault reference in prod).
    pub correlation_key: SecretField,
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
    WhatsappWeb,
    Signal,
    MsTeams,
    Discord,
    GoogleChat,
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

/// A credential value. Either a Vault reference (production-safe)
/// or a literal string (dev-only). Parse-time we only distinguish
/// the two — production refusal happens in [`Manifest::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretField {
    /// Vault reference of the form `vault://<path>#<field>`, both
    /// path and field non-empty (per spec §3.1.v0.2).
    Vault {
        path: String,
        field: String,
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
        } else {
            Ok(SecretField::Literal(s))
        }
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
         use `vault://<path>#<field>` (M-SECRETS-1, FR-L-6)"
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
    let inbound_required = match adapter.inbound.signature {
        SignatureScheme::SecretToken => Some(("secret_token", "secret")),
        SignatureScheme::Hmac256 => Some(("hmac256", "secret")),
        SignatureScheme::Ed25519 => Some(("ed25519", "public_key")),
        // Bot Framework + Google OIDC validate against the
        // platform's published OpenID metadata; the adapter holds
        // no shared secret of its own.
        SignatureScheme::BotFrameworkJwt | SignatureScheme::GoogleOidcJwt => None,
    };
    if let Some((scheme, field)) = inbound_required
        && !adapter.inbound.credentials.contains_key(field)
    {
        return Err(ManifestError::MissingSchemeCredential {
            adapter: name.to_string(),
            scheme: format!("inbound.signature={scheme}"),
            field: field.to_string(),
        });
    }

    let outbound_required = match adapter.outbound.kind {
        OutboundKind::RestApi => Some(("rest_api", "token")),
        OutboundKind::Socket => None, // session-locality, no token
        OutboundKind::BotConnector => Some(("bot_connector", "azure_identity")),
    };
    if let Some((scheme, field)) = outbound_required
        && !adapter.outbound.credentials.contains_key(field)
    {
        return Err(ManifestError::MissingSchemeCredential {
            adapter: name.to_string(),
            scheme: format!("outbound.kind={scheme}"),
            field: field.to_string(),
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
        paths.push((format!("adapters.{name}.inbound.{k}"), v));
    }
    for (k, v) in &adapter.outbound.credentials {
        paths.push((format!("adapters.{name}.outbound.{k}"), v));
    }
    for (k, v) in &adapter.identity.credentials {
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
