//! In-process tools registered for the walking skeleton.
//!
//! * `echo` — round-trips its argument so PR 4 could prove the
//!   dispatcher + audit pipeline end-to-end.
//! * `narrate` — emits a small A2UI surface so PR 10 can exercise
//!   the v0.8/v0.9 builders + ACC-1 parity across the HTTP trio.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use triton_core::a2ui::{Component, Surface};
// These component types are only used by the `dev-token`-gated
// `demo_panel` tool below, so importing them unconditionally warns in a
// production (`--no-default-features`) build.
#[cfg(feature = "dev-token")]
use triton_core::a2ui::{DashboardTile, FormField, FormFieldKind, SelectionOption};
use triton_core::{Tool, ToolPrincipal, TritonError};

pub struct Echo;

#[derive(Debug, Deserialize)]
struct EchoArgs {
    message: String,
}

#[derive(Debug, Serialize)]
struct EchoOut {
    echo: String,
}

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["message"],
            "properties": { "message": { "type": "string" } },
            "additionalProperties": false
        })
    }

    async fn invoke(&self, args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let parsed: EchoArgs = serde_json::from_value(args)
            .map_err(|e| TritonError::Validation(format!("echo args: {e}")))?;
        let out = EchoOut {
            echo: parsed.message,
        };
        Ok(serde_json::to_value(out).expect("EchoOut serialises"))
    }
}

/// Synchronously sleep for the requested number of milliseconds.
/// Used by ACC-2's mid-flight-SIGTERM test. **Dev-only**: gated by
/// the `dev-token` cargo feature so production builds
/// (`--no-default-features`) don't ship a tool that lets any
/// authenticated caller park a request task for `u64::MAX` ms.
#[cfg(feature = "dev-token")]
pub struct Delay;

/// Cap on `delay.ms` even in dev. The test uses 1500 ms; raising
/// the cap doesn't help anyone and a runaway value (`u64::MAX`)
/// would tie up a tokio task essentially forever.
#[cfg(feature = "dev-token")]
const DELAY_MAX_MS: u64 = 5_000;

#[cfg(feature = "dev-token")]
#[derive(Debug, Deserialize)]
struct DelayArgs {
    ms: u64,
}

#[cfg(feature = "dev-token")]
#[async_trait]
impl Tool for Delay {
    fn name(&self) -> &'static str {
        "delay"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["ms"],
            "properties": {
                "ms": { "type": "integer", "minimum": 0, "maximum": DELAY_MAX_MS }
            },
            "additionalProperties": false
        })
    }

    async fn invoke(&self, args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let parsed: DelayArgs = serde_json::from_value(args)
            .map_err(|e| TritonError::Validation(format!("delay args: {e}")))?;
        if parsed.ms > DELAY_MAX_MS {
            return Err(TritonError::Validation(format!(
                "delay.ms must be <= {DELAY_MAX_MS}"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(parsed.ms)).await;
        Ok(serde_json::json!({ "delayed_ms": parsed.ms }))
    }
}

pub struct Narrate;

#[derive(Debug, Deserialize)]
struct NarrateArgs {
    subject: String,
}

#[async_trait]
impl Tool for Narrate {
    fn name(&self) -> &'static str {
        "narrate"
    }

    fn returns_a2ui(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["subject"],
            "properties": { "subject": { "type": "string" } },
            "additionalProperties": false
        })
    }

    async fn invoke(&self, args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let parsed: NarrateArgs = serde_json::from_value(args)
            .map_err(|e| TritonError::Validation(format!("narrate args: {e}")))?;
        // The tool builds a canonical, version-agnostic Surface.
        // The adapter wraps it with the negotiated builder
        // (ADR-4: dispatcher and tools never branch on version).
        let surface = Surface {
            components: vec![
                Component::Text {
                    value: format!("Hello, {}.", parsed.subject),
                },
                Component::Narration {
                    text: format!("This is a generated narration about {}.", parsed.subject),
                },
                Component::Button {
                    label: "Refresh".into(),
                    tool: "narrate".into(),
                    args: serde_json::json!({ "subject": parsed.subject }),
                },
            ],
        };
        Ok(serde_json::json!({
            "surface": surface,
        }))
    }
}

/// `demo_panel` — emits an A2UI Surface that uses every component
/// variant (Text, Narration, Button, Selection, Form, Dashboard) so
/// the explorer's A2UI diff page can render the full v0.8 vs v0.9
/// vocabulary side-by-side. Pure stub data, no upstream call.
///
/// Codex PR 26 review concern: gated on `dev-token` so production
/// builds don't ship a reference/demo tool that users can poke.
/// The explorer is itself a dev/internal surface; promoting the
/// demo to prod would be a separate ADR.
#[cfg(feature = "dev-token")]
pub struct DemoPanel;

#[cfg(feature = "dev-token")]
#[async_trait]
impl Tool for DemoPanel {
    fn name(&self) -> &'static str {
        "demo_panel"
    }

    fn returns_a2ui(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        })
    }

    async fn invoke(&self, _args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let surface = Surface {
            components: vec![
                Component::Text {
                    value: "Triton demo panel".into(),
                },
                Component::Narration {
                    text: "Every A2UI component, rendered through both v0.8 and v0.9 builders."
                        .into(),
                },
                Component::Dashboard {
                    title: "Last hour".into(),
                    tiles: vec![
                        DashboardTile {
                            label: "invocations".into(),
                            value: "1,284".into(),
                            trend: Some("+12% vs prior".into()),
                        },
                        DashboardTile {
                            label: "p95 latency".into(),
                            value: "84 ms".into(),
                            trend: None,
                        },
                        DashboardTile {
                            label: "errors".into(),
                            value: "3".into(),
                            trend: Some("-2 vs prior".into()),
                        },
                    ],
                },
                Component::Selection {
                    prompt: "Pick a sample tone".into(),
                    options: vec![
                        SelectionOption {
                            label: "Friendly".into(),
                            value: "friendly".into(),
                        },
                        SelectionOption {
                            label: "Formal".into(),
                            value: "formal".into(),
                        },
                        SelectionOption {
                            label: "Terse".into(),
                            value: "terse".into(),
                        },
                    ],
                    tool: "narrate".into(),
                    args_key: "subject".into(),
                },
                Component::Form {
                    title: "Customer feedback".into(),
                    fields: vec![
                        FormField {
                            name: "name".into(),
                            label: "Your name".into(),
                            kind: FormFieldKind::String,
                            required: true,
                        },
                        FormField {
                            name: "rating".into(),
                            label: "Rating (1-5)".into(),
                            kind: FormFieldKind::Integer,
                            required: true,
                        },
                        FormField {
                            name: "contact_ok".into(),
                            label: "OK to follow up?".into(),
                            kind: FormFieldKind::Boolean,
                            required: false,
                        },
                    ],
                    submit_label: "Send feedback".into(),
                    tool: "echo".into(),
                },
                Component::Button {
                    label: "Refresh".into(),
                    tool: "demo_panel".into(),
                    args: serde_json::json!({}),
                },
            ],
        };
        Ok(serde_json::json!({ "surface": surface }))
    }
}

/// Dev-only A2UI tool emitting an *empty* Surface so the PR 20
/// integration test can drive the L6'-edge empty-surface fallback
/// end-to-end. Without this, the only path to an empty Surface
/// would be a misbehaving production tool — which the
/// `dev-token`-gated registry entry below makes impossible to
/// register in a production build (ADR-10 / FR-I-5 parallel).
/// Dev-only A2UI tool emitting a Surface that contains **only** a
/// `Component::Form`. PR 30's Discord adapter routes form-only
/// Surfaces through the modal-opener path (interaction response
/// type 9), so the integration test exercises:
///   * slash command / button → form-only surface → type=9 modal
///     opener with a correlation token in `custom_id`,
///   * modal submit (type 5) → token decode → arg substitution →
///     dispatch the form's submit tool (`echo`) → type=4 reply.
///
/// The single field uses `kind: String` because Discord modals
/// only support TEXT_INPUT; PR 30 doesn't attempt Integer/Boolean
/// coercion yet.
#[cfg(feature = "dev-token")]
pub struct FormOnlyDemo;

#[cfg(feature = "dev-token")]
#[async_trait]
impl Tool for FormOnlyDemo {
    fn name(&self) -> &'static str {
        "form_only_demo"
    }

    fn returns_a2ui(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false
        })
    }

    async fn invoke(&self, _args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        // One-field form so the submit dispatches `echo(message)`
        // against echo's existing input schema (one required
        // `message` string, `additionalProperties: false`).
        let surface = Surface {
            components: vec![Component::Form {
                title: "Quick feedback".into(),
                fields: vec![FormField {
                    name: "message".into(),
                    label: "Your message".into(),
                    kind: FormFieldKind::String,
                    required: true,
                }],
                submit_label: "Send".into(),
                tool: "echo".into(),
            }],
        };
        Ok(serde_json::json!({ "surface": surface }))
    }
}

/// Dev-only A2UI tool emitting a multi-field, form-only Surface.
/// Used by PR 32's Telegram integration test to exercise the
/// numbered-prompts state machine:
///   * `name` — String (required), checks the happy passthrough,
///   * `age` — Integer (required), exercises the parse-error +
///     re-prompt path,
///   * `subscribe` — Boolean (optional), exercises the
///     yes/no/true/false/1/0 coercion table plus the
///     optional-blank-stores-null branch.
///
/// The submit tool is `echo`, so the form's completion lands on
/// the existing echo tool's `{ message: <string> }` schema — but
/// the form args don't match that schema (we send a three-key
/// object). On completion echo will Validation-fail, which is
/// also fine for the test: the test inspects the `phase: dispatch`
/// audit line for the submit tool, not echo's reply content.
///
/// Same `dev-token` gate as the other reference/demo tools so
/// production builds don't reserve the route.
#[cfg(feature = "dev-token")]
pub struct FormOnlyDemoMulti;

#[cfg(feature = "dev-token")]
#[async_trait]
impl Tool for FormOnlyDemoMulti {
    fn name(&self) -> &'static str {
        "form_only_demo_multi"
    }

    fn returns_a2ui(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false
        })
    }

    async fn invoke(&self, _args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let surface = Surface {
            components: vec![Component::Form {
                title: "Quick feedback (multi)".into(),
                fields: vec![
                    FormField {
                        name: "name".into(),
                        label: "name".into(),
                        kind: FormFieldKind::String,
                        required: true,
                    },
                    FormField {
                        name: "age".into(),
                        label: "age".into(),
                        kind: FormFieldKind::Integer,
                        required: true,
                    },
                    FormField {
                        name: "subscribe".into(),
                        label: "subscribe".into(),
                        kind: FormFieldKind::Boolean,
                        required: false,
                    },
                ],
                submit_label: "Send".into(),
                // `submitted_form` is a dev tool that echoes back
                // its full args as JSON; using it instead of `echo`
                // lets the integration test assert the submit
                // dispatched with the user-supplied values without
                // tripping `echo`'s schema (which insists on a
                // single `message` string field, additional-
                // properties: false).
                tool: "submitted_form".into(),
            }],
        };
        Ok(serde_json::json!({ "surface": surface }))
    }
}

/// Dev-only tool that echoes its entire args map back as a
/// single-key result. Used as the submit tool for
/// [`FormOnlyDemoMulti`] so the integration test can assert the
/// form's collected values reached the dispatcher without having
/// to massage the args through `echo`'s strict
/// `additionalProperties: false` schema.
#[cfg(feature = "dev-token")]
pub struct SubmittedForm;

#[cfg(feature = "dev-token")]
#[async_trait]
impl Tool for SubmittedForm {
    fn name(&self) -> &'static str {
        "submitted_form"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    }

    async fn invoke(&self, args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        // Serialise the args so the integration test can scan the
        // post-back text for individual field values.
        let s = serde_json::to_string(&args)
            .map_err(|e| TritonError::Validation(format!("submitted_form serialise: {e}")))?;
        Ok(serde_json::json!({ "submitted": s }))
    }
}

#[cfg(feature = "dev-token")]
pub struct EmptySurface;

#[cfg(feature = "dev-token")]
#[async_trait]
impl Tool for EmptySurface {
    fn name(&self) -> &'static str {
        "empty_surface"
    }

    fn returns_a2ui(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false
        })
    }

    async fn invoke(&self, _args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let surface = Surface { components: vec![] };
        Ok(serde_json::json!({ "surface": surface }))
    }
}
