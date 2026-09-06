//! Agent multiplexing: route one chat channel to many agents.
//!
//! A single chat identity per platform (one Google Chat bot, one Teams bot,
//! one WhatsApp number) fronts potentially dozens of agents. This crate owns
//! the *routing* half of that: given
//!
//! * a **caller-scoped** [`AgentCatalog`] (already filtered to the agents this
//!   caller is entitled to — entitlement is enforced fail-closed by the host),
//! * the incoming message text (bot-mention already stripped by the adapter),
//! * the conversation's current sticky **binding** (read by the host), and
//! * the caller's tenant,
//!
//! [`resolve`] decides which agent handles the turn and whether the binding
//! should change. It is **pure** — no I/O. The host reads/writes the binding
//! (escurel, async) and, once an agent is chosen, runs its existing
//! per-platform `route_command` to turn the remaining text into `(tool, args)`.
//! That split keeps `/narrate`, `/help`, `/echo` and their tests untouched.
//!
//! ## Two axes, never conflated
//!
//! *Which agent* a message selects lives entirely here. It **never** moves the
//! caller-derived tenant (the data/crypto boundary): the tenant is an input,
//! and genuine cross-tenant isolation is a separate deployment, not a routing
//! decision.
//!
//! ## Precedence
//!
//! reserved command → explicit switch (`/use X`, `@X`) → sticky binding →
//! tenant default → first-contact chooser → global default.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A selectable agent, as the routing layer sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDescriptor {
    /// Dispatch id — the `X-Triton-Tool` / roster `tool_name`.
    pub id: String,
    /// Human-facing name shown in choosers and `/agents`.
    pub display: String,
    /// One-line description shown in choosers and `/agents`.
    pub description: String,
    /// True for the deployment-wide fallback agent (the "front door").
    pub default: bool,
}

impl AgentDescriptor {
    pub fn new(
        id: impl Into<String>,
        display: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            display: display.into(),
            description: description.into(),
            default: false,
        }
    }

    pub fn as_default(mut self) -> Self {
        self.default = true;
        self
    }
}

/// The set of agents a caller may reach, plus the tenant/global defaults.
///
/// Implementations MUST be caller-scoped: `list` returns only agents the
/// current caller is entitled to. The host builds this from its roster after
/// applying `may(caller, agent)`.
pub trait AgentCatalog {
    /// Every agent this caller may use, in a stable display order.
    fn list(&self) -> Vec<AgentDescriptor>;

    /// Exact lookup by dispatch id (must also respect entitlement).
    fn get(&self, id: &str) -> Option<AgentDescriptor> {
        self.list().into_iter().find(|a| a.id == id)
    }

    /// The default agent pinned for `tenant`, if any (before the global default).
    fn default_for(&self, _tenant: &str) -> Option<AgentDescriptor> {
        None
    }

    /// The deployment-wide fallback agent (`default == true`), if any.
    fn global_default(&self) -> Option<AgentDescriptor> {
        self.list().into_iter().find(|a| a.default)
    }
}

/// Per-conversation sticky binding storage. Async in practice (escurel), so
/// [`resolve`] does not call it — the host loads the current binding first and
/// persists [`Resolution`]'s `bind` afterwards. Provided here for the in-memory
/// dev fallback and to give hosts one contract to implement.
pub trait BindingStore {
    fn get(&self, key: &ConvKey) -> Option<String>;
    fn set(&self, key: &ConvKey, agent_id: &str, by: &str);
    fn clear(&self, key: &ConvKey);
}

/// Normalized conversation identity used to key sticky bindings across surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConvKey {
    pub platform: String,
    /// Space / chat / conversation id.
    pub space: String,
    /// Thread id within the space ("" when the surface has no threads).
    pub thread: String,
    /// Caller subject (per-user binding within a shared space).
    pub caller: String,
}

impl ConvKey {
    pub fn new(
        platform: impl Into<String>,
        space: impl Into<String>,
        thread: impl Into<String>,
        caller: impl Into<String>,
    ) -> Self {
        Self {
            platform: platform.into(),
            space: space.into(),
            thread: thread.into(),
            caller: caller.into(),
        }
    }

    pub fn googlechat(
        space: impl Into<String>,
        thread: impl Into<String>,
        caller: impl Into<String>,
    ) -> Self {
        Self::new("googlechat", space, thread, caller)
    }

    pub fn msteams(
        conversation: impl Into<String>,
        thread: impl Into<String>,
        caller: impl Into<String>,
    ) -> Self {
        Self::new("msteams", conversation, thread, caller)
    }

    /// A stable, filesystem/URL-safe id for use as an escurel page id segment.
    /// Deterministic across processes so a binding survives restarts/replicas.
    pub fn storage_id(&self) -> String {
        fn san(s: &str) -> String {
            s.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect()
        }
        format!(
            "{}__{}__{}__{}",
            san(&self.platform),
            san(&self.space),
            san(&self.thread),
            san(&self.caller),
        )
    }
}

/// Why an agent was chosen (for logging / audit / source line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    /// `/use X` or `@X` in the message text.
    Switch,
    /// The conversation's existing sticky binding.
    Sticky,
    /// A tenant-pinned default.
    TenantDefault,
    /// The deployment-wide default (or the only entitled agent).
    GlobalDefault,
    /// The user picked from a chooser card.
    ChooserPick,
}

/// Why a chooser is being shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChooserReason {
    /// No binding, no default, and more than one entitled agent.
    FirstContact,
    /// `/use`/`@` named an agent that matched nothing entitled.
    UnknownAgent,
    /// `/use`/`@` matched more than one agent.
    Ambiguous,
    /// A sticky binding pointed at an agent no longer entitled/present.
    BindingGone,
}

/// Reserved multiplexing commands the adapter handles locally (no dispatch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservedCmd {
    /// `/agents` — list the entitled agents.
    ListAgents,
    /// `/whoami` — report the current binding.
    WhoAmI,
}

/// The outcome of routing one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Dispatch to `agent_id`. The adapter runs its existing
    /// `route_command(text, &agent_id)` to produce `(tool, args)`.
    /// `bind` is `Some(id)` when the sticky binding should be (re)written.
    Selection {
        agent_id: String,
        /// The message with any `/use X` / `@X` switch prefix removed.
        text: String,
        source: SelectionSource,
        bind: Option<String>,
    },
    /// Present a chooser. `candidates` are entitled agents; `pending_text` is
    /// the message that triggered it (carried through the pick so it can be
    /// replayed statelessly).
    Chooser {
        candidates: Vec<AgentDescriptor>,
        reason: ChooserReason,
        pending_text: String,
    },
    /// A reserved command handled locally by the adapter.
    Reserved(ReservedCmd),
}

/// Sentinel tool name a chooser button carries back through the signed click
/// round-trip. Args: `{ "id": <agent id>, "msg": <pending text> }`.
pub const USE_AGENT_TOOL: &str = "__use_agent";

/// What an adapter should do with a routed message. Platform-neutral: the
/// adapter turns this into its own reply (running `route_command` for a
/// dispatch, or building a chooser card / text reply otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Hand the turn to `agent_id`. The adapter runs its existing
    /// `route_command(text, &agent_id)` to derive `(tool, args)`.
    Dispatch { agent_id: String, text: String },
    /// Show a chooser. The adapter renders `candidates` as buttons carrying a
    /// signed [`USE_AGENT_TOOL`] token (`{id, msg}`), so a pick replays
    /// `pending_text` statelessly.
    Chooser {
        candidates: Vec<AgentDescriptor>,
        reason: ChooserReason,
        pending_text: String,
    },
    /// A plain-text reply the adapter posts verbatim (e.g. `/agents`,
    /// `/whoami`), already formatted by the host.
    Info { text: String },
    /// Entitlement denied — the adapter posts `message` and does not dispatch.
    Deny { message: String },
}

/// Everything the host needs to route one turn.
pub struct RouteCtx<'a> {
    /// Normalized conversation identity for the sticky binding.
    pub key: ConvKey,
    /// User message, bot-mention already stripped.
    pub text: &'a str,
    /// Caller-derived tenant (input only; routing never changes it).
    pub tenant: &'a str,
    /// Verified caller subject, for entitlement + binding audit.
    pub caller_sub: &'a str,
    /// Set when this turn is a chooser-button click: `(agent_id, replayed_msg)`
    /// decoded from a [`USE_AGENT_TOOL`] token. Bypasses `resolve`.
    pub pick: Option<(String, String)>,
}

/// The host-provided router: resolves which agent handles a turn, reading and
/// writing the (async, escurel-backed) sticky binding and enforcing the
/// fail-closed `may(caller, agent)` entitlement gate. Adapters hold an
/// `Option<Arc<dyn AgentRouter>>`; when `None`, they fall back to the legacy
/// single-tool `route_command(text, &adapter.inbound_tool)` path unchanged.
pub trait AgentRouter: Send + Sync {
    fn route<'a>(&'a self, ctx: RouteCtx<'a>) -> futures::future::BoxFuture<'a, RouteOutcome>;
}

/// Route one incoming message. See the crate/precedence docs.
///
/// * `text` — user message, bot-mention already stripped.
/// * `current_binding` — the agent id this conversation is bound to, if any.
/// * `tenant` — caller-derived tenant (input only; never changed here).
/// * `catalog` — caller-scoped entitled agents.
pub fn resolve(
    text: &str,
    current_binding: Option<&str>,
    tenant: &str,
    catalog: &dyn AgentCatalog,
) -> Resolution {
    let trimmed = text.trim();

    // 1. Reserved multiplexing commands (highest precedence).
    if let Some(cmd) = parse_reserved(trimmed) {
        return Resolution::Reserved(cmd);
    }

    // 2. Explicit switch: `/use X [rest]` (sticky) or `@X rest` (one-turn).
    if let Some(sw) = parse_switch(trimmed) {
        return match match_one(catalog, sw.name) {
            MatchResult::One(agent) => Resolution::Selection {
                agent_id: agent.id,
                text: sw.rest.to_string(),
                source: SelectionSource::Switch,
                bind: if sw.sticky {
                    Some(sw_bind(catalog, sw.name))
                } else {
                    None
                },
            },
            MatchResult::None => Resolution::Chooser {
                candidates: catalog.list(),
                reason: ChooserReason::UnknownAgent,
                pending_text: sw.rest.to_string(),
            },
            MatchResult::Many(candidates) => Resolution::Chooser {
                candidates,
                reason: ChooserReason::Ambiguous,
                pending_text: sw.rest.to_string(),
            },
        };
    }

    // 3. Sticky binding — if it still points at an entitled agent.
    if let Some(bound) = current_binding {
        match catalog.get(bound) {
            Some(agent) => {
                return Resolution::Selection {
                    agent_id: agent.id,
                    text: trimmed.to_string(),
                    source: SelectionSource::Sticky,
                    bind: None,
                };
            }
            None => {
                // Bound agent gone / no longer entitled: re-choose.
                return Resolution::Chooser {
                    candidates: catalog.list(),
                    reason: ChooserReason::BindingGone,
                    pending_text: trimmed.to_string(),
                };
            }
        }
    }

    // 4. Tenant default.
    if let Some(agent) = catalog.default_for(tenant) {
        let id = agent.id.clone();
        return Resolution::Selection {
            agent_id: agent.id,
            text: trimmed.to_string(),
            source: SelectionSource::TenantDefault,
            bind: Some(id),
        };
    }

    // 5/6. First-contact chooser vs. global default / single entitled agent.
    let entitled = catalog.list();
    match entitled.len() {
        0 => Resolution::Chooser {
            candidates: entitled,
            reason: ChooserReason::FirstContact,
            pending_text: trimmed.to_string(),
        },
        1 => {
            let agent = entitled.into_iter().next().unwrap();
            let id = agent.id.clone();
            Resolution::Selection {
                agent_id: agent.id,
                text: trimmed.to_string(),
                source: SelectionSource::GlobalDefault,
                bind: Some(id),
            }
        }
        _ => {
            // Multiple entitled agents. A configured global default is the
            // deliberate fallback; otherwise show the first-contact chooser.
            match catalog.global_default() {
                Some(agent) => {
                    let id = agent.id.clone();
                    Resolution::Selection {
                        agent_id: agent.id,
                        text: trimmed.to_string(),
                        source: SelectionSource::GlobalDefault,
                        bind: Some(id),
                    }
                }
                None => Resolution::Chooser {
                    candidates: entitled,
                    reason: ChooserReason::FirstContact,
                    pending_text: trimmed.to_string(),
                },
            }
        }
    }
}

/// Resolve a chooser-button click (`__use_agent` sentinel) into a selection.
///
/// `id` is the clicked agent; `msg` is the replayed pending message. Fails
/// closed: if `id` is not in the (caller-scoped) catalog, returns `None` and
/// the adapter should re-show the chooser / deny.
pub fn confirm_pick(id: &str, msg: &str, catalog: &dyn AgentCatalog) -> Option<Resolution> {
    catalog.get(id).map(|agent| Resolution::Selection {
        agent_id: agent.id.clone(),
        text: msg.to_string(),
        source: SelectionSource::ChooserPick,
        bind: Some(agent.id),
    })
}

// --- internals -------------------------------------------------------------

fn parse_reserved(text: &str) -> Option<ReservedCmd> {
    let word = text.strip_prefix('/')?;
    let head = word.split_whitespace().next().unwrap_or(word);
    match head.to_ascii_lowercase().as_str() {
        "agents" | "agent" => Some(ReservedCmd::ListAgents),
        "whoami" => Some(ReservedCmd::WhoAmI),
        _ => None,
    }
}

struct Switch<'a> {
    name: &'a str,
    rest: &'a str,
    sticky: bool,
}

fn parse_switch(text: &str) -> Option<Switch<'_>> {
    // `/use <name> [rest]` — sticky.
    if let Some(rest) = text.strip_prefix('/') {
        let head = rest.split_whitespace().next().unwrap_or(rest);
        if head.eq_ignore_ascii_case("use") {
            let after = rest[head.len()..].trim_start();
            let (name, rest) = split_first_word(after);
            if name.is_empty() {
                return None;
            }
            return Some(Switch {
                name,
                rest,
                sticky: true,
            });
        }
        return None;
    }
    // `@<name> <rest>` — one-turn switch (must be at the very start).
    if let Some(after) = text.strip_prefix('@') {
        let (name, rest) = split_first_word(after);
        if name.is_empty() {
            return None;
        }
        return Some(Switch {
            name,
            rest,
            sticky: false,
        });
    }
    None
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

enum MatchResult {
    One(AgentDescriptor),
    None,
    Many(Vec<AgentDescriptor>),
}

/// Case-insensitive fuzzy match of a switch name against the catalog:
/// exact id/display → then unique prefix → then unique substring.
fn match_one(catalog: &dyn AgentCatalog, name: &str) -> MatchResult {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return MatchResult::None;
    }
    let agents = catalog.list();

    let exact: Vec<_> = agents
        .iter()
        .filter(|a| a.id.eq_ignore_ascii_case(&name) || a.display.eq_ignore_ascii_case(&name))
        .cloned()
        .collect();
    if exact.len() == 1 {
        return MatchResult::One(exact.into_iter().next().unwrap());
    }
    if exact.len() > 1 {
        return MatchResult::Many(exact);
    }

    let prefix: Vec<_> = agents
        .iter()
        .filter(|a| {
            a.id.to_ascii_lowercase().starts_with(&name)
                || a.display.to_ascii_lowercase().starts_with(&name)
        })
        .cloned()
        .collect();
    match prefix.len() {
        1 => return MatchResult::One(prefix.into_iter().next().unwrap()),
        n if n > 1 => return MatchResult::Many(prefix),
        _ => {}
    }

    let sub: Vec<_> = agents
        .iter()
        .filter(|a| {
            a.id.to_ascii_lowercase().contains(&name)
                || a.display.to_ascii_lowercase().contains(&name)
        })
        .cloned()
        .collect();
    match sub.len() {
        1 => MatchResult::One(sub.into_iter().next().unwrap()),
        0 => MatchResult::None,
        _ => MatchResult::Many(sub),
    }
}

fn sw_bind(catalog: &dyn AgentCatalog, name: &str) -> String {
    match match_one(catalog, name) {
        MatchResult::One(a) => a.id,
        _ => name.to_string(),
    }
}

/// A simple in-memory [`AgentCatalog`] — the dev fallback and a base the host
/// can build from its roster. Construct with [`InMemoryCatalog::new`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryCatalog {
    agents: Vec<AgentDescriptor>,
    tenant_defaults: HashMap<String, String>,
}

impl InMemoryCatalog {
    pub fn new(agents: Vec<AgentDescriptor>) -> Self {
        Self {
            agents,
            tenant_defaults: HashMap::new(),
        }
    }

    pub fn with_tenant_default(
        mut self,
        tenant: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        self.tenant_defaults.insert(tenant.into(), agent_id.into());
        self
    }
}

impl AgentCatalog for InMemoryCatalog {
    fn list(&self) -> Vec<AgentDescriptor> {
        self.agents.clone()
    }

    fn default_for(&self, tenant: &str) -> Option<AgentDescriptor> {
        let id = self.tenant_defaults.get(tenant)?;
        self.agents.iter().find(|a| &a.id == id).cloned()
    }
}

/// A non-persistent [`BindingStore`] for single-replica dev only. Not safe for
/// the shared production tenant (state is lost on restart and not shared across
/// replicas) — the host wires an escurel-backed store there.
#[derive(Debug, Default)]
pub struct InMemoryBindingStore {
    inner: std::sync::Mutex<HashMap<String, String>>,
}

impl InMemoryBindingStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl BindingStore for InMemoryBindingStore {
    fn get(&self, key: &ConvKey) -> Option<String> {
        self.inner.lock().unwrap().get(&key.storage_id()).cloned()
    }

    fn set(&self, key: &ConvKey, agent_id: &str, _by: &str) {
        self.inner
            .lock()
            .unwrap()
            .insert(key.storage_id(), agent_id.to_string());
    }

    fn clear(&self, key: &ConvKey) {
        self.inner.lock().unwrap().remove(&key.storage_id());
    }
}
