//! What a channel can show (#352): the single source of truth for a chat
//! adapter's surface capabilities.
//!
//! Each chat crate declares one `pub const SURFACE: SurfaceProfile`, and its
//! surface mapper reads its numeric caps (text size, button count) from that
//! const instead of keeping loose constants of its own. The same profile is
//! what Triton advertises to upstream agents, so the hint and the mapper
//! cannot drift apart.
//!
//! The profile is the **nominal** capability. The mapper still has the final
//! say on combinations: on WhatsApp, for example, buttons lose to a selection
//! list or a dashboard image in the same reply. An agent that ignores the
//! profile keeps working; its reply is degraded exactly as before.

use serde::Serialize;

/// How much markdown the channel renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Markdown {
    /// Markdown syntax reaches the user verbatim (or escaped).
    None,
    /// Emphasis and links, no structure (WhatsApp `*bold*`, Google Chat).
    Basic,
    /// Full markdown, tables included.
    Full,
}

/// What happens to a markdown table in the answer text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tables {
    /// Shown as raw text, pipes and all. Avoid tables on this channel.
    Text,
    /// Rendered as a real table (card table, markdown table, HTML table).
    Native,
}

/// What happens to a `report` (chart) component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Charts {
    /// Dropped from the reply.
    None,
    /// Reduced to a one-line caption.
    Caption,
    /// Rendered to an image and attached.
    Image,
    /// Drawn natively by the channel (e.g. an Adaptive Card chart).
    Native,
}

/// One channel's surface capabilities. `const`-constructible so it can be the
/// mapper's own source of its caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SurfaceProfile {
    /// The channel label agents see in `X-Triton-Channel`.
    pub channel: &'static str,
    pub markdown: Markdown,
    pub tables: Tables,
    /// Whether follow-up buttons render as tappable actions at all.
    pub buttons: bool,
    /// The most buttons one reply shows; the rest are dropped. `None` when
    /// Triton enforces no cap of its own.
    pub max_buttons: Option<usize>,
    pub charts: Charts,
    /// Whether the channel can carry an image (e.g. a rasterised dashboard).
    pub images: bool,
    /// The reply text is truncated beyond this many bytes (UTF-8).
    pub max_text_bytes: usize,
    /// Whether the reply can arrive progressively (streamed or patched).
    pub streaming: bool,
    /// Whether the first line of the answer becomes a subject (email).
    pub subject_line: bool,
    /// One line an agent can drop into its prompt as-is.
    pub guidance: &'static str,
}

impl SurfaceProfile {
    /// `max_buttons` for a mapper that needs a hard cap in a `const`.
    /// Fails compilation (const evaluation) if the profile declares none.
    pub const fn button_cap(&self) -> usize {
        match self.max_buttons {
            Some(n) => n,
            None => panic!("this surface profile declares no button cap"),
        }
    }
}
