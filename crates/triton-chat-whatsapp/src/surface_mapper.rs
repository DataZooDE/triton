//! v0.2 PR 31 — L6' surface mapper for WhatsApp Cloud API.
//!
//! Takes the canonical `triton_core::a2ui::Surface` and renders it
//! into a WhatsApp `messages` body. PR 31's scope is intentionally
//! narrow: text + narration only. Buttons, Selection, Form, and
//! Dashboard components are counted into per-category `deferred_*`
//! fields so the operator can see the gap via `tracing::warn`, but
//! they are NOT rendered through interactive primitives — that
//! lands in PR 32.
//!
//! Output discipline:
//!   * text → plain text (WhatsApp doesn't speak HTML/Markdown in
//!     the same parse-mode way Telegram does; we keep it simple
//!     until PR 32 introduces native primitives).
//!   * narration → `_<text>_` (WhatsApp's documented italic markup;
//!     applied client-side only, no risk of breaking parsing if it
//!     fails to render).
//!   * buttons/selection/form/dashboard → deferred.
//!
//! Truncation strategy mirrors Telegram's: cut between components
//! first, raw-text-budget fallback for a single oversized chunk.
//! The cap is WhatsApp's documented 4096 chars; we treat it as
//! bytes (any UTF-8 string ≤ 4096 bytes is ≤ 4096 UTF-16 code
//! units, so a byte cap only ever rejects messages WhatsApp itself
//! would also reject).

use serde_json::{Value, json};
use triton_core::a2ui::{Component, DashboardTile, Surface, extract_surface};

/// WhatsApp `text.body` hard limit. Same byte-conservative reading
/// as Telegram's TELEGRAM_TEXT_MAX_BYTES (UTF-8 length ≤ UTF-16
/// length).
pub const WHATSAPP_TEXT_MAX_BYTES: usize = 4096;

/// Sentinel appended when we truncate to fit
/// [`WHATSAPP_TEXT_MAX_BYTES`]. Bracketed so it's visibly an
/// adapter artefact, not the tool's output.
const TRUNCATION_SENTINEL: &str = "\n\n[truncated — content exceeded WhatsApp's 4096-byte limit]";

/// Rendered WhatsApp message body.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub text: String,
    /// Buttons in PR 31 cannot ship as interactive primitives; we
    /// count them so the operator-visible tracing::warn line names
    /// the category. PR 32 lifts this restriction.
    pub deferred_buttons: usize,
    pub deferred_selections: usize,
    pub deferred_forms: usize,
    /// Number of `Component::Dashboard` entries we couldn't ship.
    /// PR 38 wires the rasterizer, so the FIRST Dashboard in a
    /// surface is surfaced via [`Self::dashboard`] for the adapter
    /// to rasterise + send as a WhatsApp image message. Any
    /// subsequent dashboards bump this counter — WhatsApp image
    /// messages carry one media attachment.
    pub deferred_dashboards: usize,
    pub truncated: bool,
    /// PR 38: the FIRST `Component::Dashboard` found in the surface,
    /// surfaced to the caller for rasterisation. The adapter calls
    /// the rasterizer service, uploads the PNG to WhatsApp's media
    /// endpoint, and dispatches an `image` message carrying the
    /// returned `media_id`. Mirrors Telegram PR 36's
    /// `RenderedMessage::dashboard` slot.
    pub dashboard: Option<RasterDashboard>,
}

/// Dashboard payload the caller passes to the rasterizer. Local to
/// the adapter — each chat adapter owns its own L6′ rendering
/// concerns; sharing the type across crates would couple them.
#[derive(Debug, Clone)]
pub struct RasterDashboard {
    pub title: String,
    pub tiles: Vec<DashboardTile>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderError {
    /// Zero renderable components after deferring everything that
    /// isn't Text or Narration.
    EmptyAfterRender,
}

/// Try to render `result` as a WhatsApp message. Returns `None`
/// when the result isn't an A2UI surface — caller falls back to
/// the bare-text path. Returns `Some(Err(...))` when the result IS
/// an A2UI surface but renders to nothing usable.
pub fn try_render_surface(result: &Value) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
}

/// Render a [`Surface`] into a `RenderedMessage` or a
/// `RenderError`. Public so the in-crate unit tests can exercise
/// the mapper without spinning the whole binary.
pub fn render(surface: &Surface) -> Result<RenderedMessage, RenderError> {
    let mut chunks: Vec<String> = Vec::new();
    let mut deferred_buttons = 0usize;
    let mut deferred_selections = 0usize;
    let mut deferred_forms = 0usize;
    let mut deferred_dashboards = 0usize;
    // PR 38: first Dashboard surfaced to the caller for rasterisation.
    // WhatsApp image messages carry one media attachment, so any
    // subsequent dashboards bump `deferred_dashboards`.
    let mut first_dashboard: Option<RasterDashboard> = None;
    // Track which chunks came from raw text so the single-chunk
    // fallback path can re-render at a smaller budget without
    // corrupting markup. Narration carries `_..._` wrappers we
    // mustn't split mid-token.
    let mut raw_sources: Vec<Option<RawChunk>> = Vec::new();

    for c in &surface.components {
        match c {
            Component::Text { value } => {
                chunks.push(value.clone());
                raw_sources.push(Some(RawChunk {
                    kind: RawKind::Text,
                    raw: value.clone(),
                }));
            }
            Component::Narration { text } => {
                chunks.push(format!("_{text}_"));
                raw_sources.push(Some(RawChunk {
                    kind: RawKind::Narration,
                    raw: text.clone(),
                }));
            }
            Component::Button { .. } => {
                // PR 31 scope-limit: interactive primitives defer
                // until PR 32. The Button's tool/args MUST NOT leak
                // into the text body — that would let the user
                // re-execute by typing.
                deferred_buttons += 1;
            }
            Component::Selection { .. } => {
                deferred_selections += 1;
            }
            Component::Form { .. } => {
                deferred_forms += 1;
            }
            Component::Dashboard { title, tiles } => {
                // PR 38: the manifest's `dashboard: rasterised_png`
                // degrade rule now has a real renderer behind it.
                // The mapper surfaces the FIRST Dashboard for
                // rasterisation. The adapter uploads the PNG to
                // WhatsApp's media endpoint and dispatches an
                // `image` message carrying the returned media_id.
                //
                // We deliberately do NOT push a text chunk for the
                // dashboard: surrounding Text/Narration components
                // become the IMAGE's caption (WhatsApp's image
                // message supports an inline `caption` field), and
                // the dashboard itself is the image. The fallback
                // path adds its own placeholder string when needed.
                if first_dashboard.is_none() {
                    first_dashboard = Some(RasterDashboard {
                        title: title.clone(),
                        tiles: tiles.clone(),
                    });
                } else {
                    deferred_dashboards += 1;
                }
            }
        }
    }

    // PR 38: a dashboard-only surface is legitimate — WhatsApp's
    // image messages don't require a caption, so we can ship a
    // photo with no text. Only refuse when there's literally
    // nothing renderable.
    if chunks.is_empty() && first_dashboard.is_none() {
        return Err(RenderError::EmptyAfterRender);
    }

    let joined = chunks.join("\n\n");
    if joined.len() <= WHATSAPP_TEXT_MAX_BYTES {
        return Ok(RenderedMessage {
            text: joined,
            deferred_buttons,
            deferred_selections,
            deferred_forms,
            deferred_dashboards,
            truncated: false,
            dashboard: first_dashboard,
        });
    }

    // Over cap. Walk the chunk prefix that fits under
    // `cap - sentinel`, leaving every cut on a between-component
    // boundary so narration `_..._` markup stays balanced.
    let budget = WHATSAPP_TEXT_MAX_BYTES - TRUNCATION_SENTINEL.len();
    let mut accepted: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for chunk in &chunks {
        let sep_cost = if accepted.is_empty() { 0 } else { 2 };
        if total + sep_cost + chunk.len() > budget {
            break;
        }
        total += sep_cost + chunk.len();
        accepted.push(chunk.as_str());
    }

    if !accepted.is_empty() {
        let mut out = accepted.join("\n\n");
        out.push_str(TRUNCATION_SENTINEL);
        return Ok(RenderedMessage {
            text: out,
            deferred_buttons,
            deferred_selections,
            deferred_forms,
            deferred_dashboards,
            truncated: true,
            dashboard: first_dashboard,
        });
    }

    // Even the first chunk is too large. We can budget-truncate
    // the raw text and re-wrap. WhatsApp's `_..._` markup spans the
    // full narration body so we re-emit the wrapper around the
    // truncated raw.
    let first_raw = raw_sources[0].clone();
    let out = match first_raw {
        Some(RawChunk {
            kind: RawKind::Text,
            raw,
        }) => {
            let trimmed = budget_raw(&raw, budget);
            let mut s = trimmed.to_string();
            s.push_str(TRUNCATION_SENTINEL);
            s
        }
        Some(RawChunk {
            kind: RawKind::Narration,
            raw,
        }) => {
            // Narration adds `_` + `_` = 2 bytes of wrapping.
            let inner_budget = budget.saturating_sub(2);
            let trimmed = budget_raw(&raw, inner_budget);
            let mut s = format!("_{trimmed}_");
            s.push_str(TRUNCATION_SENTINEL);
            s
        }
        None => TRUNCATION_SENTINEL.trim_start().to_string(),
    };
    Ok(RenderedMessage {
        text: out,
        deferred_buttons,
        deferred_selections,
        deferred_forms,
        deferred_dashboards,
        truncated: true,
        dashboard: first_dashboard,
    })
}

/// Build the WhatsApp `messages` body for an `image` message
/// carrying a previously-uploaded media_id. The PR 38 dashboard
/// path uploads the rasterised PNG to `/v18.0/{id}/media` first,
/// then sends this body to `/v18.0/{id}/messages`. The optional
/// caption is the rendered Text/Narration content that surrounded
/// the dashboard in the source surface.
pub fn build_image_message_body(to: &str, media_id: &str, caption: Option<&str>) -> Value {
    let mut image = serde_json::Map::new();
    image.insert("id".into(), Value::String(media_id.to_string()));
    if let Some(c) = caption.filter(|s| !s.is_empty()) {
        image.insert("caption".into(), Value::String(c.to_string()));
    }
    json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": "image",
        "image": Value::Object(image),
    })
}

#[derive(Clone)]
struct RawChunk {
    kind: RawKind,
    raw: String,
}

#[derive(Clone, Copy)]
enum RawKind {
    Text,
    Narration,
}

/// Walk `raw` char-by-char so a cut never lands mid-codepoint.
fn budget_raw(raw: &str, max_bytes: usize) -> &str {
    let mut total = 0usize;
    let mut end = 0usize;
    for (i, c) in raw.char_indices() {
        let n = c.len_utf8();
        if total + n > max_bytes {
            break;
        }
        total += n;
        end = i + n;
    }
    &raw[..end]
}

/// Build the WhatsApp `messages` request body for `to` + the
/// rendered text. Lives next to the renderer so the rendering +
/// serialisation stay paired.
pub fn build_messages_body(to: &str, msg: &RenderedMessage) -> Value {
    json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": "text",
        "text": {
            "body": msg.text,
            "preview_url": false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use triton_core::a2ui::{Component, Surface};

    #[test]
    fn passthrough_text_and_narration() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "hello".into(),
                },
                Component::Narration {
                    text: "a footnote".into(),
                },
            ],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.text, "hello\n\n_a footnote_");
        assert_eq!(r.deferred_buttons, 0);
        assert!(!r.truncated);
    }

    #[test]
    fn buttons_selection_form_are_deferred_dashboard_is_surfaced() {
        // PR 38 update: Dashboards are no longer deferred — the
        // first one is surfaced to the caller for rasterisation
        // (subsequent ones bump `deferred_dashboards`). The other
        // interactive primitives remain deferred until PR 32's
        // numbered_prompts wiring lands on WhatsApp.
        use serde_json::json;
        use triton_core::a2ui::{DashboardTile, FormField, FormFieldKind, SelectionOption};
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "preamble".into(),
                },
                Component::Button {
                    label: "Refresh".into(),
                    tool: "narrate".into(),
                    args: json!({}),
                },
                Component::Selection {
                    prompt: "pick".into(),
                    options: vec![SelectionOption {
                        label: "A".into(),
                        value: "a".into(),
                    }],
                    tool: "narrate".into(),
                    args_key: "s".into(),
                },
                Component::Form {
                    title: "form".into(),
                    fields: vec![FormField {
                        name: "name".into(),
                        label: "Name".into(),
                        kind: FormFieldKind::String,
                        required: false,
                    }],
                    submit_label: "Send".into(),
                    tool: "narrate".into(),
                },
                Component::Dashboard {
                    title: "Secrets".into(),
                    tiles: vec![DashboardTile {
                        label: "invocations".into(),
                        value: "1234".into(),
                        trend: None,
                    }],
                },
            ],
        };
        let r = render(&s).expect("renders");
        // None of the interactive components contribute to text.
        assert_eq!(r.text, "preamble");
        assert_eq!(r.deferred_buttons, 1);
        assert_eq!(r.deferred_selections, 1);
        assert_eq!(r.deferred_forms, 1);
        // PR 38: first dashboard surfaced, not deferred.
        assert_eq!(r.deferred_dashboards, 0);
        let dash = r.dashboard.as_ref().expect("dashboard surfaced");
        assert_eq!(dash.title, "Secrets");
        assert_eq!(dash.tiles[0].label, "invocations");
        // Dashboard tile content MUST NEVER leak into the body
        // (would silently violate the `rasterised_png` degrade rule).
        assert!(!r.text.contains("invocations"));
        assert!(!r.text.contains("1234"));
    }

    #[test]
    fn second_dashboard_is_deferred_first_is_surfaced() {
        // WhatsApp image messages carry one media attachment. With
        // two Dashboards in the same surface, the first becomes the
        // image and any others bump `deferred_dashboards`.
        use triton_core::a2ui::DashboardTile;
        let make_dash = |title: &str| Component::Dashboard {
            title: title.into(),
            tiles: vec![DashboardTile {
                label: "x".into(),
                value: "1".into(),
                trend: None,
            }],
        };
        let s = Surface {
            components: vec![make_dash("first"), make_dash("second")],
        };
        let r = render(&s).expect("renders");
        let dash = r.dashboard.expect("first surfaced");
        assert_eq!(dash.title, "first");
        assert_eq!(r.deferred_dashboards, 1);
    }

    #[test]
    fn dashboard_only_surface_renders_with_empty_caption() {
        // A surface that's nothing but a Dashboard is now valid:
        // WhatsApp's image message accepts no caption, so we ship
        // the photo with no text rather than failing
        // EmptyAfterRender.
        use triton_core::a2ui::DashboardTile;
        let s = Surface {
            components: vec![Component::Dashboard {
                title: "Solo".into(),
                tiles: vec![DashboardTile {
                    label: "x".into(),
                    value: "1".into(),
                    trend: None,
                }],
            }],
        };
        let r = render(&s).expect("renders");
        assert!(r.text.is_empty());
        assert!(r.dashboard.is_some());
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        let s = Surface { components: vec![] };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }

    #[test]
    fn button_only_surface_defers_to_empty_after_render() {
        // No text-bearing components → nothing to ship via WhatsApp's
        // text channel. PR 32 will rewire this through interactive
        // primitives; for now it's an explicit drop.
        use serde_json::json;
        let s = Surface {
            components: vec![Component::Button {
                label: "Click".into(),
                tool: "narrate".into(),
                args: json!({}),
            }],
        };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }

    #[test]
    fn oversized_text_is_truncated_below_cap() {
        let big = "x".repeat(10_000);
        let s = Surface {
            components: vec![Component::Text { value: big }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        assert!(r.text.len() <= WHATSAPP_TEXT_MAX_BYTES);
        assert!(r.text.ends_with(TRUNCATION_SENTINEL));
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        let fourbyte = "𝄞"; // U+1D11E, 4 bytes
        let s = Surface {
            components: vec![Component::Text {
                value: fourbyte.repeat(2000),
            }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        // The String type guarantees valid UTF-8; spot-check char
        // boundaries don't panic when iterated.
        for i in 0..=r.text.len() {
            let _ = r.text.is_char_boundary(i);
        }
    }

    #[test]
    fn narration_truncation_keeps_underscores_balanced() {
        let big = "n".repeat(10_000);
        let s = Surface {
            components: vec![Component::Narration { text: big }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        // The italic wrapper must remain matched: one opener + one
        // closer in the body. We tolerate raw text that itself
        // happens to contain `_` (it doesn't for an `n`-only fill).
        let opens = r.text.chars().filter(|c| *c == '_').count();
        assert_eq!(opens, 2, "expected balanced _..._ wrapper; got: {}", r.text);
    }

    #[test]
    fn build_messages_body_shape_matches_cloud_api() {
        let msg = RenderedMessage {
            text: "hello".into(),
            deferred_buttons: 0,
            deferred_selections: 0,
            deferred_forms: 0,
            deferred_dashboards: 0,
            truncated: false,
            dashboard: None,
        };
        let body = build_messages_body("491234567", &msg);
        assert_eq!(body["messaging_product"], "whatsapp");
        assert_eq!(body["recipient_type"], "individual");
        assert_eq!(body["to"], "491234567");
        assert_eq!(body["type"], "text");
        assert_eq!(body["text"]["body"], "hello");
        assert_eq!(body["text"]["preview_url"], false);
    }

    #[test]
    fn build_image_message_body_shape_matches_cloud_api() {
        let body = build_image_message_body("491234567", "media_id_stub", Some("a caption"));
        assert_eq!(body["messaging_product"], "whatsapp");
        assert_eq!(body["recipient_type"], "individual");
        assert_eq!(body["to"], "491234567");
        assert_eq!(body["type"], "image");
        assert_eq!(body["image"]["id"], "media_id_stub");
        assert_eq!(body["image"]["caption"], "a caption");
    }

    #[test]
    fn build_image_message_body_omits_caption_when_empty() {
        let body = build_image_message_body("491234567", "m1", None);
        assert!(body["image"].as_object().unwrap().get("caption").is_none());
    }
}
