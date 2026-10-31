//! Gemini-Enterprise-compatible **A2UI v0.9** builder (a2ui.org, the
//! `application/json+a2ui` wire form that Vertex AI Search / Agentspace
//! actually renders).
//!
//! This is deliberately SEPARATE from the historical `v08`/`v09` builders in
//! this module: those emit Triton's own `{version, stream:[…]}` dialect for
//! the Explorer's Flutter renderer, which no shipping surface consumes. Gemini
//! Enterprise renders the a2ui.org catalog format — a streamed message array
//! (`createSurface` → `updateComponents`) of flat, id-referenced components
//! drawn from GE's **composite (Material) catalog** — which is what this file
//! produces.
//!
//! Contract (github.com/a2ui-project/a2ui `specification/v0_9`, Google's GE
//! A2UI component-gallery reference, and the a2ui-project GE sample):
//!   - `data` is an ARRAY of messages, each stamped `"version":"v0.9"`.
//!   - components use the flat discriminator `{"id","component":"…", …}`.
//!   - we render with GE's **composite catalog** (Material Design components),
//!     so the card is `MaterialCard`/`MaterialColumn`/`MaterialRow` and the
//!     widgets are `MaterialButton`/`MaterialImage` — real Material, not plain
//!     primitives. A `MaterialCard` takes a `children` LIST (not a single
//!     `child`).
//!   - a `MaterialButton` carries its `label` DIRECTLY (no child Text) plus a
//!     `variant`/`color`; its `action` is REQUIRED — we emit
//!     `{"event":{"name","context"}}` so a click posts an A2UI `action` back.
//!   - charts render as a native `VegaChart` (Vega-Lite `spec`, interactive)
//!     when the A2A layer injects one from peacock; else a static `Image` URL.
//!
//! Input is the RAW surface JSON the agent produced (`{components:[…]}` with
//! each component tagged by `kind`), not the typed [`super::Surface`], because
//! the embedded agent stamps fields the typed model does not carry (notably a
//! signed `image_url` on a `report`), and the spec-A2A path already treats the
//! surface as raw JSON.

use serde_json::{Value, json};

use super::{Action, Component, FormField, FormFieldKind, FormGroup, StepState, Tone, TreeNode};

/// The A2UI v0.9 A2A-extension URI (agent-card advertisement + activation).
pub const EXTENSION_URI: &str = "https://a2ui.org/a2a-extension/a2ui/v0.9";
/// The DataPart MIME type Gemini Enterprise's renderer actually reads.
///
/// The a2ui.org SDK names the two spellings BACKWARDS: the constant it calls
/// `A2UI_MIME_TYPE` (`application/a2ui+json`) is the unshipped *future*
/// spelling that no client reads yet, while `DEPRECATED_A2UI_MIME_TYPE`
/// (`application/json+a2ui`) is what every shipping renderer — Gemini
/// Enterprise included — matches. Emitting the "+a2ui" form makes GE treat the
/// DataPart as opaque and fall back to rendering the TextPart only (a
/// perfectly-formed card shows as text). Verified against the a2ui SDK
/// (`create_a2ui_part`) and Google's own GE integration notes.
pub const MIME: &str = "application/json+a2ui";
/// The catalog id declared in the card and in `createSurface`.
///
/// Gemini Enterprise's own **composite catalog** (Material Design components +
/// the basic primitives + GE-native ones), NOT the design-agnostic a2ui.org
/// `basic` catalog. Using it lets the card render as real Material — themed
/// `MaterialButton`/`MaterialText`/`MaterialCard` instead of plain primitives.
/// (Authoritative id per Google's GE A2UI reference + the a2ui-project GE
/// sample catalog's own `$id`.)
pub const BASIC_CATALOG: &str =
    "https://www.gstatic.com/vertexaisearch/a2ui/v0_9/gemini_enterprise_composite_catalog.json";
/// Surface theme `primaryColor` (`^#[0-9a-fA-F]{6}$`) sent in `createSurface`.
/// Gemini brand blue — the same `--pk-brand` the peacock `gemini.css` host theme
/// gives the charts — so the Material card's primary accents match the chart.
const THEME_PRIMARY_COLOR: &str = "#1a73e8";

/// The in-band form [`question_for`] hands the A2A layer for an open-doc
/// click, which detects it via [`open_doc`] and replies with the document
/// surface instead of running the agent.
///
/// F7: the click itself is a TYPED, signed action ([`GeAction::OpenDoc`]);
/// this string exists only so the existing `question_for` → `open_doc`
/// caller keeps working. Its prefix carries a random per-process nonce, so
/// caller-supplied text that imitates it opens nothing — only a decoded
/// signed id can produce it. New callers should use [`action_for`].
fn doc_sentinel() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| format!("\u{1}a2ui-open-doc:{}\u{1}", uuid::Uuid::new_v4()))
}

/// What a clicked GE component id resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeAction {
    /// Send this complete message as a new turn.
    Message(String),
    /// A form submit whose click did not carry every required input (the
    /// named fields). Nothing is sent; the caller says which choice is
    /// missing.
    Incomplete(Vec<String>),
    /// Open a cited document (no agent turn).
    OpenDoc {
        label: String,
        skill: String,
        id: String,
        url: String,
    },
}

/// Feature flag (`TRITON_GE_DOC_CANVAS`): emit in-card "Open source" buttons
/// that open the cited document inline in GE. OFF by default; when off,
/// `sources` stay a no-op here and ride the prose bubble as Markdown links
/// (`reply_text`), exactly as before. The RENDERING a click produces is the
/// **native Material** document view ([`build_document_material`], GA
/// composite-catalog widgets, no allowlist) unless [`doc_iframe_enabled`].
pub fn doc_canvas_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TRITON_GE_DOC_CANVAS")
            .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(false)
    })
}

/// Feature flag (`TRITON_GE_DOC_IFRAME`): opt into the richer **Canvas side
/// panel + `IFrameUrl`** rendering (the exact Material `/docs` page) instead of
/// the native-Material default. OFF by default because `IFrameUrl`/`Canvas` are
/// composite-catalog components Google has not documented, and the `IFrameUrl`
/// host must be on GE's widget allowlist (a Google-side request) or the panel
/// shows a security error. Turn on ONLY once that allowlist is confirmed.
pub fn doc_iframe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TRITON_GE_DOC_IFRAME")
            .or_else(|_| std::env::var("TRITON_GE_DOC_CANVAS"))
            .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(false)
    })
}

/// A source's document reference, recovered from the open-doc sentinel a card
/// button round-tripped through [`question_for`]. `skill`/`id` drive the
/// native Material render (a `render_report(document,…)` dispatch); `url` is
/// the signed `/docs` page for the iframe variant. All borrow from the input.
pub struct OpenDocRef<'a> {
    pub label: &'a str,
    pub skill: &'a str,
    pub id: &'a str,
    pub url: &'a str,
}

/// Parse the open-doc sentinel (`label \u1 skill \u1 id \u1 url`). `None` for
/// an ordinary re-ask or text turn, or when neither a `(skill,id)` pair nor a
/// `url` is recoverable (nothing to open).
pub fn open_doc(text: &str) -> Option<OpenDocRef<'_>> {
    let rest = text.strip_prefix(doc_sentinel())?;
    let mut parts = rest.splitn(4, '\u{1}');
    let label = parts.next().unwrap_or("");
    let skill = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("");
    let url = parts.next().unwrap_or("").trim();
    let has_ref = (!skill.is_empty() && !id.is_empty()) || !url.is_empty();
    has_ref.then_some(OpenDocRef {
        label,
        skill,
        id,
        url,
    })
}

/// Split a source's `"skill \u{b7} id"` label (report.rs) back into its parts.
fn split_skill_id(label: &str) -> (&str, &str) {
    match label.split_once(" \u{b7} ") {
        Some((s, i)) => (s.trim(), i.trim()),
        None => ("", ""),
    }
}

/// Fixed surfaceId prefix; a v4 UUID follows. The id is kept SHORT
/// (`triton-answer-<uuid>`, 50 chars) on purpose: Gemini Enterprise TRUNCATES
/// the surfaceId to the uuid on a button click (observed on the wire — a longer
/// encoded id came back stripped), so it cannot carry a payload.
const SURFACE_PREFIX: &str = "triton-answer-";

/// Upper bound on one signed action token carried in a component id. A
/// complete follow-up request is a sentence, not a document; the cap keeps a
/// runaway message from bloating every card and bounds the decode work an
/// inbound click can force.
pub const GE_ACTION_CAP: usize = 2048;
/// How long a card's actions stay valid. Long enough for a card that sits in
/// a conversation over a holiday; short enough that a leaked id is not a
/// permanent capability. (The message it sends runs under the CLICKER's own
/// principal either way — the signature only stops crafted payloads.)
const GE_ACTION_TTL_SECS: u64 = 30 * 24 * 3600;
/// An open-doc action carries a signed `/docs` URL in a readable id, so it
/// lives a day, not a month (F7): a leaked card id stops opening the
/// document long before a send-message one stops sending its text.
const GE_DOC_TTL_SECS: u64 = 24 * 3600;
/// Sentinel tool for a signed open-doc action.
const OPEN_DOC_TOOL: &str = "__open_doc";
/// Binding platform for GE action tokens, so a GE card id can never be
/// replayed as a Teams/Telegram token minted under the same key.
const GE_PLATFORM: &str = "a2ui-ge";

/// Signs and verifies the **stateless** GE card actions.
///
/// GE strips an action's `name`/`context` on click and truncates the
/// `surfaceId`, but echoes the clicked component's own `id` verbatim as
/// `sourceComponentId` (verified live, #275). So the id IS the payload: a
/// clickable component's id is `a<N>-<token>`, where `<token>` is an
/// HMAC-signed `(SEND_MESSAGE_TOOL, {m: <complete message>})` correlation
/// token and `<N>` keeps ids unique when two controls send the same text.
/// Any replica that holds the key recovers the complete message from the id
/// alone — no card store, no affinity. This replaces the process-local
/// `SURFACE_QUESTIONS` table.
///
/// Key: installed at boot via [`configure_action_key`] from
/// `TRITON_A2UI_ACTION_KEY` (≥ 16 bytes; a comma-separated list rotates,
/// first signs). Without it, a random per-process key is used — which
/// degrades to exactly the old single-process behaviour (a click resolves
/// only on the replica that rendered the card).
pub struct ActionSigner {
    ring: triton_correlation::KeyRing,
}

impl ActionSigner {
    /// A signer over an explicit key ring (tests, embedded hosts).
    pub fn new(ring: triton_correlation::KeyRing) -> Self {
        Self { ring }
    }

    /// The process-wide signer: the key installed by
    /// [`configure_action_key`] at boot, else a random per-process key (a
    /// click then resolves only on the replica that rendered the card —
    /// the binary warns about that at startup). Core never reads the
    /// environment itself; the process edge injects the key (F4).
    pub fn global() -> &'static ActionSigner {
        if let Some(s) = INSTALLED.get() {
            return s;
        }
        static EPHEMERAL: std::sync::OnceLock<ActionSigner> = std::sync::OnceLock::new();
        EPHEMERAL.get_or_init(|| {
            let random = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
            ActionSigner {
                ring: triton_correlation::KeyRing::single(random.into_bytes())
                    .expect("72 random bytes clear the minimum"),
            }
        })
    }

    fn binding() -> triton_correlation::Binding<'static> {
        triton_correlation::Binding {
            platform: GE_PLATFORM,
            tenant: "",
            sender: "",
        }
    }

    /// Mint the token for "send `message` as a new turn".
    pub fn sign(&self, message: &str) -> Option<String> {
        triton_correlation::encode_bound(
            triton_correlation::message::SEND_MESSAGE_TOOL,
            &triton_correlation::message::text_args(message),
            self.ring.signing(),
            GE_ACTION_CAP,
            Self::binding(),
            Some(GE_ACTION_TTL_SECS),
        )
        .ok()
    }

    /// Mint the token for "fill this template from the form's inputs and
    /// send it as a new turn" (a form submit with real input fields).
    pub fn sign_template(&self, template: &str, labels: &[(&str, &str)]) -> Option<String> {
        // The field labels ride along so an incomplete submit can say which
        // choice is missing in the reader's words, never by field key.
        let mut args = triton_correlation::message::template_args(template);
        args["labels"] = Value::Object(
            labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), json!(v)))
                .collect(),
        );
        triton_correlation::encode_bound(
            triton_correlation::message::SEND_MESSAGE_TOOL,
            &args,
            self.ring.signing(),
            GE_ACTION_CAP,
            Self::binding(),
            Some(GE_ACTION_TTL_SECS),
        )
        .ok()
    }

    /// Mint the component id (`a<n>-<token>`) for "open this cited
    /// document" — signed like a send-message action, typed on decode.
    pub fn sign_open_doc(
        &self,
        n: usize,
        label: &str,
        skill: &str,
        id: &str,
        url: &str,
    ) -> Option<String> {
        let token = triton_correlation::encode_bound(
            OPEN_DOC_TOOL,
            &json!({ "l": label, "s": skill, "i": id, "u": url }),
            self.ring.signing(),
            GE_ACTION_CAP,
            Self::binding(),
            Some(GE_DOC_TTL_SECS),
        )
        .ok()?;
        Some(format!("a{n}-{token}"))
    }

    /// Recover the complete message from a clicked component id
    /// (`a<N>-<token>`). `None` for a forged, expired or foreign id, and for
    /// an open-doc id (use [`ActionSigner::open_action`]).
    pub fn open(&self, component_id: &str) -> Option<String> {
        match self.open_action(component_id)? {
            GeAction::Message(m) => Some(m),
            GeAction::OpenDoc { .. } | GeAction::Incomplete(_) => None,
        }
    }

    /// Decode a clicked component id into its typed action.
    pub fn open_action(&self, component_id: &str) -> Option<GeAction> {
        self.open_action_with(component_id, &Value::Null)
    }

    /// Decode a clicked component id; a form submit's template is filled
    /// from `context` (the click's resolved `action.context`, one string per
    /// field name). Unsigned inputs only ever complete a signed template.
    pub fn open_action_with(&self, component_id: &str, context: &Value) -> Option<GeAction> {
        let rest = component_id.strip_prefix('a')?;
        let (n, token) = rest.split_once('-')?;
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let (tool, args) =
            triton_correlation::decode_bound_any(token, &self.ring, GE_ACTION_CAP, Self::binding())
                .ok()?;
        if tool == OPEN_DOC_TOOL {
            let f = |k: &str| {
                args.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            return Some(GeAction::OpenDoc {
                label: f("l"),
                skill: f("s"),
                id: f("i"),
                url: f("u"),
            });
        }
        if tool != triton_correlation::message::SEND_MESSAGE_TOOL {
            return None;
        }
        let input = |k: &str| {
            context.get(k).and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                // A select bound to a list (ChoicePicker style) or a number.
                Value::Array(a) => a.first().and_then(Value::as_str).map(str::to_owned),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
        };
        match triton_correlation::message::resolve(&args, input) {
            Ok(m) => Some(GeAction::Message(m)),
            Err(triton_correlation::message::MessageError::Missing(names)) => {
                let label = |n: &String| {
                    args.get("labels")
                        .and_then(|l| l.get(n))
                        .and_then(Value::as_str)
                        .map_or_else(|| n.clone(), str::to_owned)
                };
                Some(GeAction::Incomplete(names.iter().map(label).collect()))
            }
            Err(_) => None,
        }
    }
}

static INSTALLED: std::sync::OnceLock<ActionSigner> = std::sync::OnceLock::new();

/// What [`configure_action_key`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKeyState {
    /// A shared key is installed: every replica holding it resolves every
    /// card id.
    Configured,
    /// No key: a random per-process key signs, so clicks resolve on this
    /// replica only. Fine for one replica; the caller warns.
    Ephemeral,
}

/// Install the A2UI action key from its raw operator value
/// (`TRITON_A2UI_ACTION_KEY`: one key, or `new,old` during a rotation; each
/// ≥ 16 bytes). Call once at boot, before any card renders. `Err` is a
/// malformed value — the caller must refuse to start rather than fall back
/// to a key no other replica shares. A second call keeps the first key.
pub fn configure_action_key(
    raw: Option<&str>,
) -> Result<ActionKeyState, triton_correlation::KeyRingError> {
    let Some(raw) = raw else {
        return Ok(ActionKeyState::Ephemeral);
    };
    let ring = triton_correlation::KeyRing::parse(raw)?;
    let _ = INSTALLED.set(ActionSigner { ring });
    Ok(ActionKeyState::Configured)
}

/// Resolve a GE button click (the echoed `sourceComponentId`) to its typed
/// action. `None` ⇒ not one of our actions (forged, expired, foreign).
pub fn action_for(source_component_id: &str) -> Option<GeAction> {
    ActionSigner::global().open_action(source_component_id)
}

/// [`action_for`] with the click's resolved `action.context`, which carries
/// a form's input values.
pub fn action_for_with(source_component_id: &str, context: &Value) -> Option<GeAction> {
    ActionSigner::global().open_action_with(source_component_id, context)
}

/// String form of [`action_for`] for the existing A2A caller: a message
/// verbatim, an open-doc as the nonce-protected in-band form [`open_doc`]
/// parses. The surface id is not used (the clicked id is self-contained);
/// the parameter stays only so that caller's signature is unchanged.
pub fn question_for(_surface_id: &str, source_component_id: &str) -> Option<String> {
    Some(match action_for(source_component_id)? {
        GeAction::Message(m) => m,
        GeAction::Incomplete(_) => return None,
        GeAction::OpenDoc {
            label,
            skill,
            id,
            url,
        } => format!("{}{label}\u{1}{skill}\u{1}{id}\u{1}{url}", doc_sentinel()),
    })
}

/// Rendering switches for [`build_messages_with`]. `open_url` and `embed`
/// default OFF because each depends on something only a live Gemini
/// Enterprise tenant can confirm; `form_fields` defaults ON with a switch
/// back to one-click buttons; see the field docs.
#[derive(Debug, Clone, Copy)]
pub struct GeOptions {
    /// Emit `open_url` actions as in-card buttons (`functionCall: openUrl`,
    /// the composite catalog's own function). OFF: an earlier, malformed
    /// attempt (`{name: openUrl}` under the basic catalog, #263) was rejected
    /// live; the spec-correct `{call, args, returnType: void}` form is
    /// schema-valid but unqualified. When off, deep links ride the prose
    /// bubble (the producer's text) and rows show their label only.
    /// Env: `TRITON_GE_OPEN_URL`.
    pub open_url: bool,
    /// Render an `embed` as a Canvas side panel with an `IFrameUrl`. OFF:
    /// the framed host must be on GE's widget allowlist (a Google-side
    /// request), else the panel shows a security error. When off, the
    /// embed's `fallback` renders inline. Env: `TRITON_GE_EMBED`.
    pub embed: bool,
    /// Render a template form as real input fields (`MaterialSelect`,
    /// `MaterialInput`) bound to the surface data model, whose submit sends
    /// the bound values in the click's `action.context`. ON by default. OFF
    /// (`TRITON_GE_FORM_FIELDS=off`) restores the one-click buttons: a form
    /// whose template needs one select becomes one button per option, which
    /// works even where a host drops the click context.
    pub form_fields: bool,
}

impl Default for GeOptions {
    fn default() -> Self {
        GeOptions {
            open_url: false,
            embed: false,
            form_fields: true,
        }
    }
}

impl GeOptions {
    /// The process-wide options from the environment.
    pub fn from_env() -> GeOptions {
        static OPTS: std::sync::OnceLock<GeOptions> = std::sync::OnceLock::new();
        *OPTS.get_or_init(|| {
            let on = |k: &str| {
                std::env::var(k)
                    .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
                    .unwrap_or(false)
            };
            let off = |k: &str| {
                std::env::var(k)
                    .map(|v| matches!(v.trim(), "0" | "false" | "FALSE" | "no" | "off"))
                    .unwrap_or(false)
            };
            GeOptions {
                open_url: on("TRITON_GE_OPEN_URL"),
                embed: on("TRITON_GE_EMBED"),
                form_fields: !off("TRITON_GE_FORM_FIELDS"),
            }
        })
    }
}

/// GE-specific presentation tweaks to peacock's Vega-Lite spec (on a clone —
/// peacock's shared spec is untouched). GE's `VegaChart` renders the raw spec
/// as-is, which looks cluttered next to the Teams card: peacock rotates the
/// x-axis labels vertical, and a single-series bar colored by its own x-field
/// carries a redundant color legend. Fix both, and let the chart fill GE's
/// iframe width.
fn beautify_vega_for_ge(spec: &Value) -> Value {
    let mut s = spec.clone();
    if let Some(obj) = s.as_object_mut() {
        // Responsive: fill the iframe rather than a fixed peacock width.
        obj.insert("width".to_string(), json!("container"));
        if let Some(enc) = obj.get_mut("encoding").and_then(Value::as_object_mut) {
            let x_field = enc
                .get("x")
                .and_then(|x| x.get("field"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // Horizontal x-axis labels (peacock rotates them vertical, which is
            // unreadable in GE's renderer).
            if let Some(x) = enc.get_mut("x").and_then(Value::as_object_mut) {
                match x.get_mut("axis").and_then(Value::as_object_mut) {
                    Some(a) => {
                        a.insert("labelAngle".to_string(), json!(0));
                    }
                    None => {
                        x.insert("axis".to_string(), json!({ "labelAngle": 0 }));
                    }
                }
            }
            // Drop the redundant color legend when `color` encodes the SAME
            // field as `x` (e.g. supplier-reliability colors each bar by the
            // supplier the x-axis already labels). A genuine series (a stacked
            // bar colored by a different field) keeps its legend.
            if let (Some(xf), Some(color)) =
                (x_field, enc.get_mut("color").and_then(Value::as_object_mut))
                && color.get("field").and_then(Value::as_str) == Some(xf.as_str())
            {
                color.insert("legend".to_string(), Value::Null);
            }
        }
    }
    s
}

/// Build the A2UI v0.9 message array from a raw surface value with the
/// process-wide signer and env options: the producer-built surfaces of
/// `surface.a2ui` first (verbatim, see [`passthrough_surfaces`]), then
/// Triton's own card from `surface.components`. Returns `None` when neither
/// yields a surface, so the caller can fall back to a text part.
pub fn build_messages(result: &Value) -> Option<Vec<Value>> {
    build_messages_with(result, ActionSigner::global(), GeOptions::from_env())
}

/// [`build_messages`] for a request that did or did not activate the A2UI
/// extension: producer-built surfaces (`surface.a2ui`) only go to a client
/// that negotiated A2UI.
pub fn build_messages_negotiated(result: &Value, a2ui: bool) -> Option<Vec<Value>> {
    if a2ui || result.get("surface").and_then(|s| s.get("a2ui")).is_none() {
        return build_messages(result);
    }
    let mut without = result.clone();
    if let Some(surface) = without.get_mut("surface").and_then(Value::as_object_mut) {
        surface.remove("a2ui");
    }
    build_messages(&without)
}

/// The flat, id-referenced component list of one card under construction.
struct Card<'a> {
    flat: Vec<Value>,
    n: usize,
    /// Field-form counter: each form's data-model namespace (`/f<k>`).
    forms: usize,
    signer: &'a ActionSigner,
    opts: GeOptions,
}

impl Card<'_> {
    fn next_id(&mut self, prefix: &str) -> String {
        self.n += 1;
        format!("{prefix}-{}", self.n)
    }

    /// Push `comp` under a fresh id and return the id.
    fn add(&mut self, prefix: &str, mut comp: Value) -> String {
        let id = self.next_id(prefix);
        comp["id"] = json!(id);
        self.flat.push(comp);
        id
    }

    fn text(&mut self, text: &str, hint: &str) -> String {
        self.add(
            "t",
            json!({ "component": "MaterialText", "text": text, "usageHint": hint }),
        )
    }

    fn column(&mut self, children: Vec<String>) -> String {
        self.add(
            "col",
            json!({ "component": "MaterialColumn", "children": children }),
        )
    }

    fn row(&mut self, children: Vec<String>) -> String {
        self.add(
            "row",
            json!({
                "component": "MaterialRow", "children": children,
                "justify": "start", "align": "stretch",
            }),
        )
    }

    /// A button that sends `message` as a new turn: its id carries the
    /// signed payload (see [`ActionSigner`]). `None` when the message is too
    /// long to sign — the control is dropped rather than shipped dead.
    fn send_button(&mut self, label: &str, message: &str, variant: &str) -> Option<String> {
        let Some(token) = self.signer.sign(message) else {
            tracing::warn!(
                bytes = message.len(),
                cap = GE_ACTION_CAP,
                "a2ui/ge: dropping a send-message control (request too long to sign)"
            );
            return None;
        };
        self.n += 1;
        let id = format!("a{}-{token}", self.n);
        self.flat.push(json!({
            "id": id,
            "component": "MaterialButton",
            "label": label,
            "variant": variant,
            "color": "primary",
            "action": { "event": { "name": "send_message", "context": {} } },
        }));
        Some(id)
    }

    /// An "open this cited document" button (a typed, signed action).
    fn doc_button(&mut self, label: &str, doc: (&str, &str, &str, &str)) -> Option<String> {
        self.n += 1;
        let Some(id) = self
            .signer
            .sign_open_doc(self.n, doc.0, doc.1, doc.2, doc.3)
        else {
            tracing::warn!("a2ui/ge: dropping an open-doc control (reference too long to sign)");
            return None;
        };
        self.flat.push(json!({
            "id": id,
            "component": "MaterialButton",
            "label": label,
            "variant": "outlined",
            "color": "primary",
            "action": { "event": { "name": "open_doc", "context": {} } },
        }));
        Some(id)
    }

    /// A button that opens `url` client-side — only when enabled.
    fn url_button(&mut self, label: &str, url: &str) -> Option<String> {
        if !self.opts.open_url || !crate::links::is_allowed_link(url) {
            return None;
        }
        Some(self.add(
            "lnk",
            json!({
                "component": "MaterialButton",
                "label": label,
                "variant": "basic",
                "color": "primary",
                "action": { "functionCall": {
                    "call": "openUrl", "args": { "url": url }, "returnType": "void",
                } },
            }),
        ))
    }

    fn action_button(&mut self, label: &str, a: &Action, variant: &str) -> Option<String> {
        match a {
            Action::SendMessage { text } => self.send_button(label, text, variant),
            Action::OpenUrl { url } => self.url_button(label, url),
        }
    }

    /// Buttons laid out two per `MaterialRow` (GE's row neither wraps nor
    /// gaps, so four in a row overprint).
    fn button_rows(&mut self, ids: Vec<String>) -> Vec<String> {
        let chunks: Vec<Vec<String>> = ids.chunks(2).map(<[String]>::to_vec).collect();
        chunks.into_iter().map(|c| self.row(c)).collect()
    }

    /// One conversation component → the ids it adds under the root column.
    fn component(&mut self, c: &Component) -> Vec<String> {
        match c {
            Component::List { title, items } => {
                let mut ids = Vec::new();
                if let Some(t) = title {
                    ids.push(self.text(t, "subtitle1"));
                }
                for it in items {
                    let mut cell = Vec::new();
                    // With an `action_label` the control is a button at the
                    // end of the row ("Agent 2 · Requirements: Forecast ·
                    // [Retire]"); without one the label is the control.
                    let (head, tail) = match (&it.action, &it.action_label) {
                        (Some(a), Some(caption)) => {
                            (None, self.action_button(caption, a, "stroked"))
                        }
                        (Some(a), None) => (self.action_button(&it.label, a, "basic"), None),
                        (None, _) => (None, None),
                    };
                    cell.push(head.unwrap_or_else(|| self.text(&it.label, "body1")));
                    if let Some(d) = it.detail.as_deref().filter(|d| !d.is_empty()) {
                        cell.push(self.text(d, "caption"));
                    }
                    let col = self.column(cell);
                    match tail {
                        Some(btn) => ids.push(self.row(vec![col, btn])),
                        None => ids.push(col),
                    }
                }
                ids
            }
            Component::Table {
                title,
                columns,
                rows,
            } => {
                let mut ids = Vec::new();
                let cols: Vec<Value> = columns
                    .iter()
                    .map(|c| json!({ "header": c.label, "field": c.key }))
                    .collect();
                let data: Vec<Value> = rows
                    .iter()
                    .map(|r| {
                        let mut o = serde_json::Map::new();
                        for c in columns {
                            o.insert(
                                c.key.clone(),
                                json!(r.cells.get(&c.key).cloned().unwrap_or_default()),
                            );
                        }
                        Value::Object(o)
                    })
                    .collect();
                let mut table =
                    json!({ "component": "MaterialTable", "columns": cols, "rows": data });
                if let Some(t) = title {
                    table["caption"] = json!(t);
                }
                // A MaterialTable cannot carry a row action. A button per
                // row would show every row twice (the rows once in the
                // table, once as buttons), so row actions stay with the
                // hosts that can make a row tappable.
                ids.push(self.add("tbl", table));
                ids
            }
            Component::Chips { prompt, chips } => {
                let mut ids = Vec::new();
                if let Some(p) = prompt {
                    ids.push(self.text(p, "body2"));
                }
                let btns: Vec<String> = chips
                    .iter()
                    .filter_map(|ch| self.send_button(&ch.label, &ch.message, "stroked"))
                    .collect();
                ids.extend(self.button_rows(btns));
                ids
            }
            Component::Tree { title, nodes, .. } => {
                // The "only gaps" toggle has no GE equivalent (the composite
                // catalog has no client-side visibility binding), so it is
                // not rendered; gap nodes are marked and start expanded.
                let mut ids = Vec::new();
                if let Some(t) = title {
                    ids.push(self.text(t, "subtitle1"));
                }
                for n in nodes {
                    ids.push(self.tree_node(n));
                }
                ids
            }
            Component::Notice { tone, title, text } => {
                let icon = match tone {
                    Tone::Info => ("info", "primary"),
                    Tone::Warn => ("warning", "warn"),
                    Tone::Attention => ("error", "warn"),
                };
                let icon_id = self.add(
                    "ic",
                    json!({ "component": "MaterialIcon", "icon": icon.0, "color": icon.1 }),
                );
                let body = match title {
                    Some(t) if !t.is_empty() => format!("{t}: {text}"),
                    _ => text.clone(),
                };
                let text_id = self.text(&body, "body2");
                vec![self.row(vec![icon_id, text_id])]
            }
            Component::Progress {
                title,
                steps,
                fraction,
            } => {
                let mut ids = Vec::new();
                if let Some(t) = title {
                    ids.push(self.text(t, "subtitle2"));
                }
                let bar = match fraction {
                    Some(f) => json!({
                        "component": "MaterialProgressBar", "mode": "determinate",
                        "value": (f.clamp(0.0, 1.0) * 100.0).round(),
                    }),
                    None => json!({ "component": "MaterialProgressBar", "mode": "indeterminate" }),
                };
                ids.push(self.add("pb", bar));
                for s in steps {
                    let icon = match s.state {
                        StepState::Done => "check_circle",
                        StepState::Active => "pending",
                        StepState::Pending => "radio_button_unchecked",
                    };
                    let ic = self.add(
                        "ic",
                        json!({ "component": "MaterialIcon", "icon": icon, "color": "primary" }),
                    );
                    let hint = if s.state == StepState::Active {
                        "body1"
                    } else {
                        "body2"
                    };
                    let tx = self.text(&s.label, hint);
                    ids.push(self.row(vec![ic, tx]));
                }
                ids
            }
            Component::KeyValue { title, pairs } => {
                let rows: Vec<Value> = pairs
                    .iter()
                    .map(|p| {
                        let v = match &p.before {
                            Some(b) => format!("{b} \u{2192} {}", p.value),
                            None => p.value.clone(),
                        };
                        json!({ "label": p.label, "value": v })
                    })
                    .collect();
                let mut t = json!({
                    "component": "MaterialTable",
                    "columns": [ { "header": "", "field": "label" }, { "header": "", "field": "value" } ],
                    "rows": rows,
                });
                if let Some(title) = title {
                    t["caption"] = json!(title);
                }
                vec![self.add("kv", t)]
            }
            Component::Selection {
                prompt,
                options,
                args_key,
                submit: Some(sub),
                ..
            } => {
                // GE posts `context: {}` on click, so a radio value chosen
                // client-side never reaches us. Each option therefore
                // becomes its own button carrying the template filled with
                // THAT option — the same complete request, one click.
                let mut ids = vec![self.text(prompt, "body2")];
                let btns: Vec<String> = options
                    .iter()
                    .filter_map(|o| {
                        let msg = triton_correlation::message::fill_template(&sub.template, |k| {
                            (k == args_key).then(|| o.value.clone())
                        })
                        .ok()?;
                        self.send_button(&o.label, &msg, "stroked")
                    })
                    .collect();
                ids.extend(self.button_rows(btns));
                ids
            }
            Component::Form {
                title,
                fields,
                submit_label,
                send_template: Some(tpl),
                preview_label,
                ..
            } => {
                if self.opts.form_fields {
                    let mut ids = Vec::new();
                    if !title.is_empty() {
                        ids.push(self.text(title, "subtitle2"));
                    }
                    ids.extend(self.field_form(fields, tpl, submit_label));
                    ids
                } else {
                    self.template_form(title, fields, tpl, preview_label.as_deref())
                }
            }
            // The text part says the title; the card keeps the context line.
            Component::Header { context, stamp, .. } => {
                // The text part says the title; the card keeps the context
                // line and the stamp (a read's time), on one caption.
                let line: Vec<&str> = [context.as_deref(), stamp.as_deref()]
                    .into_iter()
                    .flatten()
                    .filter(|t| !t.is_empty())
                    .collect();
                if line.is_empty() {
                    Vec::new()
                } else {
                    vec![self.text(&line.join(" \u{b7} "), "caption")]
                }
            }
            Component::FormGroups {
                title,
                groups,
                submit_label,
                note,
            } => self.form_groups(title.as_deref(), groups, submit_label, note.as_deref()),
            Component::Button {
                label,
                action: Some(a),
                ..
            } => self.action_button(label, a, "flat").into_iter().collect(),
            Component::Embed { fallback, .. } => self.component(fallback),
            // Prose reaches the card only as an embed's fallback (the bubble
            // carries the surface's own text/narration).
            Component::Text { value, .. } => vec![self.text(value, "body1")],
            Component::Narration { text } => vec![self.text(text, "caption")],
            _ => Vec::new(),
        }
    }

    /// A form with real inputs: each field is bound to `/f<k>/<name>` in the
    /// surface data model, and the submit's `action.context` maps every
    /// field name to its binding, so the click carries the chosen values.
    /// The submit's id is the signed template ([`ActionSigner::sign_template`]);
    /// the A2A layer fills it from the context ([`action_for_with`]).
    fn field_form(&mut self, fields: &[FormField], tpl: &str, submit_label: &str) -> Vec<String> {
        self.forms += 1;
        let ns = format!("/f{}", self.forms);
        let mut ids = Vec::new();
        let mut context = serde_json::Map::new();
        for f in fields {
            let path = format!("{ns}/{}", f.name);
            context.insert(f.name.clone(), json!({ "path": path }));
            let comp = match f.kind {
                FormFieldKind::Select => json!({
                    "component": "MaterialSelect",
                    "label": f.label,
                    "options": f.options.iter()
                        .map(|o| json!({ "label": o.label, "value": o.value }))
                        .collect::<Vec<_>>(),
                    "value": { "path": path },
                }),
                FormFieldKind::Integer => json!({
                    "component": "MaterialInput", "label": f.label, "type": "number",
                    "value": { "path": path },
                }),
                _ => {
                    let mut c = json!({
                        "component": "MaterialInput", "label": f.label,
                        "value": { "path": path },
                    });
                    if let Some(p) = &f.placeholder {
                        c["placeholder"] = json!(p);
                    }
                    c
                }
            };
            ids.push(self.add("in", comp));
        }
        let labels: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.label.as_str()))
            .collect();
        let Some(token) = self.signer.sign_template(tpl, &labels) else {
            // No submit, no inputs: fields that cannot be sent would mislead.
            tracing::warn!("a2ui/ge: form too long to sign; showing its one-click form instead");
            let mark = self.flat.len();
            self.flat.truncate(mark - ids.len());
            return self.template_form("", fields, tpl, None);
        };
        self.n += 1;
        let id = format!("a{}-{token}", self.n);
        let label = if submit_label.is_empty() {
            "Send"
        } else {
            submit_label
        };
        self.flat.push(json!({
            "id": id,
            "component": "MaterialButton",
            "label": label,
            "variant": "flat",
            "color": "primary",
            "action": { "event": { "name": "submit", "context": Value::Object(context) } },
        }));
        ids.push(id);
        ids
    }

    /// Form groups: one expandable panel per group (the first open), each a
    /// [`Card::field_form`] — or, with form fields off, the one-click buttons
    /// of [`Card::template_form`].
    fn form_groups(
        &mut self,
        title: Option<&str>,
        groups: &[FormGroup],
        submit_label: &str,
        note: Option<&str>,
    ) -> Vec<String> {
        let mut ids = Vec::new();
        if let Some(t) = title.filter(|t| !t.is_empty()) {
            ids.push(self.text(t, "subtitle2"));
        }
        for (k, g) in groups.iter().enumerate() {
            let mut inner = if self.opts.form_fields {
                self.field_form(&g.fields, &g.send_template, submit_label)
            } else {
                self.template_form("", &g.fields, &g.send_template, None)
            };
            if let Some(n) = note.filter(|n| !n.is_empty()) {
                inner.push(self.text(n, "caption"));
            }
            ids.push(self.add(
                "grp",
                json!({
                    "component": "MaterialExpansionPanel",
                    "title": g.label,
                    "expanded": k == 0,
                    "children": inner,
                }),
            ));
        }
        ids
    }

    /// The one-click fallback of a template form (`TRITON_GE_FORM_FIELDS=off`,
    /// or a form too long to sign) — for a host that drops a click's context:
    /// a form whose template depends on exactly ONE select field expands to
    /// one button per option; any other form shows its "will be sent as"
    /// preview for the reader to copy — Gemini cannot submit it.
    fn template_form(
        &mut self,
        title: &str,
        fields: &[FormField],
        tpl: &str,
        preview_label: Option<&str>,
    ) -> Vec<String> {
        let mut ids = Vec::new();
        if !title.is_empty() {
            ids.push(self.text(title, "subtitle2"));
        }
        let names = triton_correlation::message::required_placeholders(tpl);
        let only_select = match names.as_slice() {
            [one] => fields
                .iter()
                .find(|f| &f.name == one && matches!(f.kind, FormFieldKind::Select)),
            _ => None,
        };
        if let Some(f) = only_select {
            let btns: Vec<String> = f
                .options
                .iter()
                .filter_map(|o| {
                    let msg = triton_correlation::message::fill_template(tpl, |k| {
                        (k == f.name).then(|| o.value.clone())
                    })
                    .ok()?;
                    self.send_button(&o.label, &msg, "stroked")
                })
                .collect();
            ids.extend(self.button_rows(btns));
            return ids;
        }
        let preview = triton_correlation::message::preview_template(tpl, |name| {
            fields
                .iter()
                .find(|f| f.name == name)
                .map(|f| f.label.clone())
        });
        // The preview is shown only when the producer asks for it: the
        // composed request is plumbing, not content.
        if let Some(label) = preview_label {
            ids.push(self.text(&format!("{label}: {preview}"), "body2"));
        }
        ids
    }

    /// A tree node: a `MaterialExpansionPanel` (client-side expand/collapse)
    /// when it has anything inside, else a single text line.
    fn tree_node(&mut self, n: &TreeNode) -> String {
        let counts: Vec<String> = n
            .counts
            .iter()
            .map(|c| format!("{} {}", c.value, c.label).trim().to_string())
            .collect();
        let description = counts.join(" \u{b7} ");
        // A leaf is one line: a link-like button when it opens something (a
        // panel holding a single button would be a click to reach a click),
        // else text. A gap reads as its count.
        if n.children.is_empty() && n.chips.is_empty() {
            let line = if description.is_empty() {
                n.label.clone()
            } else {
                format!("{} \u{2014} {description}", n.label)
            };
            if !n.gap
                && let Some(id) = n
                    .action
                    .as_ref()
                    .and_then(|a| self.action_button(&line, a, "basic"))
            {
                return id;
            }
            return self.text(&line, "body2");
        }
        let mut inner: Vec<String> = Vec::new();
        let chip_btns: Vec<String> = n
            .chips
            .iter()
            .filter_map(|ch| self.send_button(&ch.label, &ch.message, "stroked"))
            .collect();
        inner.extend(self.button_rows(chip_btns));
        if let Some(a) = &n.action {
            inner.extend(self.action_button(n.action_caption(), a, "basic"));
        }
        for c in &n.children {
            inner.push(self.tree_node(c));
        }
        if inner.is_empty() {
            let line = if description.is_empty() {
                n.label.clone()
            } else {
                format!("{} \u{2014} {description}", n.label)
            };
            return self.text(&line, "body2");
        }
        let mut panel = json!({
            "component": "MaterialExpansionPanel",
            "title": n.label,
            "expanded": n.expanded || n.gap,
            "children": inner,
        });
        if !description.is_empty() {
            panel["description"] = json!(description);
        }
        self.add("node", panel)
    }
}

/// The `createSurface` + `updateComponents` pair for one surface.
fn surface_pair(flat: Vec<Value>) -> Vec<Value> {
    let surface_id = format!("{SURFACE_PREFIX}{}", uuid::Uuid::new_v4());
    vec![
        json!({
            "version": "v0.9",
            "createSurface": {
                "surfaceId": surface_id,
                "catalogId": BASIC_CATALOG,
                // v0.9 styling: `createSurface.theme` (which replaced v0.8's
                // `styles`) carries theme params the Material renderer
                // applies — GE uses `primaryColor` for primary buttons and
                // active borders. Renderers ignore fields they don't use.
                "theme": {
                    "primaryColor": THEME_PRIMARY_COLOR,
                    "agentDisplayName": "DataZoo Agent",
                },
            },
        }),
        json!({
            "version": "v0.9",
            "updateComponents": { "surfaceId": surface_id, "components": flat },
        }),
    ]
}

/// [`build_messages`] with an explicit signer and options (tests, embedded
/// hosts with their own key).
pub fn build_messages_with(
    result: &Value,
    signer: &ActionSigner,
    opts: GeOptions,
) -> Option<Vec<Value>> {
    let mut messages = passthrough_surfaces(result.get("surface").and_then(|s| s.get("a2ui")));
    messages.extend(card_messages(result, signer, opts).unwrap_or_default());
    (!messages.is_empty()).then_some(messages)
}

/// Triton's own card from `surface.components`.
fn card_messages(result: &Value, signer: &ActionSigner, opts: GeOptions) -> Option<Vec<Value>> {
    let components = result
        .get("surface")
        .and_then(|s| s.get("components"))
        .and_then(Value::as_array)?;

    let mut card = Card {
        flat: Vec::new(),
        n: 0,
        forms: 0,
        signer,
        opts,
    };
    // `root_children` collects the ids under the top Column, in surface
    // order. Follow-up buttons and source buttons are collected separately
    // and laid out last, so secondary document buttons never crowd out or
    // misalign the primary action chips.
    let mut root_children: Vec<String> = Vec::new();
    let mut action_btn_ids: Vec<String> = Vec::new();
    let mut source_btn_ids: Vec<String> = Vec::new();
    // Embeds that become their own Canvas surface (GE requires Canvas to be
    // a surface ROOT).
    let mut canvases: Vec<(String, String, Option<u32>)> = Vec::new();

    for c in components {
        let kind = c.get("kind").and_then(Value::as_str).unwrap_or_default();
        match kind {
            // Prose (text/narration) is intentionally NOT put in the card:
            // the streamed answer already renders as the message bubble above
            // the card, and duplicating it inside would show the answer twice.
            "text" | "narration" => {}
            // Chart. Prefer a NATIVE interactive `VegaChart` when a Vega-Lite
            // `vega_spec` is present (the A2A layer expands the report via
            // `render_report` and injects peacock's spec). Otherwise fall back
            // to the static PNG as a basic `Image` (NOT `MaterialImage`: GE's
            // Material renderer left the signed URL as a broken/empty <img>).
            "report" => {
                if let Some(spec) = c.get("vega_spec").filter(|s| s.is_object()) {
                    let cid = card.add(
                        "chart",
                        json!({
                            "component": "VegaChart",
                            "spec": beautify_vega_for_ge(spec),
                            "height": 300,
                        }),
                    );
                    root_children.push(cid);
                } else if let Some(url) = c.get("image_url").and_then(Value::as_str) {
                    let mut img = json!({ "component": "Image", "url": url, "fit": "contain" });
                    if let Some(t) = c.get("title").and_then(Value::as_str) {
                        img["description"] = json!(t);
                    }
                    root_children.push(card.add("chart", img));
                }
            }
            "button" => {
                // A stateless-action button (send_message / open_url).
                if c.get("action").is_some_and(Value::is_object) {
                    if let Some(comp) = Component::parse(c) {
                        action_btn_ids.extend(card.component(&comp));
                    }
                    continue;
                }
                let label = c.get("label").and_then(Value::as_str).unwrap_or("Open");
                let tool = c.get("tool").and_then(Value::as_str).unwrap_or_default();
                // Legacy re-ask follow-up: the question rides in
                // `args.question` (agent-core `AssistantTool`) or
                // `args.message`. It is now signed into the button id like
                // any send-message action. `render_report` opens a report,
                // not a re-ask, and other tool buttons have nothing GE can
                // run: they render as before but resolve to no message.
                let question = c
                    .get("args")
                    .and_then(|a| a.get("question").or_else(|| a.get("message")))
                    .and_then(Value::as_str)
                    .filter(|_| tool != "render_report");
                let id = match question.and_then(|q| card.send_button(label, q, "flat")) {
                    Some(id) => id,
                    None => card.add(
                        "btn",
                        json!({
                            "component": "MaterialButton",
                            "label": label,
                            "variant": "flat",
                            "color": "primary",
                            "action": { "event": { "name": tool, "context": {} } },
                        }),
                    ),
                };
                action_btn_ids.push(id);
            }
            // Sources: with the Canvas feature OFF (default) they ride the
            // prose bubble as Markdown links (`reply_text`) — this stays a
            // no-op. With it ON, each https source becomes an OUTLINED "Open"
            // button whose signed payload is the open-doc sentinel, which the
            // A2A layer renders instead of running a turn ([`open_doc`]).
            "sources" => {
                if !doc_canvas_enabled() {
                    continue;
                }
                let Some(items) = c.get("items").and_then(Value::as_array) else {
                    continue;
                };
                for it in items {
                    let Some(url) = it
                        .get("resource")
                        .and_then(Value::as_str)
                        .filter(|u| crate::links::is_allowed_link(u))
                    else {
                        continue;
                    };
                    let label = it.get("label").and_then(Value::as_str).unwrap_or("source");
                    let (skill, doc_id) = split_skill_id(label);
                    // Shorten long labels cleanly so they don't overprint.
                    let display_label = if label.chars().count() > 24 {
                        format!("{}…", label.chars().take(23).collect::<String>())
                    } else {
                        label.to_string()
                    };
                    if let Some(id) = card.doc_button(
                        &format!("Open: {display_label}"),
                        (label, skill, doc_id, url),
                    ) {
                        source_btn_ids.push(id);
                    }
                    // At most 2 source buttons so they never crowd out actions.
                    if source_btn_ids.len() >= 2 {
                        break;
                    }
                }
            }
            "embed" if opts.embed => match Component::parse(c) {
                Some(Component::Embed {
                    url, title, height, ..
                }) if crate::links::is_allowed_link(&url) => canvases.push((url, title, height)),
                Some(comp) => root_children.extend(card.component(&comp)),
                None => {}
            },
            // The conversation components (and template Selection/Form).
            _ => {
                if let Some(comp) = Component::parse(c) {
                    root_children.extend(card.component(&comp));
                }
            }
        }
    }

    let action_rows = card.button_rows(action_btn_ids);
    root_children.extend(action_rows);
    let source_rows = card.button_rows(source_btn_ids);
    root_children.extend(source_rows);

    let mut messages = Vec::new();
    if !root_children.is_empty() {
        // Top: MaterialCard(outlined) → MaterialColumn(root_children).
        let mut flat = card.flat;
        flat.push(
            json!({ "id": "root-col", "component": "MaterialColumn", "children": root_children }),
        );
        flat.push(json!({
            "id": "root",
            "component": "MaterialCard",
            "appearance": "outlined",
            "children": ["root-col"],
        }));
        messages.extend(surface_pair(flat));
    }
    for (url, title, height) in canvases {
        messages.extend(embed_canvas(&url, &title, height.unwrap_or(640), false));
    }
    (!messages.is_empty()).then_some(messages)
}

/// Upper bounds for producer-built surfaces in one reply.
const MAX_PASSTHROUGH_SURFACES: usize = 8;
const MAX_DISPLAY_NAME_CHARS: usize = 64;
/// Producer-built surfaces get their own id prefix.
const PASSTHROUGH_PREFIX: &str = "triton-a2ui-";

/// The v0.9 messages for the producer-built surfaces in `surface.a2ui`:
/// `[{components, data_model?, theme?}]`, at most
/// [`MAX_PASSTHROUGH_SURFACES`].
///
/// Each surface is emitted verbatim as createSurface + updateComponents (+
/// updateDataModel). `components` is the flat v0.9 component list and must
/// contain the `root` component; a surface without one is dropped. `theme`
/// may set `agentDisplayName` and `primaryColor` (`#rrggbb`) only. A click on
/// a producer's button is not one of Triton's signed actions: it reaches the
/// tool as `_triton.client.a2ui_events` (the producer handles it). The
/// components themselves are the producer's responsibility: Triton does not
/// vet them, so passthrough is for upstreams the operator trusts like its own
/// code.
fn passthrough_surfaces(spec: Option<&Value>) -> Vec<Value> {
    let Some(surfaces) = spec.and_then(Value::as_array) else {
        return Vec::new();
    };
    if surfaces.len() > MAX_PASSTHROUGH_SURFACES {
        eprintln!(
            r#"{{"kind":"log","level":"warn","msg":"a2ui passthrough: surfaces beyond the cap dropped","surfaces":{},"cap":{MAX_PASSTHROUGH_SURFACES}}}"#,
            surfaces.len()
        );
    }
    surfaces
        .iter()
        .take(MAX_PASSTHROUGH_SURFACES)
        .flat_map(passthrough_messages)
        .collect()
}

fn passthrough_messages(surface: &Value) -> Vec<Value> {
    let Some(components) = surface.get("components").and_then(Value::as_array) else {
        return Vec::new();
    };
    if !components
        .iter()
        .any(|c| c.get("id").and_then(Value::as_str) == Some("root"))
    {
        eprintln!(
            r#"{{"kind":"log","level":"warn","msg":"a2ui passthrough: surface without a root component dropped"}}"#
        );
        return Vec::new();
    }
    let surface_id = format!("{PASSTHROUGH_PREFIX}{}", uuid::Uuid::new_v4());
    let create = json!({
        "surfaceId": surface_id,
        "catalogId": BASIC_CATALOG,
        "theme": passthrough_theme(surface.get("theme")),
    });
    let mut messages = vec![
        json!({ "version": "v0.9", "createSurface": create }),
        json!({ "version": "v0.9", "updateComponents": { "surfaceId": surface_id, "components": components } }),
    ];
    if let Some(model) = surface.get("data_model").filter(|m| !m.is_null()) {
        messages.push(json!({
            "version": "v0.9",
            "updateDataModel": { "surfaceId": surface_id, "path": "/", "value": model },
        }));
    }
    messages
}

/// Triton's surface theme with the producer's `agentDisplayName` and
/// `primaryColor` applied when they are well-formed; other keys are ignored.
fn passthrough_theme(spec: Option<&Value>) -> Value {
    let mut theme =
        json!({ "primaryColor": THEME_PRIMARY_COLOR, "agentDisplayName": "DataZoo Agent" });
    let Some(over) = spec.and_then(Value::as_object) else {
        return theme;
    };
    if let Some(name) = over
        .get("agentDisplayName")
        .and_then(Value::as_str)
        .map(str::trim)
        && !name.is_empty()
        && name.chars().count() <= MAX_DISPLAY_NAME_CHARS
        && !name.chars().any(char::is_control)
    {
        theme["agentDisplayName"] = json!(name);
    }
    if let Some(color) = over.get("primaryColor").and_then(Value::as_str)
        && color.len() == 7
        && color.starts_with('#')
        && color[1..].chars().all(|c| c.is_ascii_hexdigit())
    {
        theme["primaryColor"] = json!(color);
    }
    theme
}

/// A Canvas-rooted surface framing `url` (GE renders a compact opener card
/// in the stream that opens the side panel).
fn embed_canvas(url: &str, title: &str, height: u32, auto_open: bool) -> Vec<Value> {
    surface_pair(vec![
        json!({ "id": "frame", "component": "IFrameUrl", "url": url, "height": height }),
        json!({
            "id": "root",
            "component": "Canvas",
            "children": ["frame"],
            "autoOpen": auto_open,
            "cardTitle": title,
            "cardIcon": "account_tree",
        }),
    ])
}

/// Build a **Canvas-rooted** A2UI surface that shows `url` (the signed,
/// Material-themed `/docs` page) in Gemini Enterprise's resizable **side
/// panel**. `Canvas` MUST be the surface root (GE renders a compact opener
/// card in the chat stream that opens the panel — `autoOpen` opens it
/// immediately). The `IFrameUrl` host must be on GE's widget allowlist, which
/// is why the source buttons that reach here are gated by
/// [`doc_canvas_enabled`]. Returns the `createSurface` + `updateComponents`
/// message pair, ready for [`data_parts`].
pub fn build_document_canvas(url: &str, title: &str) -> Vec<Value> {
    let surface_id = format!("{SURFACE_PREFIX}{}", uuid::Uuid::new_v4());
    let flat = vec![
        json!({
            "id": "doc-frame",
            "component": "IFrameUrl",
            "url": url,
            "title": title,
            "height": 640,
        }),
        json!({
            "id": "root",
            "component": "Canvas",
            "children": ["doc-frame"],
            "autoOpen": true,
            "cardTitle": title,
            "cardDescription": "Open the source document",
            "cardIcon": "description",
        }),
    ];
    vec![
        json!({
            "version": "v0.9",
            "createSurface": {
                "surfaceId": surface_id,
                "catalogId": BASIC_CATALOG,
                "theme": {
                    "primaryColor": THEME_PRIMARY_COLOR,
                    "agentDisplayName": "DataZoo Agent",
                },
            },
        }),
        json!({
            "version": "v0.9",
            "updateComponents": { "surfaceId": surface_id, "components": flat },
        }),
    ]
}

/// Build the document as a **native Material** surface (a `MaterialCard` reply
/// in the chat stream) from a peacock `document` render's `structuredContent`:
/// a title, a `MaterialTable` of frontmatter facts, the markdown body as
/// `MaterialText`, and the event timeline. Uses ONLY the documented, GA
/// Material components — no `IFrameUrl`, no allowlist — so it renders in any
/// A2UI-GA tenant. `title` falls back to the instance id.
pub fn build_document_material(structured: &Value, title: &str) -> Vec<Value> {
    let inst = structured
        .get("instances")
        .and_then(Value::as_object)
        .and_then(|m| m.values().next());
    let heading = if title.is_empty() {
        inst.and_then(|i| i.get("id").and_then(Value::as_str))
            .unwrap_or("Document")
    } else {
        title
    };

    let mut flat: Vec<Value> = Vec::new();
    let mut children: Vec<String> = Vec::new();
    let mut n = 0usize;
    let mut add = |flat: &mut Vec<Value>, children: &mut Vec<String>, comp: Value| {
        n += 1;
        let cid = format!("d-{n}");
        let mut c = comp;
        c["id"] = json!(cid);
        flat.push(c);
        children.push(cid);
    };

    add(
        &mut flat,
        &mut children,
        json!({ "component": "MaterialText", "text": heading, "usageHint": "h3" }),
    );

    if let Some(inst) = inst {
        // Facts → a 2-column MaterialTable (rows are string-keyed objects).
        if let Some(facts) = inst.get("facts").and_then(Value::as_array) {
            let rows: Vec<Value> = facts
                .iter()
                .filter_map(|f| {
                    let k = f.get("key").and_then(Value::as_str)?;
                    let v = f.get("value").and_then(Value::as_str)?;
                    Some(json!({ "field": k, "value": v }))
                })
                .collect();
            if !rows.is_empty() {
                add(
                    &mut flat,
                    &mut children,
                    json!({
                        "component": "MaterialTable",
                        "columns": [
                            { "header": "Field", "field": "field" },
                            { "header": "Value", "field": "value" },
                        ],
                        "rows": rows,
                    }),
                );
            }
        }
        // Markdown body → MaterialText (GE renders plain text; still readable).
        if let Some(md) = inst.get("markdown").and_then(Value::as_str)
            && !md.trim().is_empty()
        {
            add(
                &mut flat,
                &mut children,
                json!({ "component": "MaterialText", "text": md, "usageHint": "body" }),
            );
        }
        // Timeline → a heading + one MaterialText line per event.
        if let Some(events) = inst.get("events").and_then(Value::as_array)
            && !events.is_empty()
        {
            add(
                &mut flat,
                &mut children,
                json!({ "component": "MaterialText", "text": "Activity", "usageHint": "subtitle2" }),
            );
            for e in events {
                let at = e.get("at").and_then(Value::as_str).unwrap_or("");
                let src = e.get("source").and_then(Value::as_str).unwrap_or("");
                let ttl = e.get("title").and_then(Value::as_str).unwrap_or("");
                let meta = [at, src]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" \u{b7} ");
                let line = if meta.is_empty() {
                    ttl.to_string()
                } else {
                    format!("{ttl} — {meta}")
                };
                add(
                    &mut flat,
                    &mut children,
                    json!({ "component": "MaterialText", "text": line, "usageHint": "caption" }),
                );
            }
        }
    }

    if children.len() == 1 {
        // Only the heading — nothing resolved; say so rather than a bare title.
        add(
            &mut flat,
            &mut children,
            json!({ "component": "MaterialText", "text": "This document is unavailable right now.", "usageHint": "body" }),
        );
    }

    flat.push(json!({ "id": "root-col", "component": "MaterialColumn", "children": children }));
    flat.push(json!({
        "id": "root", "component": "MaterialCard", "appearance": "outlined",
        "children": ["root-col"],
    }));

    let surface_id = format!("{SURFACE_PREFIX}{}", uuid::Uuid::new_v4());
    vec![
        json!({
            "version": "v0.9",
            "createSurface": {
                "surfaceId": surface_id,
                "catalogId": BASIC_CATALOG,
                "theme": {
                    "primaryColor": THEME_PRIMARY_COLOR,
                    "agentDisplayName": "DataZoo Agent",
                },
            },
        }),
        json!({
            "version": "v0.9",
            "updateComponents": { "surfaceId": surface_id, "components": flat },
        }),
    ]
}

/// Wrap each A2UI message as its OWN A2A `DataPart`.
///
/// A2A 0.3.0 types `DataPart.data` as a JSON **object**, so the A2UI message
/// stream (an array) cannot ride in a single part's `data` — a strict client
/// (Gemini Enterprise's a2a-python SDK) rejects `data: [...]` with
/// `dict_type`. One message per DataPart keeps every `data` a dict and matches
/// ADK's `create_a2ui_part` (one payload dict → one part); the receiver
/// processes the parts in order to rebuild the stream.
pub fn data_parts(messages: Vec<Value>) -> Vec<Value> {
    messages
        .into_iter()
        .map(|m| json!({ "kind": "data", "data": m, "metadata": { "mimeType": MIME } }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn find<'a>(comps: &'a [Value], id: &str) -> &'a Value {
        comps.iter().find(|c| c["id"] == id).expect("component id")
    }

    /// A producer that speaks v0.9 itself puts its surfaces in `surface.a2ui`:
    /// one createSurface + updateComponents (+ updateDataModel) per surface, in
    /// order, ahead of Triton's own card.
    #[test]
    fn a2ui_surfaces_pass_through_verbatim() {
        let result = json!({ "surface": {
            "components": [ { "kind": "text", "value": "Nine requirements, three close calls." } ],
            "a2ui": [
                {
                    "components": [
                        { "id": "root", "component": "MaterialCard", "children": ["tbl"] },
                        { "id": "tbl", "component": "MaterialTable",
                          "columns": [{ "header": "Requirement", "field": "req" }],
                          "rows": [{ "req": "Easy-Entry" }] }
                    ],
                    "data_model": { "picks": { "r04": "fold-flat mechanism" } },
                    "theme": { "agentDisplayName": "Coverage Matching", "primaryColor": "#004149" }
                },
                {
                    "components": [
                        { "id": "root", "component": "Canvas", "cardTitle": "Capability mapping",
                          "children": ["chart"] },
                        { "id": "chart", "component": "VegaChart", "spec": { "mark": "rule" } }
                    ]
                }
            ]
        }});
        let msgs = build_messages(&result).expect("messages");
        assert_eq!(msgs.len(), 5, "{msgs:?}");
        let s1 = msgs[0]["createSurface"]["surfaceId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(msgs[0]["version"], "v0.9");
        assert_eq!(msgs[0]["createSurface"]["catalogId"], BASIC_CATALOG);
        assert_eq!(
            msgs[0]["createSurface"]["theme"]["agentDisplayName"],
            "Coverage Matching"
        );
        assert_eq!(msgs[0]["createSurface"]["theme"]["primaryColor"], "#004149");
        assert_eq!(msgs[1]["updateComponents"]["surfaceId"], s1);
        assert_eq!(
            msgs[1]["updateComponents"]["components"][1]["component"],
            "MaterialTable"
        );
        assert_eq!(msgs[2]["updateDataModel"]["surfaceId"], s1);
        assert_eq!(msgs[2]["updateDataModel"]["path"], "/");
        assert_eq!(
            msgs[2]["updateDataModel"]["value"]["picks"]["r04"],
            "fold-flat mechanism"
        );
        let s2 = msgs[3]["createSurface"]["surfaceId"].as_str().unwrap();
        assert_ne!(s1, s2, "each surface has its own id");
        assert_eq!(
            msgs[3]["createSurface"]["theme"]["agentDisplayName"],
            "DataZoo Agent"
        );
        assert_eq!(
            msgs[4]["updateComponents"]["components"][0]["component"],
            "Canvas"
        );
    }

    /// Passthrough surfaces come first; Triton's own card (buttons, charts)
    /// follows as its own surface.
    #[test]
    fn a2ui_surfaces_precede_tritons_own_card() {
        let result = json!({ "surface": {
            "components": [ { "kind": "button", "label": "Again", "tool": "t", "args": { "question": "again" } } ],
            "a2ui": [ { "components": [ { "id": "root", "component": "MaterialText", "text": "x" } ] } ]
        }});
        let msgs = build_messages(&result).expect("messages");
        assert_eq!(msgs.len(), 4);
        assert_eq!(
            msgs[1]["updateComponents"]["components"][0]["component"],
            "MaterialText"
        );
        assert!(msgs[2]["createSurface"]["surfaceId"].is_string());
        assert_eq!(
            msgs[3]["updateComponents"]["components"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["id"],
            "root"
        );
    }

    /// Only `agentDisplayName` and `primaryColor` are taken from the producer,
    /// and only when well-formed.
    #[test]
    fn a2ui_theme_is_allowlisted() {
        let result = json!({ "surface": { "a2ui": [{
            "components": [ { "id": "root", "component": "MaterialCard", "children": [] } ],
            "theme": { "agentDisplayName": "x".repeat(MAX_DISPLAY_NAME_CHARS + 1),
                       "primaryColor": "red", "logoUrl": "https://evil.example/l.png" }
        }]}});
        let msgs = build_messages(&result).expect("messages");
        let theme = &msgs[0]["createSurface"]["theme"];
        assert_eq!(theme["agentDisplayName"], "DataZoo Agent");
        assert_eq!(theme["primaryColor"], THEME_PRIMARY_COLOR);
        assert!(theme.get("logoUrl").is_none());
    }

    /// At most MAX_PASSTHROUGH_SURFACES surfaces go out per result.
    #[test]
    fn a2ui_surfaces_are_capped() {
        let one =
            json!({ "components": [ { "id": "root", "component": "MaterialText", "text": "x" } ] });
        let result = json!({ "surface": { "a2ui": vec![one; MAX_PASSTHROUGH_SURFACES + 3] } });
        let msgs = build_messages(&result).expect("messages");
        let creates = msgs
            .iter()
            .filter(|m| m.get("createSurface").is_some())
            .count();
        assert_eq!(creates, MAX_PASSTHROUGH_SURFACES);
    }

    /// Without A2UI negotiated only Triton's own card goes out.
    #[test]
    fn negotiated_builder_drops_passthrough_without_a2ui() {
        let result = json!({ "surface": {
            "components": [ { "kind": "button", "label": "Again", "tool": "t", "args": { "question": "again" } } ],
            "a2ui": [ { "components": [ { "id": "root", "component": "MaterialText", "text": "x" } ] } ]
        }});
        assert_eq!(build_messages_negotiated(&result, true).unwrap().len(), 4);
        let plain = build_messages_negotiated(&result, false).unwrap();
        assert_eq!(plain.len(), 2);
        assert!(
            plain[0]["createSurface"]["surfaceId"]
                .as_str()
                .unwrap()
                .starts_with(SURFACE_PREFIX)
        );
    }

    /// A surface without a `root` component cannot render; it is dropped
    /// rather than sent to a client that would show an empty card.
    #[test]
    fn a2ui_surface_without_root_is_dropped() {
        let result = json!({ "surface": { "a2ui": [{ "components": [
            { "id": "card", "component": "MaterialCard", "children": [] }
        ]}]}});
        assert!(build_messages(&result).is_none());
    }

    /// The typed vocabulary every other channel uses ignores `surface.a2ui`, so
    /// a producer's passthrough never breaks a chat channel or MCP.
    #[test]
    fn typed_consumers_ignore_the_a2ui_sibling() {
        let result = json!({ "surface": {
            "components": [ { "kind": "text", "value": "still here" } ],
            "a2ui": [ { "components": [ { "id": "root", "component": "MaterialText", "text": "x" } ] } ]
        }});
        let typed = crate::a2ui::extract_surface(&result).expect("typed surface");
        assert_eq!(typed.components.len(), 1);
    }

    #[test]
    fn maps_text_chart_button_and_sources() {
        let result = json!({ "surface": { "components": [
            { "kind": "text", "value": "Initech leads at $2,500.75." },
            { "kind": "report", "report_id": "sales", "image_url": "https://agent-lab.data-zoo.de/report/img/tok", "title": "Sales" },
            { "kind": "button", "label": "What does Initech buy?", "tool": "assistant", "args": { "question": "What does Initech buy?" } },
            { "kind": "sources", "items": [
                { "label": "sales-by-customer", "resource": "https://agent-lab.data-zoo.de/docs/tok" },
                { "label": "ui-only", "resource": "ui://peacock/x" }
            ] }
        ] } });

        let msgs = build_messages(&result).expect("renderable");
        assert_eq!(msgs.len(), 2, "createSurface + updateComponents");
        assert_eq!(msgs[0]["version"], "v0.9");
        assert_eq!(msgs[0]["createSurface"]["catalogId"], BASIC_CATALOG);
        // createSurface carries a v0.9 theme so GE styles the card (Material
        // primary accents) with the brand color.
        assert_eq!(
            msgs[0]["createSurface"]["theme"]["primaryColor"],
            THEME_PRIMARY_COLOR
        );
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();

        // Root is a MaterialCard whose single child (a LIST) is a MaterialColumn.
        let root = find(comps, "root");
        assert_eq!(root["component"], "MaterialCard");
        let col_id = root["children"][0].as_str().unwrap();
        let col = find(comps, col_id);
        assert_eq!(col["component"], "MaterialColumn");

        // Exactly one Image (basic — GE's MaterialImage won't load the signed
        // URL), carrying the signed chart URL.
        let img = comps
            .iter()
            .find(|c| c["component"] == "Image")
            .expect("image");
        assert_eq!(img["url"], "https://agent-lab.data-zoo.de/report/img/tok");

        // The re-ask MaterialButton carries its label directly (no child Text).
        // GE strips our name/context on click but PRESERVES the component id,
        // so the id itself carries the HMAC-signed complete message
        // (`a<N>-<token>`): any replica holding the key recovers it, no store.
        let btn = comps
            .iter()
            .find(|c| {
                c["component"] == "MaterialButton" && c["action"]["event"]["name"] == "send_message"
            })
            .expect("re-ask button");
        assert_eq!(btn["label"], "What does Initech buy?");
        assert_eq!(btn["variant"], "flat");
        assert_eq!(btn["color"], "primary");
        let btn_id = btn["id"].as_str().unwrap();
        assert!(btn_id.starts_with("a2-"), "{btn_id}");
        let sid = msgs[0]["createSurface"]["surfaceId"].as_str().unwrap();
        assert_eq!(
            question_for(sid, btn_id).as_deref(),
            Some("What does Initech buy?"),
        );
        // The surface id is irrelevant now — a click on another replica, or
        // with GE's truncated surfaceId, resolves the same.
        assert_eq!(
            question_for("nope", btn_id).as_deref(),
            Some("What does Initech buy?")
        );
        // A forged / legacy / truncated id resolves to nothing.
        assert_eq!(question_for(sid, "btn-2"), None);
        assert_eq!(question_for(sid, "a2-garbage.token"), None);
        let tampered = format!("{}x", btn_id);
        assert_eq!(question_for(sid, &tampered), None);

        // Buttons are laid out in MaterialRows (a grid, ≤2 per row), not
        // stacked as direct Column children.
        let row = comps
            .iter()
            .find(|c| c["component"] == "MaterialRow")
            .expect("button row");
        assert!(
            row["children"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == btn_id),
            "button must live inside the Row: {row}"
        );
        assert_eq!(
            col["children"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| *c == btn_id),
            None,
            "button must NOT be a direct Column child"
        );

        // Prose is NOT duplicated inside the card (the bubble shows it).
        assert!(
            !comps.iter().any(|c| {
                (c["component"] == "Text" || c["component"] == "MaterialText")
                    && c["text"] == "Initech leads at $2,500.75."
            }),
            "card must not repeat the answer prose"
        );

        // Sources are NOT in the card (GE can't hyperlink there); they ride the
        // prose bubble as Markdown links via reply_text. So the card has no
        // "sources" component and no source Text line at all.
        assert!(
            !comps.iter().any(|c| c["id"] == "sources"),
            "card must not carry a sources component"
        );
        assert!(
            !comps
                .iter()
                .any(|c| c["component"] == "Text"
                    && c["text"].as_str().unwrap_or("").contains("Source")),
            "card must not carry a source text line"
        );
        assert!(
            !comps
                .iter()
                .any(|c| c["action"]["functionCall"].is_object()),
            "no functionCall actions (GE rejects them)"
        );
    }

    /// Stateless across replicas: two processes that share only the key
    /// resolve each other's card ids; a different key resolves nothing.
    #[test]
    fn a_card_id_signed_on_one_replica_opens_on_another() {
        let key =
            || triton_correlation::KeyRing::single(b"shared-a2ui-action-key".to_vec()).unwrap();
        let (a, b) = (ActionSigner::new(key()), ActionSigner::new(key()));
        let result = json!({ "surface": { "components": [
            { "kind": "chips", "chips": [ { "label": "Order matcher",
              "message": "Who owns the order matcher in Sales?" } ] }
        ] } });
        let msgs = build_messages_with(&result, &a, GeOptions::default()).unwrap();
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();
        let id = comps
            .iter()
            .find(|c| c["component"] == "MaterialButton")
            .and_then(|c| c["id"].as_str())
            .unwrap();
        assert_eq!(
            b.open(id).as_deref(),
            Some("Who owns the order matcher in Sales?")
        );
        let other = ActionSigner::new(
            triton_correlation::KeyRing::single(b"another-replica-key!".to_vec()).unwrap(),
        );
        assert_eq!(other.open(id), None);
    }

    #[test]
    fn report_with_vega_spec_emits_native_vegachart() {
        // When the report carries a Vega-Lite `vega_spec` (injected by the A2A
        // layer from peacock's render_report), GE gets a native interactive
        // VegaChart, not the static Image.
        let spec = json!({
            "mark": "bar",
            "data": { "values": [{ "c": "widgets", "v": 900 }] },
            // color == x → the legend is redundant and must be dropped for GE.
            "encoding": {
                "x": { "field": "c" },
                "y": { "field": "v" },
                "color": { "field": "c" },
            },
        });
        let result = json!({ "surface": { "components": [
            { "kind": "report", "report_id": "sales", "image_url": "https://x/img/tok", "vega_spec": spec },
        ] } });
        let msgs = build_messages(&result).expect("renderable");
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();
        let vega = comps
            .iter()
            .find(|c| c["component"] == "VegaChart")
            .expect("VegaChart");
        assert_eq!(vega["spec"]["mark"], "bar");
        // GE presentation tweaks: horizontal x labels, redundant legend dropped,
        // responsive width.
        assert_eq!(vega["spec"]["encoding"]["x"]["axis"]["labelAngle"], 0);
        assert!(vega["spec"]["encoding"]["color"]["legend"].is_null());
        assert_eq!(vega["spec"]["width"], "container");
        // Static Image must NOT also be emitted (no double chart).
        assert!(!comps.iter().any(|c| c["component"] == "Image"));
    }

    #[test]
    fn no_renderable_components_yields_none() {
        assert!(build_messages(&json!({ "text": "plain" })).is_none());
        assert!(build_messages(&json!({ "surface": { "components": [] } })).is_none());
        // A report with no image_url has nothing to draw.
        assert!(
            build_messages(&json!({ "surface": { "components": [
            { "kind": "report", "report_id": "x" }
        ] } }))
            .is_none()
        );
    }

    #[test]
    fn build_document_canvas_is_canvas_rooted_with_an_iframe() {
        let msgs =
            build_document_canvas("https://agent-lab.data-zoo.de/docs/tok", "Beverages GmbH");
        assert_eq!(msgs.len(), 2, "createSurface + updateComponents");
        assert_eq!(msgs[0]["createSurface"]["catalogId"], BASIC_CATALOG);
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();
        // Root MUST be the Canvas (GE requires it as the surface root), and it
        // holds the IFrameUrl pointing at the signed /docs page.
        let root = find(comps, "root");
        assert_eq!(root["component"], "Canvas");
        assert_eq!(root["autoOpen"], true);
        assert_eq!(root["cardTitle"], "Beverages GmbH");
        let child = root["children"][0].as_str().unwrap();
        let frame = find(comps, child);
        assert_eq!(frame["component"], "IFrameUrl");
        assert_eq!(frame["url"], "https://agent-lab.data-zoo.de/docs/tok");
    }

    /// F7: an open-doc click is a TYPED action recovered from a signed id,
    /// never an in-band prefix a caller could type. The legacy text form
    /// (the bare sentinel) is refused, and the doc-open token lives no
    /// longer than a day.
    #[test]
    fn open_doc_is_a_typed_signed_action_never_free_text() {
        let signer = ActionSigner::new(
            triton_correlation::KeyRing::single(b"open-doc-test-key-123".to_vec()).unwrap(),
        );
        let id = signer
            .sign_open_doc(
                1,
                "account \u{b7} beverages",
                "account",
                "beverages",
                "https://x/docs/tok",
            )
            .expect("signs");
        match signer.open_action(&id) {
            Some(GeAction::OpenDoc {
                label,
                skill,
                id,
                url,
            }) => {
                assert_eq!(label, "account \u{b7} beverages");
                assert_eq!((skill.as_str(), id.as_str()), ("account", "beverages"));
                assert_eq!(url, "https://x/docs/tok");
            }
            other => panic!("expected OpenDoc, got {other:?}"),
        }
        // A send-message id is a Message, not a doc.
        let msg = format!("a2-{}", signer.sign("What does Initech buy?").unwrap());
        assert_eq!(
            signer.open_action(&msg),
            Some(GeAction::Message("What does Initech buy?".into()))
        );
        // Free text that merely LOOKS like the old sentinel opens nothing.
        let typed = "\u{1}a2ui-open-doc\u{1}x\u{1}account\u{1}secret\u{1}https://evil/docs";
        assert!(
            open_doc(typed).is_none(),
            "a caller-typed sentinel must not open a doc"
        );
        assert!(open_doc("What does Initech buy?").is_none());
        // Expiry: at most 24 h (hour granularity, rounded up → ≤ 25 h).
        let token = id.split_once('-').unwrap().1;
        let body = token.split('.').next().unwrap();
        use base64::Engine;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .unwrap();
        let x = serde_json::from_slice::<Value>(&raw).unwrap()["x"]
            .as_u64()
            .unwrap();
        let now_h = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 3600;
        assert!(x <= now_h + 25, "doc-open TTL too long: {x} vs now {now_h}");
    }

    #[test]
    fn build_document_material_uses_native_widgets() {
        // The allowlist-free GE render: MaterialCard/Table/Text, no IFrameUrl.
        let structured = json!({
            "document": { "skill": "account", "id": "beverages" },
            "instances": { "self": {
                "id": "beverages", "skill": "account",
                "facts": [ { "key": "name", "value": "Beverages GmbH" } ],
                "markdown": "Renewal at risk.",
                "events": [ { "title": "Flagged", "at": "2026-09-01", "source": "analyst" } ],
            } },
        });
        let msgs = build_document_material(&structured, "account \u{b7} beverages");
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();
        assert_eq!(find(comps, "root")["component"], "MaterialCard");
        assert!(comps.iter().any(|c| c["component"] == "MaterialTable"
            && c["rows"][0]["field"] == "name"
            && c["rows"][0]["value"] == "Beverages GmbH"));
        assert!(
            comps
                .iter()
                .any(|c| c["component"] == "MaterialText" && c["text"] == "Renewal at risk.")
        );
        // No IFrameUrl / Canvas — this is the documented-widget path.
        assert!(!comps.iter().any(|c| c["component"] == "IFrameUrl"));
        assert!(!comps.iter().any(|c| c["component"] == "Canvas"));
    }

    #[test]
    fn sources_stay_out_of_the_card_when_the_canvas_flag_is_off() {
        // Default (flag off): a sources-only surface renders nothing in-card —
        // sources ride the prose bubble as Markdown links, unchanged. The
        // flag-ON rendering (source → outlined Open button → Canvas) is
        // verified live in the GE UI (the OnceLock-cached env flag can't be
        // toggled per-test in a shared process).
        let result = json!({ "surface": { "components": [
            { "kind": "sources", "items": [
                { "label": "sales-by-customer", "resource": "https://agent-lab.data-zoo.de/docs/tok" }
            ] }
        ] } });
        assert!(
            !doc_canvas_enabled(),
            "flag defaults off in the test process"
        );
        assert!(
            build_messages(&result).is_none(),
            "with the canvas flag off, a sources-only surface renders nothing in-card"
        );
    }

    #[test]
    fn data_parts_are_one_dict_each_with_the_ge_mime() {
        let ps = data_parts(vec![
            json!({"version":"v0.9","a":1}),
            json!({"version":"v0.9","b":2}),
        ]);
        assert_eq!(ps.len(), 2, "one DataPart per A2UI message");
        for p in &ps {
            assert_eq!(p["kind"], "data");
            assert_eq!(p["metadata"]["mimeType"], "application/json+a2ui");
            // A2A DataPart.data MUST be an object, never an array.
            assert!(p["data"].is_object(), "DataPart.data must be a dict: {p}");
        }
    }
}
