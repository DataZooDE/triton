//! A2UI envelope builders (ADR-4, FR-A-3..A-5).
//!
//! Two **independent** modules, one per version. Python experiment
//! finding (realizations §1): schema evolution stays compartmentalised
//! when each version owns its own builder file. The dispatcher and
//! tools never branch on version — only the adapter-edge wrap layer
//! does.
//!
//! Tools that emit a UI surface return JSON of shape
//! `{ "surface": { "components": [ ... ] } }`. The adapter, when the
//! caller has negotiated A2UI, deserialises the `surface` value
//! into [`Surface`] and hands it to the version's `build` function.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod ge;
pub mod text;
pub mod v08;
pub mod v09;

/// Canonical, version-agnostic surface a tool returns. Designed so
/// adding a new version means adding one more builder file — never
/// changing this struct in a way that breaks existing builders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Surface {
    pub components: Vec<Component>,
}

/// Component vocabulary. Both v0.8 and v0.9 builders must handle
/// every variant; appending a new variant means touching both
/// `v08::component_to_json` and `v09::component_to_json` (the
/// compiler enforces this via the non-exhaustive `match`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Component {
    Text {
        value: String,
        /// Display names for the `[[skill::id]]` wikilinks inside `value`,
        /// keyed by **id**. A wikilink must never reach a reader as raw
        /// syntax, but the label can only come from a read the *caller* is
        /// entitled to make — so an id the caller may not see simply has no
        /// entry and the renderer degrades to the bare id. Absence is the
        /// mechanism, which is why this is a map rather than a list of pairs.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pills: BTreeMap<String, String>,
    },
    Button {
        label: String,
        /// Legacy tool-call button: re-invoke `tool` with `args`. Empty when
        /// the button carries a stateless [`Action`] instead.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tool: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        args: Value,
        /// A stateless action: send one complete new message, or open a URL.
        /// When present it wins over `tool`/`args`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<Action>,
        /// Optional MCP-App the button references (`ui://<authority>/…`,
        /// params in the query) — the report-as-surface pattern. Hosts
        /// that can embed resources may auto-open it; others ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource: Option<String>,
        /// The one action the surface is actually asking for. A bool rather
        /// than an open style field: hosts own their look, and an agent that
        /// can name colours will eventually name the wrong one.
        #[serde(default, skip_serializing_if = "is_false")]
        primary: bool,
    },
    Narration {
        text: String,
    },
    /// Pick one option from a list and re-invoke a tool with the
    /// chosen value bound to `args_key`. This is the "buttons with
    /// state" rung of the L6′ degradation ladder (FR-D-3): cheaper
    /// surfaces (Telegram inline keyboards) flatten to numbered
    /// prompts, richer surfaces (web) render a SegmentedButton.
    ///
    /// Conversation extension: with `submit` set, the chosen option's
    /// `value` fills the `{<args_key>}` placeholder of `submit.template`, and
    /// the click sends that ONE complete message as a new turn (no
    /// multi-turn, no tool re-invocation). `style` picks radio buttons or a
    /// dropdown where the channel distinguishes them.
    Selection {
        prompt: String,
        options: Vec<SelectionOption>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tool: String,
        #[serde(default)]
        args_key: String,
        #[serde(default, skip_serializing_if = "SelectionStyle::is_default")]
        style: SelectionStyle,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submit: Option<SubmitTemplate>,
    },
    /// Multi-field structured input that submits as one tool call.
    /// Used by surfaces that can host real forms (web, Adaptive
    /// Cards); flattens to a sequence of numbered prompts on
    /// text-only surfaces.
    ///
    /// Conversation extension: with `send_template` set, submitting fills the
    /// template's `{<field name>}` placeholders from the inputs and sends that
    /// ONE complete message as a new turn. Hosts show the template ("will be
    /// sent as …", label `preview_label`) before the user submits.
    Form {
        title: String,
        fields: Vec<FormField>,
        submit_label: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        send_template: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview_label: Option<String>,
    },
    /// Read-only grid of summary tiles. The rasteriser turns this
    /// into a PNG for surfaces that can't render tables (Telegram);
    /// web renders it as native Material cards.
    Dashboard {
        title: String,
        tiles: Vec<DashboardTile>,
    },
    /// A rendered report to embed inline: the adapter dispatches
    /// `render_report(report_id, args)` to the report upstream and
    /// shows the returned chart image in the same reply — so an agent
    /// can surface a rich chart without the user clicking a button.
    /// Surfaces that can't render images degrade to a caption/link.
    Report {
        report_id: String,
        #[serde(default)]
        args: Value,
        /// An optional inline preview, for hosts that cannot dispatch
        /// `render_report` at all (a phone, a chat channel). Carried
        /// *alongside* `report_id` rather than instead of it, so one envelope
        /// serves both: the Explorer dispatches, the phone draws these.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        series: Vec<f64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        labels: Vec<String>,
    },
    /// A readable diff of the page a proposal would write (BR-HIL-1) — the
    /// component a human-in-the-loop approval is built on: what the consultant
    /// signs off is what this shows.
    Diff {
        lines: Vec<DiffLine>,
    },
    /// Click-to-open references — "Sources" — to the documents a reply
    /// touched (created/updated records). Each item names an MCP-App
    /// resource; hosts render a chip row and open an item's resource
    /// ONLY on click. The items' resources deliberately sit one level
    /// down so an auto-open lift (first resource-bearing component)
    /// never fires on sources. Text-only surfaces degrade to a list of
    /// labels.
    Sources {
        items: Vec<SourceItem>,
    },
    /// A list of answer rows (1a–1c), each optionally actionable: a deep
    /// link (`open_url`) or a complete follow-up request (`send_message`).
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        items: Vec<ListItem>,
    },
    /// Rows with named columns. Cells are display strings keyed by column
    /// `key`; a row may carry one action like a [`ListItem`].
    Table {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        columns: Vec<TableColumn>,
        rows: Vec<TableRow>,
    },
    /// One-click complete requests (1d–1f): each chip sends its `message`
    /// verbatim as a new turn.
    Chips {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        chips: Vec<Chip>,
    },
    /// A nested map (2a/2b): nodes with counts, leaf chips and an optional
    /// action. Expand/collapse and the "only gaps" filter are client-side
    /// where the channel can do it; nothing round-trips for either.
    Tree {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        nodes: Vec<TreeNode>,
        /// Label of the client-side "only gaps" toggle; `None` = no toggle.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gaps_toggle: Option<String>,
    },
    /// An info or warning line (stale data, assumptions, limits).
    Notice {
        #[serde(default, skip_serializing_if = "Tone::is_default")]
        tone: Tone,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        text: String,
    },
    /// Real progress of a long run (1j–1l): steps done/active/pending and an
    /// optional overall fraction in `0.0..=1.0`.
    Progress {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        steps: Vec<ProgressStep>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fraction: Option<f64>,
    },
    /// Label/value pairs; a pair with `before` renders as a change
    /// (`before → value`), which is how a draft receipt shows its diff.
    KeyValue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        pairs: Vec<KeyValuePair>,
    },
    /// An embedded read-only page (2d). Hosts that can frame a URL show
    /// `url`; every other host renders `fallback` inline instead.
    Embed {
        url: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
        fallback: Box<Component>,
    },
}

/// The dialect wire form of a button's action: a legacy button keeps its
/// `{tool, args}` byte-for-byte; a stateless [`Action`] emits itself
/// (`{type: send_message|open_url, …}`). Shared by the v0.8 and v0.9
/// builders — it is a property of the canonical model, not of either wire.
pub(crate) fn button_action(tool: &str, args: &Value, action: Option<&Action>) -> Value {
    match action {
        Some(a) => serde_json::json!(a),
        None => serde_json::json!({ "tool": tool, "args": args }),
    }
}

/// A stateless action. A `send_message` click becomes a NEW user message
/// with exactly `text` — the same as if the user had typed it; `open_url`
/// opens a deep link. Neither needs the card to be remembered anywhere: the
/// channel signs the payload into the control itself (triton-correlation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    SendMessage { text: String },
    OpenUrl { url: String },
}

/// The submit of a template-carrying [`Component::Selection`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitTemplate {
    pub label: String,
    /// The complete request with `{<args_key>}` where the choice goes.
    pub template: String,
    /// Caption for the "will be sent as" preview; hosts default it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStyle {
    #[default]
    Dropdown,
    Radio,
}

impl SelectionStyle {
    fn is_default(&self) -> bool {
        *self == SelectionStyle::Dropdown
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListItem {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableColumn {
    pub key: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRow {
    pub cells: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chip {
    pub label: String,
    /// The complete request a click sends, verbatim.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeCount {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeNode {
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counts: Vec<TreeCount>,
    /// This node itself is a gap (the "only gaps" filter keeps it and its
    /// ancestors).
    #[serde(default, skip_serializing_if = "is_false")]
    pub gap: bool,
    /// Initially expanded where the host collapses nodes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub expanded: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chips: Vec<Chip>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
    /// Caption of the node's `action` control (hosts default to `label`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_label: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// The caption for this node's action control.
    pub fn action_caption(&self) -> &str {
        self.action_label.as_deref().unwrap_or(&self.label)
    }

    /// True when this node or any descendant is a gap.
    pub fn has_gap(&self) -> bool {
        self.gap || self.children.iter().any(TreeNode::has_gap)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Info,
    Warn,
}

impl Tone {
    fn is_default(&self) -> bool {
        *self == Tone::Info
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Done,
    Active,
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressStep {
    pub label: String,
    pub state: StepState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyValuePair {
    pub label: String,
    pub value: String,
    /// The previous value, for a change (`before → value`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
}

/// One line of a [`Component::Diff`], tagged by its `op` so a host switches
/// on a single key.
///
/// `Fold` is the phone-shaped variant: a diff is mostly unchanged context, and
/// drawing all of it buries the change being approved. It carries the hidden
/// lines *with it* rather than eliding them — expanding must not need a second
/// round trip, because a diff the consultant cannot fully inspect is not a
/// diff they can meaningfully approve.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DiffLine {
    Add {
        text: String,
    },
    Del {
        text: String,
    },
    Ctx {
        text: String,
    },
    Fold {
        count: usize,
        #[serde(default)]
        hidden: Vec<DiffLine>,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A [`Component::Diff`] as plain unified-diff text — the degrade for every
/// channel with no diff primitive of its own.
///
/// Version-agnostic on purpose: this operates on the canonical model, not on
/// either wire shape, so it is not the shared base between v0.8 and v0.9 that
/// ADR-4 forbids.
///
/// A fold stays folded, and says how many lines it is holding. Expanding it
/// here would undo the only reason it exists — a diff is mostly context, and
/// on a channel where the reader cannot collapse it again, that context buries
/// the change being approved.
pub fn diff_to_text(lines: &[DiffLine]) -> String {
    let mut out = String::new();
    for line in lines {
        match line {
            DiffLine::Add { text } => out.push_str(&format!("+ {text}\n")),
            DiffLine::Del { text } => out.push_str(&format!("- {text}\n")),
            DiffLine::Ctx { text } => out.push_str(&format!("  {text}\n")),
            DiffLine::Fold { count, .. } => {
                out.push_str(&format!("@@ {count} unchanged lines @@\n"))
            }
        }
    }
    out.trim_end().to_string()
}

/// One clickable reference in a [`Component::Sources`] row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceItem {
    pub label: String,
    /// The MCP-App resource the click opens (`ui://<authority>/…`).
    pub resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionOption {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormField {
    pub name: String,
    pub label: String,
    pub kind: FormFieldKind,
    #[serde(default)]
    pub required: bool,
    /// Hint text shown in an empty control. Never submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// A value that survives an untouched submit — distinct from
    /// `placeholder` on purpose: a hint that submitted itself would put words
    /// the agent never proposed into a record a consultant signed off.
    ///
    /// Free JSON because the three field kinds carry three value types. Named
    /// `default_value` in Rust only because `default` is a keyword.
    #[serde(default, rename = "default", skip_serializing_if = "Option::is_none")]
    pub default_value: Option<Value>,
    /// The choices of a `select` field (ignored by every other kind).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<SelectionOption>,
    /// Radio buttons or a dropdown for a `select` field, where the host
    /// distinguishes them (1e, 1m).
    #[serde(default, skip_serializing_if = "SelectionStyle::is_default")]
    pub style: SelectionStyle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormFieldKind {
    String,
    Integer,
    Boolean,
    /// Pick one of `options` (an enum).
    Select,
    /// Free multi-line text.
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardTile {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trend: Option<String>,
}

/// A2UI versions Triton speaks (mirrors [`crate::A2uiVersion`] but
/// owned by the builder module). PR 10 ships v0.8 + v0.9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuilderVersion {
    V08,
    V09,
}

/// Parse a tool's raw JSON result as a [`Surface`]. Fails when the
/// tool advertises `returns_a2ui = true` but emits a shape that
/// doesn't deserialise — caller is expected to surface the error
/// (typically as a `TritonError::Tool`) so the bug shows up at the
/// API boundary instead of being silently downgraded to raw JSON.
pub fn extract_surface(result: &Value) -> Result<Surface, String> {
    let surface = result
        .get("surface")
        .ok_or_else(|| "tool emitted no `surface` field".to_string())?;
    let surface: Surface = serde_json::from_value(surface.clone())
        .map_err(|e| format!("invalid A2UI surface: {e}"))?;
    for c in &surface.components {
        c.check()?;
    }
    Ok(Surface {
        components: surface
            .components
            .into_iter()
            .filter_map(Component::sanitized)
            .collect(),
    })
}

impl Action {
    /// Only `https://` deep links are actionable (F6). The producer is
    /// LLM-driven, so a prompt-injected `javascript:`/`file:`/`ms-…:` link
    /// must never become a one-tap action on any channel. (A local-only
    /// `http://` host allowance: [`crate::links`].)
    pub fn is_safe(&self) -> bool {
        match self {
            Action::OpenUrl { url } => crate::links::is_allowed_link(url),
            Action::SendMessage { .. } => true,
        }
    }
}

fn safe(a: Option<Action>) -> Option<Action> {
    a.filter(Action::is_safe)
}

fn sanitize_node(mut n: TreeNode) -> TreeNode {
    n.action = safe(n.action);
    n.children = n.children.into_iter().map(sanitize_node).collect();
    n
}

impl Component {
    /// True for the conversation components and the stateless-action /
    /// template controls — everything rendered by the conversation
    /// mappings (`a2ui::text`, `ge`, the Teams `conversation_card`) rather
    /// than by a channel's legacy per-kind rules. The ONE place that knows
    /// the set: the match is exhaustive, so a new kind cannot be added
    /// without deciding here, and every mapper's `c if c.is_conversation()`
    /// arm follows (F8).
    pub fn is_conversation(&self) -> bool {
        match self {
            Component::Button { action, .. } => action.is_some(),
            Component::Selection { submit, .. } => submit.is_some(),
            Component::Form { send_template, .. } => send_template.is_some(),
            Component::List { .. }
            | Component::Table { .. }
            | Component::Chips { .. }
            | Component::Tree { .. }
            | Component::Notice { .. }
            | Component::Progress { .. }
            | Component::KeyValue { .. }
            | Component::Embed { .. } => true,
            Component::Text { .. }
            | Component::Narration { .. }
            | Component::Dashboard { .. }
            | Component::Report { .. }
            | Component::Diff { .. }
            | Component::Sources { .. } => false,
        }
    }

    /// The conversation components proper (no legacy-kind extension): the
    /// kinds with no field renames on the v0.8/v0.9 dialect wire.
    pub fn is_conversation_block(&self) -> bool {
        self.is_conversation()
            && !matches!(
                self,
                Component::Button { .. } | Component::Selection { .. } | Component::Form { .. }
            )
    }

    /// Structural rules serde cannot express. An embed's fallback is what a
    /// host shows INSTEAD of the frame, so it must be renderable inline: an
    /// embed nested there would never be shown and would recurse.
    pub fn check(&self) -> Result<(), String> {
        if let Component::Embed { fallback, .. } = self
            && matches!(**fallback, Component::Embed { .. })
        {
            return Err("invalid A2UI surface: an embed's fallback must not be an embed".into());
        }
        // A template Selection needs the placeholder name its choice fills;
        // without it every channel would guess differently (F9).
        if let Component::Selection {
            args_key,
            submit: Some(_),
            ..
        } = self
            && args_key.trim().is_empty()
        {
            return Err("invalid A2UI surface: a selection with `submit` needs `args_key`".into());
        }
        Ok(())
    }

    /// Parse one raw component (`{kind, …}`), apply [`Component::check`]
    /// and [`Component::sanitized`].
    pub fn parse(raw: &Value) -> Option<Component> {
        let c: Component = serde_json::from_value(raw.clone()).ok()?;
        c.check().ok()?;
        c.sanitized()
    }

    /// Drop every unsafe link ([`Action::is_safe`]): a row or node keeps
    /// its content and loses only the action; a button whose only purpose
    /// was the link disappears. Applied once here, so every channel sees
    /// the same, already-safe surface.
    pub fn sanitized(self) -> Option<Component> {
        Some(match self {
            Component::Button {
                action: Some(a), ..
            } if !a.is_safe() => return None,
            Component::List { title, items } => Component::List {
                title,
                items: items
                    .into_iter()
                    .map(|mut i| {
                        i.action = safe(i.action);
                        i
                    })
                    .collect(),
            },
            Component::Table {
                title,
                columns,
                rows,
            } => Component::Table {
                title,
                columns,
                rows: rows
                    .into_iter()
                    .map(|mut r| {
                        r.action = safe(r.action);
                        r
                    })
                    .collect(),
            },
            Component::Tree {
                title,
                nodes,
                gaps_toggle,
            } => Component::Tree {
                title,
                nodes: nodes.into_iter().map(sanitize_node).collect(),
                gaps_toggle,
            },
            Component::Embed {
                url,
                title,
                height,
                fallback,
            } => Component::Embed {
                url,
                title,
                height,
                fallback: Box::new((*fallback).sanitized()?),
            },
            other => other,
        })
    }
}

/// Build a version-specific envelope around a [`Surface`].
pub fn build_envelope(surface: &Surface, version: BuilderVersion) -> Value {
    match version {
        BuilderVersion::V08 => v08::build(surface),
        BuilderVersion::V09 => v09::build(surface),
    }
}

impl From<crate::A2uiVersion> for BuilderVersion {
    fn from(v: crate::A2uiVersion) -> Self {
        match v {
            crate::A2uiVersion::V08 => BuilderVersion::V08,
            crate::A2uiVersion::V09 => BuilderVersion::V09,
        }
    }
}

/// Reverse of the v0.9 [`build_envelope`]: recover the canonical
/// [`Surface`] from an already-negotiated envelope. The chat-channel
/// preview endpoint (`POST /v1/surface/render`) uses this so it can map a
/// turn the caller is *already showing* — the Explorer POSTs the bubble's
/// raw envelope — without re-invoking the tool (which, for an LLM agent,
/// would run a whole new turn and could yield a different surface).
///
/// It searches (bounded) for the `stream` array, tolerating the
/// per-protocol wrapping (`{result: …}` for REST/A2A, `{structuredContent:
/// …}` for MCP). Returns `None` when there is no v0.9 stream — a raw
/// `{surface}` (handled directly by [`extract_surface`]) or a v0.8 envelope
/// both yield `None`, and the caller falls back accordingly.
pub fn envelope_to_surface(value: &Value) -> Option<Surface> {
    let stream = find_stream(value, 0)?;
    let components = stream
        .iter()
        .map(v09_node_to_component)
        .collect::<Option<Vec<_>>>()?;
    Some(Surface { components })
}

/// Bounded search for a v0.9 `stream` array, descending through whatever
/// per-protocol envelope wraps it.
fn find_stream(value: &Value, depth: usize) -> Option<&Vec<Value>> {
    if depth > 6 {
        return None;
    }
    if let Some(stream) = value.get("stream").and_then(Value::as_array) {
        return Some(stream);
    }
    if let Value::Object(map) = value {
        for v in map.values() {
            if let Some(stream) = find_stream(v, depth + 1) {
                return Some(stream);
            }
        }
    }
    None
}

/// Turn one v0.9 stream node back into its canonical [`Component`] JSON
/// (internally `kind`-tagged) and deserialise it. This is the exact
/// inverse of `v09::component_to_json`: `type` → `kind`, Text's `text` →
/// `value`, and the button `action` object is unwrapped back to flat
/// `tool` / `args`. Every other field name already matches the struct, so
/// it rides through unchanged.
fn v09_node_to_component(node: &Value) -> Option<Component> {
    let ty = node.get("type")?.as_str()?;
    let mut obj = serde_json::Map::new();
    obj.insert("kind".to_string(), Value::String(ty.to_string()));
    let empty = || Value::Object(serde_json::Map::new());
    match ty {
        "text" => {
            obj.insert("value".into(), node.get("text")?.clone());
            if let Some(p) = node.get("pills") {
                obj.insert("pills".into(), p.clone());
            }
        }
        "narration" => {
            obj.insert("text".into(), node.get("text")?.clone());
        }
        "button" => {
            obj.insert("label".into(), node.get("label")?.clone());
            let action = node.get("action")?;
            if action.get("type").is_some() {
                // A stateless action rides as-is; the legacy pair is empty.
                obj.insert("action".into(), action.clone());
            } else {
                obj.insert("tool".into(), action.get("tool")?.clone());
                obj.insert(
                    "args".into(),
                    action.get("args").cloned().unwrap_or_else(empty),
                );
            }
            if let Some(r) = node.get("resource") {
                obj.insert("resource".into(), r.clone());
            }
            if let Some(p) = node.get("primary") {
                obj.insert("primary".into(), p.clone());
            }
        }
        // Every other kind carries its canonical field names unchanged.
        "selection" | "form" | "dashboard" | "report" | "diff" | "sources" | "list" | "table"
        | "chips" | "tree" | "notice" | "progress" | "key_value" | "embed" => {
            let map = node.as_object()?;
            for (k, v) in map {
                if k != "type" {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        _ => return None,
    }
    serde_json::from_value(Value::Object(obj)).ok()
}

#[cfg(test)]
mod diff_degrade_tests {
    use super::*;

    /// The text degrade of a diff, for the channels with no diff primitive.
    ///
    /// A fold stays folded. It is tempting to expand it — the lines are right
    /// there — but the fold exists precisely because that context buries the
    /// change being approved, and a chat reader cannot collapse it again. The
    /// marker names the count so nothing looks silently omitted.
    #[test]
    fn a_fold_degrades_to_a_marker_and_never_expands() {
        let text = diff_to_text(&[
            DiffLine::Ctx {
                text: "tier: enterprise".into(),
            },
            DiffLine::Fold {
                count: 42,
                hidden: vec![DiffLine::Ctx {
                    text: "buried line".into(),
                }],
            },
            DiffLine::Del {
                text: "status: prospect".into(),
            },
            DiffLine::Add {
                text: "status: active".into(),
            },
        ]);
        assert!(text.contains("@@ 42 unchanged lines @@"), "{text}");
        assert!(!text.contains("buried line"), "{text}");
        // Positive control: the change itself, with its markers, IS rendered —
        // so "buried line is absent" is not just an empty string passing.
        assert!(text.contains("- status: prospect"), "{text}");
        assert!(text.contains("+ status: active"), "{text}");
        assert!(text.contains("  tier: enterprise"), "{text}");
    }
}

#[cfg(test)]
mod reverse_tests {
    use super::*;
    use serde_json::json;

    fn sample_surface() -> Surface {
        Surface {
            components: vec![
                Component::Text {
                    value: "hi [[customer::h]]".into(),
                    pills: [("h".to_string(), "Hoffmann".to_string())]
                        .into_iter()
                        .collect(),
                },
                Component::Narration {
                    text: "note".into(),
                },
                Component::Button {
                    label: "Open".into(),
                    tool: "render_report".into(),
                    args: json!({ "id": "x" }),
                    resource: Some("ui://peacock/x".into()),
                    primary: true,
                    action: None,
                },
                Component::Selection {
                    prompt: "pick".into(),
                    options: vec![SelectionOption {
                        label: "A".into(),
                        value: "a".into(),
                    }],
                    tool: "t".into(),
                    args_key: "k".into(),
                    style: Default::default(),
                    submit: None,
                },
                Component::Form {
                    title: "F".into(),
                    fields: vec![FormField {
                        name: "n".into(),
                        label: "L".into(),
                        kind: FormFieldKind::Integer,
                        required: true,
                        placeholder: Some("how many".into()),
                        default_value: Some(json!(2)),
                        options: Vec::new(),
                        style: Default::default(),
                    }],
                    submit_label: "Go".into(),
                    tool: "t".into(),
                    preview_label: None,
                    send_template: None,
                },
                Component::Dashboard {
                    title: "D".into(),
                    tiles: vec![DashboardTile {
                        label: "x".into(),
                        value: "1".into(),
                        trend: Some("up".into()),
                    }],
                },
                Component::Report {
                    report_id: "r".into(),
                    args: json!({ "a": 1 }),
                    title: Some("R".into()),
                    series: vec![1.0, 2.0],
                    labels: vec!["a".into(), "b".into()],
                },
                Component::Diff {
                    lines: vec![
                        DiffLine::Ctx { text: "c".into() },
                        DiffLine::Fold {
                            count: 3,
                            hidden: vec![DiffLine::Ctx { text: "h".into() }],
                        },
                        DiffLine::Del { text: "d".into() },
                        DiffLine::Add { text: "a".into() },
                    ],
                },
                Component::Sources {
                    items: vec![SourceItem {
                        label: "s".into(),
                        resource: "ui://peacock/d".into(),
                    }],
                },
            ],
        }
    }

    /// The whole point: `build` then reverse is the identity, across every
    /// component variant — so the preview endpoint can trust a round-tripped
    /// surface to render byte-for-byte what the live courier would.
    #[test]
    fn v09_envelope_round_trips_to_surface() {
        let s = sample_surface();
        let env = v09::build(&s);
        let back = envelope_to_surface(&env).expect("v0.9 envelope is reversible");
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&s).unwrap(),
        );
    }

    /// REST/A2A nest the envelope under `result`; the finder descends into it.
    #[test]
    fn tolerates_a_result_wrapper() {
        let s = sample_surface();
        let wrapped = json!({ "result": v09::build(&s) });
        assert!(envelope_to_surface(&wrapped).is_some());
    }

    /// A raw `{surface}` has no stream → `None`, so the endpoint falls back to
    /// `extract_surface` instead of double-handling it.
    #[test]
    fn raw_surface_is_not_a_v09_envelope() {
        let raw = json!({ "surface": { "components": [] } });
        assert!(envelope_to_surface(&raw).is_none());
    }
}

#[cfg(test)]
mod resource_roundtrip_tests {
    use super::*;
    use serde_json::json;

    /// A surface BUTTON may reference an MCP-App (`resource: ui://…`) — the
    /// agent's report-as-surface pattern. The negotiated A2UI path must
    /// PRESERVE it through extract → build (it was silently dropped before),
    /// or hosts can never auto-open the referenced report.
    #[test]
    fn button_resource_survives_the_negotiated_round_trip() {
        let raw = json!({
            "surface": { "components": [{
                "kind": "button",
                "label": "Open report: nba-report",
                "tool": "render_report",
                "args": { "report_id": "nba-report", "params": { "account": "beverages-gmbh" } },
                "resource": "ui://peacock/nba-report?account=beverages-gmbh",
            }]}
        });
        let surface = extract_surface(&raw).expect("valid surface");
        for version in [BuilderVersion::V08, BuilderVersion::V09] {
            let envelope = build_envelope(&surface, version);
            let s = envelope.to_string();
            assert!(
                s.contains("ui://peacock/nba-report?account=beverages-gmbh"),
                "{version:?} must keep the button's resource: {s}"
            );
        }
        // A button WITHOUT a resource stays byte-shaped as before (no null key).
        let plain = extract_surface(&json!({
            "surface": { "components": [{
                "kind": "button", "label": "Ask again", "tool": "assistant", "args": {}
            }]}
        }))
        .expect("valid surface");
        let envelope = build_envelope(&plain, BuilderVersion::V09);
        assert!(!envelope.to_string().contains("resource"));
    }

    /// The SOURCES component: click-to-open references to the documents the
    /// agent touched this turn. Items carry their `resource` one level down
    /// (`items[].resource`) — NEVER as a top-level component key — so a
    /// host's "auto-open the first resource-bearing component" lift skips
    /// sources by construction and only an explicit click opens one.
    #[test]
    fn sources_round_trips_and_never_leaks_a_top_level_resource() {
        let raw = json!({
            "surface": { "components": [{
                "kind": "sources",
                "items": [
                    { "label": "account · initech-corp",
                      "resource": "ui://peacock/document?skill=account&id=initech-corp" },
                    { "label": "email · email-demo-1",
                      "resource": "ui://peacock/document?skill=email&id=email-demo-1" },
                ],
            }]}
        });
        let surface = extract_surface(&raw).expect("valid surface");
        for version in [BuilderVersion::V08, BuilderVersion::V09] {
            let envelope = build_envelope(&surface, version);
            let stream = envelope["stream"].as_array().expect("stream");
            assert_eq!(stream.len(), 1, "{version:?}: {envelope}");
            let node = &stream[0];
            // The wire node itself must NOT carry a `resource` key…
            let obj = match version {
                BuilderVersion::V09 => node.clone(),
                BuilderVersion::V08 => node["Component"]["Sources"].clone(),
            };
            assert!(
                obj.get("resource").is_none(),
                "{version:?} leaks a top-level resource: {node}"
            );
            // …while both items ride under `items` with label + resource.
            let items = obj["items"].as_array().expect("items");
            assert_eq!(items.len(), 2);
            assert_eq!(items[0]["label"], "account · initech-corp");
            assert_eq!(
                items[1]["resource"],
                "ui://peacock/document?skill=email&id=email-demo-1"
            );
        }
    }
}
