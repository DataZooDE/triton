//! A2UI v0.9 envelope builder. **No shared base** with v0.8 per
//! ADR-4. v0.9 flattens each stream entry — no `Component`
//! wrapper, lowercase `type` field, action data inlined:
//!
//! ```json
//! {
//!   "version": "0.9",
//!   "stream": [
//!     { "type": "text", "text": "Hello" },
//!     { "type": "button", "label": "Click", "action": {...} }
//!   ]
//! }
//! ```

use serde_json::{Value, json};

use super::{Component, FormField, FormFieldKind, Surface};

pub fn build(surface: &Surface) -> Value {
    let stream: Vec<Value> = surface.components.iter().map(component_to_json).collect();
    json!({
        "version": "0.9",
        "stream": stream
    })
}

fn component_to_json(c: &Component) -> Value {
    match c {
        Component::Text { value, pills } => {
            let mut t = json!({ "type": "text", "text": value });
            if !pills.is_empty() {
                t["pills"] = json!(pills);
            }
            t
        }
        Component::Narration { text } => json!({ "type": "narration", "text": text }),
        Component::Button {
            label,
            tool,
            args,
            resource,
            primary,
        } => {
            let mut b = json!({
                "type": "button",
                "label": label,
                "action": { "tool": tool, "args": args }
            });
            if let Some(r) = resource {
                b["resource"] = json!(r);
            }
            if *primary {
                b["primary"] = json!(true);
            }
            b
        }
        Component::Selection {
            prompt,
            options,
            tool,
            args_key,
        } => json!({
            "type": "selection",
            "prompt": prompt,
            "options": options.iter().map(|o| json!({
                "label": o.label,
                "value": o.value,
            })).collect::<Vec<_>>(),
            "tool": tool,
            "args_key": args_key,
        }),
        Component::Form {
            title,
            fields,
            submit_label,
            tool,
        } => json!({
            "type": "form",
            "title": title,
            "fields": fields.iter().map(form_field_to_json).collect::<Vec<_>>(),
            "submit_label": submit_label,
            "tool": tool,
        }),
        Component::Dashboard { title, tiles } => json!({
            "type": "dashboard",
            "title": title,
            "tiles": tiles.iter().map(|t| {
                let mut o = json!({ "label": t.label, "value": t.value });
                if let Some(trend) = &t.trend {
                    o["trend"] = json!(trend);
                }
                o
            }).collect::<Vec<_>>(),
        }),
        Component::Report {
            report_id,
            args,
            title,
            series,
            labels,
        } => {
            let mut r = json!({
                "type": "report",
                "report_id": report_id,
                "args": args,
            });
            if let Some(t) = title {
                r["title"] = json!(t);
            }
            if !series.is_empty() {
                r["series"] = json!(series);
            }
            if !labels.is_empty() {
                r["labels"] = json!(labels);
            }
            r
        }
        Component::Diff { lines } => json!({
            "type": "diff",
            "lines": lines,
        }),
        Component::Sources { items } => json!({
            "type": "sources",
            "items": items.iter().map(|i| json!({
                "label": i.label,
                "resource": i.resource,
            })).collect::<Vec<_>>(),
        }),
    }
}

/// `placeholder` and `default` ride only when the agent set them, so a form
/// composed before they existed is byte-identical on the wire.
fn form_field_to_json(f: &FormField) -> Value {
    let mut o = json!({
        "name": f.name,
        "label": f.label,
        "kind": form_kind_str(f.kind),
        "required": f.required,
    });
    if let Some(p) = &f.placeholder {
        o["placeholder"] = json!(p);
    }
    if let Some(d) = &f.default_value {
        o["default"] = d.clone();
    }
    o
}

fn form_kind_str(k: FormFieldKind) -> &'static str {
    match k {
        FormFieldKind::String => "string",
        FormFieldKind::Integer => "integer",
        FormFieldKind::Boolean => "boolean",
    }
}

/// The field groups a phone-shaped host needs and v0.9 had no slot for.
///
/// Every one of these is **omitted when unset**, so an agent that has never
/// heard of them emits byte-for-byte the envelope it emitted before. That is
/// the property that makes this extension safe to land while v0.9 is already
/// negotiated on the wire, and each test below asserts it as its own positive
/// control rather than trusting the `skip_serializing_if`.
#[cfg(test)]
mod heron_vocabulary_tests {
    use super::super::*;
    use serde_json::json;

    fn stream(components: Vec<Component>) -> Vec<Value> {
        super::build(&Surface { components })["stream"]
            .as_array()
            .expect("stream")
            .clone()
    }

    /// `pills` resolves the `[[skill::id]]` wikilinks inside a text node to
    /// display names. It is a map keyed by **id** because the renderer's
    /// degrade is "no entry → show the bare id": a consultant who may not read
    /// an instance (D27) simply gets no key, and absence is the whole
    /// mechanism. A list of {id,label} pairs would make absence a search.
    #[test]
    fn text_carries_pill_labels_for_its_wikilinks() {
        let s = stream(vec![
            Component::Text {
                value: "Filing on [[customer::hoffmann]] today".into(),
                pills: [("hoffmann".to_string(), "Hoffmann Automotive".to_string())]
                    .into_iter()
                    .collect(),
            },
            Component::Text {
                value: "See [[customer::secret]]".into(),
                pills: Default::default(),
            },
        ]);
        assert_eq!(s[0]["type"], "text");
        assert_eq!(s[0]["text"], "Filing on [[customer::hoffmann]] today");
        assert_eq!(s[0]["pills"]["hoffmann"], "Hoffmann Automotive");
        // Positive control: the un-pilled node still renders its text, and it
        // carries NO `pills` key — not an empty object — so a host cannot tell
        // this envelope from a pre-extension one.
        assert_eq!(s[1]["text"], "See [[customer::secret]]");
        assert!(s[1].get("pills").is_none(), "{}", s[1]);
    }

    /// `primary` marks the one action the surface is actually asking for.
    /// A bool, not a style enum: the design distinguishes exactly one primary
    /// action per surface, and an open style field would let an agent paint.
    #[test]
    fn button_marks_the_primary_action_and_only_when_set() {
        let s = stream(vec![
            Component::Button {
                label: "Approve".into(),
                tool: "heron_approve".into(),
                args: json!({ "id": "p1" }),
                resource: None,
                primary: true,
            },
            Component::Button {
                label: "Reject".into(),
                tool: "heron_reject".into(),
                args: json!({}),
                resource: None,
                primary: false,
            },
        ]);
        assert_eq!(s[0]["primary"], true);
        assert_eq!(s[0]["action"]["tool"], "heron_approve");
        // Positive control: the secondary button is still a fully-formed
        // button, it just has no `primary` key.
        assert_eq!(s[1]["label"], "Reject");
        assert_eq!(s[1]["action"]["tool"], "heron_reject");
        assert!(s[1].get("primary").is_none(), "{}", s[1]);
    }

    /// `placeholder` is hint text; `default` is a value that must survive an
    /// untouched submit. They are separate because they answer different
    /// questions — a placeholder that submitted itself would put words the
    /// agent never proposed into a record a consultant signed off.
    #[test]
    fn form_fields_carry_placeholder_and_default_independently() {
        let s = stream(vec![Component::Form {
            title: "Run skill".into(),
            submit_label: "Run".into(),
            tool: "heron_run".into(),
            fields: vec![
                FormField {
                    name: "window".into(),
                    label: "Window".into(),
                    kind: FormFieldKind::String,
                    required: false,
                    placeholder: Some("e.g. 30d".into()),
                    default_value: Some(json!("30d")),
                },
                FormField {
                    name: "dry".into(),
                    label: "Dry run".into(),
                    kind: FormFieldKind::Boolean,
                    required: true,
                    placeholder: None,
                    default_value: None,
                },
            ],
        }]);
        let fields = s[0]["fields"].as_array().expect("fields");
        assert_eq!(fields[0]["placeholder"], "e.g. 30d");
        assert_eq!(fields[0]["default"], "30d");
        // Positive control: the bare field is still a complete field — and
        // carries neither key, so `default: null` can never be mistaken for
        // "the agent proposed null".
        assert_eq!(fields[1]["name"], "dry");
        assert_eq!(fields[1]["kind"], "boolean");
        assert_eq!(fields[1]["required"], true);
        assert!(fields[1].get("placeholder").is_none(), "{}", fields[1]);
        assert!(fields[1].get("default").is_none(), "{}", fields[1]);
    }

    /// The `diff` component: BR-HIL-1's readable diff of the page an approval
    /// will write. Lines are tagged by `op` so a host switches on one key.
    #[test]
    fn diff_lines_are_op_tagged_and_a_fold_carries_its_hidden_lines() {
        let s = stream(vec![Component::Diff {
            lines: vec![
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
            ],
        }]);
        assert_eq!(s[0]["type"], "diff");
        let lines = s[0]["lines"].as_array().expect("lines");
        assert_eq!(lines[0]["op"], "ctx");
        assert_eq!(lines[1]["op"], "fold");
        assert_eq!(lines[1]["count"], 42);
        assert_eq!(lines[1]["hidden"][0]["text"], "buried line");
        // Positive control: the change itself is NOT inside the fold — it sits
        // at top level, visible without expanding anything.
        assert_eq!(lines[2]["op"], "del");
        assert_eq!(lines[3]["op"], "add");
        assert_eq!(lines[3]["text"], "status: active");
        // …and a fold carries no `text` of its own to be mistaken for content.
        assert!(lines[1].get("text").is_none(), "{}", lines[1]);
    }

    /// A `report` names the report to dispatch (`report_id` + `args`) AND may
    /// inline the preview a host that cannot dispatch should draw instead.
    /// Both, not either: the same envelope has to work in the Explorer (which
    /// calls `render_report`) and on a phone in a chat channel (which cannot),
    /// and that is the whole degradation contract.
    #[test]
    fn report_may_inline_a_preview_beside_the_id_it_dispatches() {
        let s = stream(vec![
            Component::Report {
                report_id: "renewals".into(),
                args: json!({ "window": "90d" }),
                title: Some("Renewals".into()),
                series: vec![4.0, 8.0, 6.0],
                labels: vec!["Q1".into(), "Q2".into(), "Q3".into()],
            },
            Component::Report {
                report_id: "plain".into(),
                args: json!({}),
                title: None,
                series: vec![],
                labels: vec![],
            },
        ]);
        assert_eq!(s[0]["report_id"], "renewals");
        assert_eq!(s[0]["title"], "Renewals");
        assert_eq!(s[0]["series"][1], 8.0);
        assert_eq!(s[0]["labels"][1], "Q2");
        // Positive control: a dispatch-only report is byte-identical to what
        // v0.9 emitted before this extension.
        assert_eq!(
            s[1],
            json!({ "type": "report", "report_id": "plain", "args": {} })
        );
    }
}
