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

use std::collections::HashMap;
use std::sync::Arc;

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
    /// The reply text is truncated beyond this many bytes (UTF-8). `None`
    /// when Triton enforces no cap of its own (the HTTP trio).
    pub max_text_bytes: Option<usize>,
    /// Whether the reply can arrive progressively (streamed or patched).
    pub streaming: bool,
    /// Whether the first line of the answer becomes a subject (email).
    pub subject_line: bool,
    /// One line an agent can drop into its prompt as-is.
    pub guidance: &'static str,
}

impl SurfaceProfile {
    /// `max_text_bytes` for a mapper that needs a hard cap in a `const`.
    /// Fails compilation (const evaluation) if the profile declares none.
    pub const fn text_cap(&self) -> usize {
        match self.max_text_bytes {
            Some(n) => n,
            None => panic!("this surface profile declares no text cap"),
        }
    }

    /// `max_buttons` for a mapper that needs a hard cap in a `const`.
    /// Fails compilation (const evaluation) if the profile declares none.
    pub const fn button_cap(&self) -> usize {
        match self.max_buttons {
            Some(n) => n,
            None => panic!("this surface profile declares no button cap"),
        }
    }
}

/// REST (`/v1/tools`): the full A2UI surface reaches the client (the
/// Explorer, a web app), which renders every component.
pub const REST: SurfaceProfile = SurfaceProfile {
    channel: "rest",
    markdown: Markdown::Full,
    tables: Tables::Native,
    buttons: true,
    max_buttons: None,
    charts: Charts::Native,
    images: true,
    max_text_bytes: None,
    streaming: true,
    subject_line: false,
    guidance: "Web client: the full surface renders. Markdown, tables, buttons and charts all work.",
};

/// MCP `tools/call`: text-bubble hosts get the surface flattened to markdown
/// prose plus an inline chart image; the typed envelope rides in
/// `structuredContent` for clients that read it.
pub const MCP: SurfaceProfile = SurfaceProfile {
    channel: "mcp",
    markdown: Markdown::Full,
    tables: Tables::Native,
    buttons: false,
    max_buttons: None,
    charts: Charts::Image,
    images: true,
    max_text_bytes: None,
    streaming: false,
    subject_line: false,
    guidance: "MCP host: the answer is shown as markdown prose with an inline chart image. Buttons are not shown.",
};

/// A2A: the answer is served as markdown prose with an inline chart image.
pub const A2A: SurfaceProfile = SurfaceProfile {
    channel: "a2a",
    markdown: Markdown::Full,
    tables: Tables::Native,
    buttons: false,
    max_buttons: None,
    charts: Charts::Image,
    images: true,
    max_text_bytes: None,
    streaming: true,
    subject_line: false,
    guidance: "A2A client: the answer is shown as markdown prose with an inline chart image. Buttons are not shown.",
};

/// A2A with the A2UI extension active (e.g. Gemini Enterprise): the client
/// renders native A2UI components, so buttons, tables and charts all work.
/// Same `a2a` channel; chosen per request from the activated extensions.
pub const A2A_A2UI: SurfaceProfile = SurfaceProfile {
    channel: "a2a",
    buttons: true,
    charts: Charts::Native,
    guidance: "A2A client with A2UI (e.g. Gemini Enterprise): native components. Markdown, tables, buttons and charts all render.",
    ..A2A
};

/// Registry key of the [`A2A_A2UI`] variant. It reports the `a2a` channel
/// but is tuned and listed separately, since plain-A2A guidance ("buttons are
/// not shown") is false for an A2UI client.
pub const A2A_A2UI_KEY: &str = "a2a+a2ui";

/// Version of the descriptor wire shape. Bump it, and document the change in
/// `doc/architecture.md`, whenever a field changes meaning or is removed.
pub const DESCRIPTOR_VERSION: u32 = 1;

/// Whether follow-up buttons render, and how many.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ButtonSupport {
    pub supported: bool,
    pub max: Option<usize>,
}

/// The wire form of a profile, sent to upstream agents as `_triton.surface`
/// and listed by `GET /v1/manifest`. Non-authoritative rendering advice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SurfaceDescriptor {
    pub v: u32,
    pub channel: String,
    pub markdown: Markdown,
    pub tables: Tables,
    pub buttons: ButtonSupport,
    pub charts: Charts,
    pub images: bool,
    pub max_text_bytes: Option<usize>,
    pub streaming: bool,
    pub subject_line: bool,
    pub guidance: String,
}

impl SurfaceDescriptor {
    /// The descriptor for `profile`, with the operator's `guidance` override
    /// in place of the profile's default when one is given.
    pub fn from_profile(profile: &SurfaceProfile, guidance: Option<&str>) -> Self {
        Self {
            v: DESCRIPTOR_VERSION,
            channel: profile.channel.to_string(),
            markdown: profile.markdown,
            tables: profile.tables,
            buttons: ButtonSupport {
                supported: profile.buttons,
                max: if profile.buttons {
                    profile.max_buttons
                } else {
                    None
                },
            },
            charts: profile.charts,
            images: profile.images,
            max_text_bytes: profile.max_text_bytes,
            streaming: profile.streaming,
            subject_line: profile.subject_line,
            guidance: guidance.unwrap_or(profile.guidance).to_string(),
        }
    }
}

/// Channel → descriptor, consulted by the dispatcher for every dispatch.
#[derive(Debug, Clone, Default)]
pub struct SurfaceRegistry {
    by_channel: HashMap<String, Arc<SurfaceDescriptor>>,
}

impl SurfaceRegistry {
    /// A registry holding only the HTTP trio (`rest`, `mcp`, `a2a`). The
    /// binary adds the chat channels; an embedding host gets these defaults.
    pub fn with_trio_defaults() -> Self {
        let mut r = Self::default();
        for p in [&REST, &MCP, &A2A] {
            r.insert(p, None);
        }
        r.insert_as(A2A_A2UI_KEY, &A2A_A2UI, None);
        r
    }

    /// Register (or replace) a channel's descriptor.
    pub fn insert(&mut self, profile: &SurfaceProfile, guidance: Option<&str>) {
        self.by_channel.insert(
            profile.channel.to_string(),
            Arc::new(SurfaceDescriptor::from_profile(profile, guidance)),
        );
    }

    /// Register (or replace) a descriptor under a key other than its channel
    /// (a request-scoped variant such as [`A2A_A2UI_KEY`]).
    pub fn insert_as(&mut self, key: &str, profile: &SurfaceProfile, guidance: Option<&str>) {
        self.by_channel.insert(
            key.to_string(),
            Arc::new(SurfaceDescriptor::from_profile(profile, guidance)),
        );
    }

    pub fn get(&self, channel: &str) -> Option<Arc<SurfaceDescriptor>> {
        self.by_channel.get(channel).cloned()
    }

    /// Every registered descriptor, keyed by channel or variant key (for
    /// discovery).
    pub fn all(&self) -> impl Iterator<Item = (&str, &SurfaceDescriptor)> {
        self.by_channel
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_ref()))
    }
}
