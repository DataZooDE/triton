//! Every mock item renders from a fixture surface, on every channel.
//!
//! The Definition of Done for "the conversation mock is implementable": each
//! item of the mock that is a card (1a–1f, 1h–1i, 1l–1p,
//! 2a–2b, plus the 2d embed) is a fixture surface under
//! `fixtures/conversation/surfaces/`, and this test renders it three ways:
//!
//! * **Gemini Enterprise** — the A2UI v0.9 message array (`ge.rs`), each
//!   message validated against the official a2ui.org v0.9 server-to-client
//!   schema with the Gemini Enterprise composite catalog Triton declares;
//! * **Microsoft Teams** — the Adaptive Card the webhook sends
//!   (`conversation_card::card_for_result`), validated against the official
//!   Adaptive Cards 1.5 schema;
//! * **plain text** — the no-multi-turn degradation (`a2ui::text`).
//!
//! and pins all three as golden files under `fixtures/conversation/golden/`.
//! Regenerate with `UPDATE_CONVERSATION_GOLDEN=1 cargo test -p triton-tests
//! --test it conversation_components` and review the diff.
//!
//! Golden normalisation: GE component ids carry an HMAC token with an expiry
//! (time-dependent), so each `a<N>-<token>` id is verified to decode to its
//! message and then pinned as `a<N>` plus an `_actions` map {id → message}.
//! The Teams signer here is a deterministic stand-in (`<tool>|<args>`); the
//! real HMAC path is exercised against the running binary in
//! `conversation_teams_e2e.rs`.

use std::path::PathBuf;

use serde_json::{Value, json};
use triton_core::a2ui::ge::{ActionSigner, GeOptions, build_messages_with};
use triton_core::a2ui::{extract_surface, text::surface_to_text};

fn dir(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(sub)
}

fn read_json(p: &PathBuf) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{p:?}: {e}")))
        .unwrap_or_else(|e| panic!("{p:?}: {e}"))
}

/// Every fixture surface, sorted by mock item id.
fn fixtures() -> Vec<(String, Value)> {
    let mut v: Vec<(String, Value)> = std::fs::read_dir(dir("fixtures/conversation/surfaces"))
        .expect("fixture dir")
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "json").then(|| {
                let name = p.file_stem().unwrap().to_string_lossy().to_string();
                (name, read_json(&p))
            })
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

const A2UI_BASE: &str = "https://a2ui.org/specification/v0_9/";

/// A compiled JSON-schema validator (the `jsonschema` crate, as for the A2A
/// schemas): offline, every `$ref` resolved from the vendored files.
pub(crate) struct SchemaCheck(jsonschema::Validator);

impl SchemaCheck {
    /// `Ok`, or the first errors as one readable string.
    pub(crate) fn validate(&self, value: &Value) -> Result<(), String> {
        let errs: Vec<String> = self
            .0
            .iter_errors(value)
            .take(5)
            .map(|e| format!("{e} at {}", e.instance_path()))
            .collect();
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("; "))
        }
    }
}

/// Resolves the vendored schemas by URI; nothing is fetched.
struct Vendored(std::collections::HashMap<String, Value>);

impl jsonschema::Retrieve for Vendored {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let key = uri.as_str().trim_end_matches('#');
        self.0
            .get(key)
            .cloned()
            .ok_or_else(|| format!("schema {key} is not vendored").into())
    }
}

/// A compiled validator for one A2UI v0.9 server→client message, with the
/// GE composite catalog registered as the active `catalog.json`.
pub(crate) fn a2ui_validator() -> SchemaCheck {
    let s = |f: &str| read_json(&dir(&format!("schemas/a2ui-v0_9/{f}")));
    let mut catalog = s("gemini_enterprise_composite_catalog.json");
    catalog["$id"] = json!(format!("{A2UI_BASE}catalog.json"));
    let root = s("server_to_client.json");
    let resources = std::collections::HashMap::from([
        (format!("{A2UI_BASE}server_to_client.json"), root.clone()),
        (
            format!("{A2UI_BASE}common_types.json"),
            s("common_types.json"),
        ),
        (format!("{A2UI_BASE}catalog.json"), catalog),
    ]);
    let v = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .with_base_uri(format!("{A2UI_BASE}server_to_client.json"))
        .with_retriever(Vendored(resources))
        .build(&root)
        .unwrap_or_else(|e| panic!("compile a2ui schema: {e}"));
    SchemaCheck(v)
}

pub(crate) fn adaptive_card_validator() -> SchemaCheck {
    let url = "http://adaptivecards.io/schemas/adaptive-card.json";
    let schema = read_json(&dir("schemas/adaptive-cards/adaptive-card-1.5.0.json"));
    let v = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft6)
        .with_base_uri(url.to_string())
        .with_retriever(Vendored(std::collections::HashMap::from([(
            url.to_string(),
            schema.clone(),
        )])))
        .build(&schema)
        .unwrap_or_else(|e| panic!("compile adaptive card schema: {e}"));
    SchemaCheck(v)
}

fn signer() -> ActionSigner {
    ActionSigner::new(
        triton_correlation::KeyRing::single(b"conversation-golden-test-key".to_vec()).unwrap(),
    )
}

/// Replace time-dependent bits of a GE message array: the surface id, and
/// each signed `a<N>-<token>` id (verified to decode first).
fn normalise_ge(messages: &[Value], signer: &ActionSigner) -> Value {
    let mut actions = serde_json::Map::new();
    let mut text = serde_json::to_string(messages).unwrap();
    for m in messages {
        if let Some(sid) = m
            .pointer("/createSurface/surfaceId")
            .and_then(Value::as_str)
        {
            text = text.replace(sid, "SURFACE");
        }
        let comps = m
            .pointer("/updateComponents/components")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for c in comps {
            let id = c["id"].as_str().unwrap_or_default().to_string();
            if let Some((short, _)) = id.split_once('-').filter(|(p, _)| {
                p.starts_with('a') && p[1..].bytes().all(|b| b.is_ascii_digit()) && p.len() > 1
            }) {
                // A form submit (signed template) decodes to the inputs it
                // still needs; anything else to its complete message.
                let msg = match signer
                    .open_action(&id)
                    .unwrap_or_else(|| panic!("action id {id} must decode"))
                {
                    triton_core::a2ui::ge::GeAction::Message(m) => m,
                    triton_core::a2ui::ge::GeAction::Incomplete(names) => {
                        format!("form submit, fills: {}", names.join(", "))
                    }
                    other => panic!("unexpected action {other:?}"),
                };
                actions.insert(short.to_string(), json!(msg));
                text = text.replace(&format!("\"{id}\""), &format!("\"{short}\""));
            }
        }
    }
    json!({ "messages": serde_json::from_str::<Value>(&text).unwrap(), "_actions": actions })
}

fn teams_card(surface: &Value) -> Option<Value> {
    let mut sign = |tool: &str, args: &Value| Some(format!("{tool}|{args}"));
    triton_chat_msteams::conversation_card::card_for_result(
        surface,
        None,
        None,
        &Default::default(),
        &mut sign,
    )
}

/// Compare against the golden file, or (re)write it when asked.
fn pin(name: &str, produced: &str, failures: &mut Vec<String>) {
    let path = dir("fixtures/conversation/golden").join(name);
    if std::env::var("UPDATE_CONVERSATION_GOLDEN").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, produced).unwrap();
        return;
    }
    match std::fs::read_to_string(&path) {
        Ok(want) if want == produced => {}
        Ok(_) => failures.push(format!("{name}: differs from golden")),
        Err(_) => failures.push(format!("{name}: no golden file")),
    }
}

fn pretty(v: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(v).unwrap())
}

#[test]
fn every_mock_item_renders_as_valid_a2ui_v09_adaptive_card_and_text() {
    let fixtures = fixtures();
    let expected = [
        "1a",
        "1b",
        "1c",
        "1d",
        "1e",
        "1f",
        "1h",
        "1i",
        "1l",
        "1l-done",
        "1m",
        "1n",
        "1o",
        "1p",
        "2a",
        "2a-map",
        "2b",
        "2d",
        // Turn 3: the redesigned renderings (one anatomy).
        "3a",
        "3b",
        "3c",
        "3d",
        "3d-list",
        "3e",
        "3f",
        "3g",
        // Extra cases beyond the mock items (prefixed `x-`).
        "x-embed-text",
    ];
    let names: Vec<&str> = fixtures.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names, expected,
        "one fixture per card mock item, plus the x- cases"
    );

    let a2ui = a2ui_validator();
    let ac = adaptive_card_validator();
    let signer = signer();
    let mut failures: Vec<String> = Vec::new();

    let mut variants: Vec<(String, Value, GeOptions)> = fixtures
        .iter()
        .map(|(n, v)| (format!("{n}.ge.json"), v.clone(), GeOptions::default()))
        .collect();
    // The opt-in GE renderings (in-card openUrl, Canvas embed) must be
    // schema-valid too, so they are pinned beside the defaults.
    let on = GeOptions {
        open_url: true,
        embed: true,
        ..GeOptions::default()
    };
    for (n, v) in &fixtures {
        if ["1a", "2b", "2d", "x-embed-text"].contains(&n.as_str()) {
            variants.push((format!("{n}.ge-optin.json"), v.clone(), on));
        }
    }
    // The fallback switch for forms (one-click buttons instead of fields).
    let chips = GeOptions {
        form_fields: false,
        ..GeOptions::default()
    };
    for (n, v) in &fixtures {
        if ["1e", "1m", "1o", "3c", "3d"].contains(&n.as_str()) {
            variants.push((format!("{n}.ge-chips.json"), v.clone(), chips));
        }
    }

    for (golden, surface, opts) in &variants {
        // ── Gemini Enterprise: A2UI v0.9 ───────────────────────────────
        let msgs = build_messages_with(surface, &signer, *opts)
            .unwrap_or_else(|| panic!("{golden}: GE renders nothing"));
        for m in &msgs {
            if let Err(e) = a2ui.validate(m) {
                failures.push(format!("{golden}: A2UI v0.9 schema: {e:#}\n{m}"));
            }
        }
        // Every flat component id is unique, every child reference resolves.
        for m in &msgs {
            let Some(comps) = m
                .pointer("/updateComponents/components")
                .and_then(Value::as_array)
            else {
                continue;
            };
            let ids: Vec<&str> = comps.iter().filter_map(|c| c["id"].as_str()).collect();
            let mut uniq = ids.clone();
            uniq.sort();
            uniq.dedup();
            if uniq.len() != ids.len() {
                failures.push(format!("{golden}: duplicate component ids"));
            }
            for c in comps {
                for child in c["children"].as_array().into_iter().flatten() {
                    if !ids.contains(&child.as_str().unwrap_or_default()) {
                        failures.push(format!("{golden}: dangling child {child}"));
                    }
                }
            }
            if !ids.contains(&"root") {
                failures.push(format!("{golden}: no root component"));
            }
        }
        pin(
            golden,
            &pretty(&normalise_ge(&msgs, &signer)),
            &mut failures,
        );
    }

    for (name, surface) in &fixtures {
        // ── Microsoft Teams: Adaptive Card ─────────────────────────────
        let card = teams_card(surface).unwrap_or_else(|| panic!("{name}: Teams renders no card"));
        let mut checked = card.clone();
        // The OSS schema does not know the Teams host extension (README).
        checked.as_object_mut().unwrap().remove("msteams");
        if let Err(e) = ac.validate(&checked) {
            failures.push(format!("{name}: Adaptive Card schema: {e:#}"));
        }
        pin(&format!("{name}.teams.json"), &pretty(&card), &mut failures);

        // ── Plain text ─────────────────────────────────────────────────
        let s = extract_surface(surface).expect("fixture is a canonical surface");
        let text = surface_to_text(&s);
        assert!(!text.trim().is_empty(), "{name}: empty text degradation");
        pin(&format!("{name}.txt"), &format!("{text}\n"), &mut failures);
    }

    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Positive control for the validators: they must REJECT what GE and Teams
/// reject, or a green golden run proves nothing.
#[test]
fn the_schema_validators_reject_invalid_output() {
    let a2ui = a2ui_validator();
    let msg = |comp: Value| json!({ "version": "v0.9", "updateComponents": { "surfaceId": "s", "components": [comp] } });
    let good = msg(json!({ "id": "root", "component": "MaterialText", "text": "hi" }));
    assert!(a2ui.validate(&good).is_ok());
    // Unknown component.
    let bad = msg(json!({ "id": "root", "component": "NoSuchWidget", "text": "hi" }));
    assert!(a2ui.validate(&bad).is_err());
    // The malformed openUrl GE rejected live (#263): `name` instead of `call`.
    let bad = msg(
        json!({ "id": "root", "component": "MaterialButton", "label": "x",
        "action": { "functionCall": { "name": "openUrl", "args": { "url": "https://x" } } } }),
    );
    assert!(a2ui.validate(&bad).is_err());
    // The spec-correct openUrl is accepted.
    let ok = msg(
        json!({ "id": "root", "component": "MaterialButton", "label": "x",
        "action": { "functionCall": { "call": "openUrl", "args": { "url": "https://x.example/a" },
                                      "returnType": "void" } } }),
    );
    assert!(
        a2ui.validate(&ok).is_ok(),
        "{:#}",
        a2ui.validate(&ok).unwrap_err()
    );

    let ac = adaptive_card_validator();
    let card = |body: Value| json!({ "type": "AdaptiveCard", "version": "1.4", "body": body });
    assert!(
        ac.validate(&card(json!([{ "type": "TextBlock", "text": "x" }])))
            .is_ok()
    );
    assert!(
        ac.validate(&card(json!([{ "type": "NoSuchElement" }])))
            .is_err()
    );
    assert!(
        ac.validate(&card(
            json!([{ "type": "Container", "style": "purple", "items": [] }])
        ))
        .is_err()
    );
}

/// The canonical JSON round-trips through the typed model unchanged: what
/// a portfolio agent emits is exactly what Triton reads.
#[test]
fn fixture_surfaces_round_trip_through_the_canonical_model() {
    for (name, v) in fixtures() {
        let s = extract_surface(&v).unwrap_or_else(|e| panic!("{name}: {e}"));
        let back = serde_json::to_value(&s).unwrap();
        assert_eq!(back, v["surface"], "{name}: canonical JSON must round-trip");
    }
}

/// The product rule on text channels: no multi-turn. Every one-click request
/// in the surface appears as its COMPLETE message on a copy line, and no
/// deep-link id leaks except inside its URL.
#[test]
fn text_degradation_shows_complete_requests_never_bare_numbers() {
    let v = read_json(&dir("fixtures/conversation/surfaces/1d.json"));
    let text = surface_to_text(&extract_surface(&v).unwrap());
    for line in [
        "\u{203a} Retire the agent \"Agent 2\" (Requirements: Forecast)",
        "\u{203a} Retire the agent \"Agent 20\" (Design: Monitor)",
        "\u{203a} Retire the agent \"Agent 28\" (Sales: Forecast)",
        "Replying \"Agent 20\" alone won't work: each message is read on its own.",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    let v = read_json(&dir("fixtures/conversation/surfaces/1h.json"));
    let text = surface_to_text(&extract_surface(&v).unwrap());
    assert!(
        text.contains("Lifecycle: Deployed \u{2192} Retired"),
        "{text}"
    );
    // The draft id appears only inside the review link.
    for line in text.lines().filter(|l| l.contains("01M636Z8XN")) {
        assert!(line.contains("https://"), "draft id outside a link: {line}");
    }
}

/// F5 (crew review): a fixed-message action must not gather or validate the
/// card's inputs — else a required template input blocks every chip on the
/// same card, and sibling inputs ride along. Template submits keep them.
#[test]
fn fixed_message_actions_do_not_associate_card_inputs() {
    let surface = json!({ "surface": { "components": [
        { "kind": "selection", "prompt": "Capability", "style": "radio", "args_key": "capability",
          "options": [ { "label": "Procurement", "value": "Procurement" } ],
          "submit": { "label": "Ask", "template": "Show agents in {capability}" } },
        { "kind": "chips", "chips": [ { "label": "All agents", "message": "List all agents" } ] },
        { "kind": "list", "items": [ { "label": "Row",
            "action": { "type": "send_message", "text": "Show the row" } } ] },
        { "kind": "button", "label": "Refresh", "action": { "type": "send_message", "text": "Refresh" } }
    ] } });
    let card = teams_card(&surface).expect("card");
    let mut execs: Vec<Value> = Vec::new();
    fn walk(v: &Value, out: &mut Vec<Value>) {
        match v {
            Value::Object(m) => {
                if m.get("type").and_then(Value::as_str) == Some("Action.Execute") {
                    out.push(v.clone());
                }
                m.values().for_each(|x| walk(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(&card, &mut execs);
    assert_eq!(execs.len(), 4, "{card}");
    for e in &execs {
        let want = if e["title"] == "Ask" {
            None
        } else {
            Some("none")
        };
        assert_eq!(e["associatedInputs"].as_str(), want, "{e}");
    }
    let ac = adaptive_card_validator();
    let mut checked = card.clone();
    checked.as_object_mut().unwrap().remove("msteams");
    assert!(ac.validate(&checked).is_ok());
}

/// F3 (crew review): an embed whose fallback is plain prose must show that
/// prose on every channel; an embed nested in an embed's fallback is refused.
#[test]
fn an_embed_text_fallback_renders_on_every_channel() {
    let surface = json!({ "surface": { "components": [
        { "kind": "embed", "url": "https://app.example.com/acme/open?view=map&embed=1",
          "title": "Open the map",
          "fallback": { "kind": "text", "value": "The capability map has 9 agents and 2 gaps." } }
    ] } });
    let prose = "The capability map has 9 agents and 2 gaps.";
    let text = surface_to_text(&extract_surface(&surface).unwrap());
    assert!(text.contains(prose), "text: {text}");
    let card = teams_card(&surface).expect("teams card");
    assert!(card.to_string().contains(prose), "teams: {card}");
    let msgs = build_messages_with(&surface, &signer(), GeOptions::default()).expect("ge");
    assert!(
        serde_json::to_string(&msgs).unwrap().contains(prose),
        "ge: {msgs:?}"
    );

    let nested = json!({ "surface": { "components": [
        { "kind": "embed", "url": "https://a.example/x", "title": "x",
          "fallback": { "kind": "embed", "url": "https://a.example/y", "title": "y",
                        "fallback": { "kind": "text", "value": "deep" } } }
    ] } });
    assert!(
        extract_surface(&nested).is_err(),
        "a nested embed must be refused"
    );
}

/// F6 (crew review): only https deep links survive. The producer is
/// LLM-driven; a prompt-injected `javascript:` (or `file:`, `ms-…:`) link
/// must never become a one-tap action on any channel.
#[test]
fn non_https_links_are_dropped_on_every_channel() {
    let surface = json!({ "surface": { "components": [
        { "kind": "text", "value": "Two links." },
        { "kind": "list", "items": [
            { "label": "Evil row", "action": { "type": "open_url", "url": "javascript:alert(1)" } },
            { "label": "Good row", "action": { "type": "open_url", "url": "https://app.example.com/a" } } ] },
        { "kind": "button", "label": "Evil button", "action": { "type": "open_url", "url": "file:///etc/passwd" } },
        { "kind": "tree", "nodes": [ { "label": "Node",
            "action": { "type": "open_url", "url": "ms-settings:privacy" } } ] }
    ] } });
    let s = extract_surface(&surface).unwrap();
    let text = surface_to_text(&s);
    let card = teams_card(&surface).expect("teams").to_string();
    let ge = serde_json::to_string(
        &build_messages_with(
            &surface,
            &signer(),
            GeOptions {
                open_url: true,
                embed: false,
                ..GeOptions::default()
            },
        )
        .expect("ge"),
    )
    .unwrap();
    for (channel, out) in [("text", &text), ("teams", &card), ("ge", &ge)] {
        for bad in ["javascript:", "file:", "ms-settings:", "Evil button"] {
            assert!(!out.contains(bad), "{channel} keeps {bad}: {out}");
        }
        assert!(
            out.contains("https://app.example.com/a"),
            "{channel} lost the good link"
        );
        assert!(
            out.contains("Evil row"),
            "{channel}: the row itself stays, only its link goes"
        );
    }
}

/// F9 (crew review): a template Selection needs the name its choice fills —
/// an empty `args_key` is refused instead of each channel guessing.
#[test]
fn a_template_selection_without_args_key_is_refused() {
    let s = json!({ "surface": { "components": [
        { "kind": "selection", "prompt": "Pick", "options": [ { "label": "A", "value": "A" } ],
          "submit": { "label": "Go", "template": "Show {x}" } }
    ] } });
    assert!(extract_surface(&s).is_err());
}

/// F9: Teams input ids are card-global, so two template components sharing a
/// field name must not collide — each gets its own namespace, and its signed
/// template names exactly its own inputs.
#[test]
fn teams_template_inputs_are_namespaced_per_component() {
    let form = |title: &str, tpl: &str| {
        json!({ "kind": "form", "title": title, "submit_label": title, "fields": [
            { "name": "capability", "label": "Capability", "kind": "string", "required": true } ],
          "send_template": tpl })
    };
    let surface = json!({ "surface": { "components": [
        form("A", "Show agents in {capability}"),
        form("B", "Show gaps in {capability}")
    ] } });
    let card = teams_card(&surface).expect("card");
    let mut ids: Vec<String> = Vec::new();
    let mut tokens: Vec<String> = Vec::new();
    fn walk(v: &Value, ids: &mut Vec<String>, tokens: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if m.get("type").and_then(Value::as_str) == Some("Input.Text") {
                    ids.push(m["id"].as_str().unwrap().to_string());
                }
                if let Some(ct) = m
                    .get("data")
                    .and_then(|d| d.get("ct"))
                    .and_then(Value::as_str)
                {
                    tokens.push(ct.to_string());
                }
                m.values().for_each(|x| walk(x, ids, tokens));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, ids, tokens)),
            _ => {}
        }
    }
    walk(&card, &mut ids, &mut tokens);
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "input ids must be unique per card: {ids:?}");
    // Each submit's signed template references exactly its own input id.
    for (id, tok) in ids.iter().zip(&tokens) {
        let args: Value = serde_json::from_str(tok.split_once('|').unwrap().1).unwrap();
        let tpl = args["tpl"].as_str().unwrap();
        assert_eq!(
            triton_correlation::message::placeholders(tpl),
            vec![id.clone()],
            "{tpl}"
        );
    }
}

/// Every visible string of a card or an A2UI message array (not action
/// data, not URLs): what a reader sees.
fn visible(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x) {
                    (
                        "text" | "title" | "label" | "placeholder" | "value" | "caption"
                        | "description" | "header",
                        Value::String(s),
                    ) => out.push(s.clone()),
                    ("data" | "url" | "action" | "id" | "targetElements", _) => {}
                    _ => visible(x, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| visible(x, out)),
        _ => {}
    }
}

fn turn3(name: &str) -> Value {
    read_json(&dir(&format!("fixtures/conversation/surfaces/{name}.json")))
}

/// Rule 4 (no internals): no version pin, no "Will be sent as", no raw URL
/// is visible on a turn-3 card, on Teams or Gemini.
#[test]
fn turn3_cards_show_no_internals() {
    let signer = signer();
    for name in ["3a", "3b", "3c", "3d", "3d-list", "3e", "3f", "3g"] {
        let s = turn3(name);
        let mut seen = Vec::new();
        visible(&teams_card(&s).expect("card"), &mut seen);
        for opts in [
            GeOptions::default(),
            GeOptions {
                form_fields: false,
                ..GeOptions::default()
            },
        ] {
            visible(
                &json!(build_messages_with(&s, &signer, opts).expect("ge")),
                &mut seen,
            );
        }
        for t in &seen {
            for bad in [
                "version",
                "Will be sent as",
                "http://",
                "https://",
                "{value}",
            ] {
                assert!(
                    !t.contains(bad),
                    "{name}: visible internal {bad:?} in {t:?}"
                );
            }
        }
    }
}

/// Rule 5 (one anatomy) on Teams: the header carries the title and the
/// stamp, the primary action leads the bar in the primary style, and a
/// table keeps one value per cell.
#[test]
fn turn3_teams_cards_follow_one_anatomy() {
    let card = teams_card(&turn3("3a")).expect("card");
    let body = card["body"].as_array().unwrap();
    let head = &body[0];
    assert_eq!(head["type"], "ColumnSet");
    assert_eq!(head["columns"][0]["items"][0]["text"], "6 planned Agents");
    assert_eq!(head["columns"][1]["items"][0]["text"], "as of 18:17");
    let actions = card["actions"].as_array().expect("actions");
    assert_eq!(actions.len(), 3);
    assert_eq!(actions[0]["title"], "Open in the portfolio");
    assert_eq!(actions[0]["style"], "positive");
    assert!(actions[1].get("style").is_none());
    // Header row + 6 rows, three columns each.
    let rows: Vec<&Value> = body
        .iter()
        .filter(|e| {
            e["type"] == "ColumnSet" && e["columns"].as_array().is_some_and(|c| c.len() == 3)
        })
        .collect();
    assert_eq!(rows.len(), 7, "{card}");
    assert_eq!(rows[1]["columns"][0]["items"][0]["text"], "Agent 3");
    assert_eq!(
        rows[1]["selectAction"]["type"], "Action.Execute",
        "a row opens the Agent card"
    );

    // 3c: one ShowCard per field group; each holds its inputs and its own submit.
    let card = teams_card(&turn3("3c")).expect("card");
    let set = card["body"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "ActionSet")
        .expect("openers");
    let openers = set["actions"].as_array().unwrap();
    let titles: Vec<&str> = openers
        .iter()
        .map(|a| a["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Owner", "Lifecycle", "AI Act class", "More…"]);
    for o in openers {
        assert_eq!(o["type"], "Action.ShowCard");
        let b = o["card"]["body"].as_array().unwrap();
        assert_eq!(b[0]["type"], "Input.ChoiceSet");
        assert_eq!(b[1]["type"], "Input.Text");
        assert_eq!(b[2]["actions"][0]["type"], "Action.Execute");
        assert_eq!(b[2]["actions"][0]["title"], "File for review");
        assert_eq!(b[3]["text"], "A colleague reviews it before it goes live.");
    }

    // 3g: the failure sits in an attention container.
    let card = teams_card(&turn3("3g")).expect("card");
    assert!(
        card["body"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "Container" && e["style"] == "attention"),
        "{card}"
    );
}

/// Rule 1 on Gemini: the rows appear once (no row buttons beside the
/// table), the header's title is left to the text part, and a list row can
/// end in its own button ("Retire").
#[test]
fn turn3_gemini_shows_every_row_once() {
    let signer = signer();
    let comps = |name: &str, opts: GeOptions| -> Vec<Value> {
        build_messages_with(&turn3(name), &signer, opts)
            .unwrap()
            .iter()
            .filter_map(|m| {
                m.pointer("/updateComponents/components")
                    .and_then(Value::as_array)
                    .cloned()
            })
            .flatten()
            .collect()
    };
    let c = comps("3a", GeOptions::default());
    let labels: Vec<&str> = c
        .iter()
        .filter(|x| x["component"] == "MaterialButton")
        .filter_map(|x| x["label"].as_str())
        .collect();
    assert_eq!(
        labels,
        ["High AI Act only", "Without owner"],
        "no row buttons"
    );
    assert!(
        !c.iter().any(|x| x["text"] == "6 planned Agents"),
        "the text part says the title"
    );
    // Rule 3 on Gemini too: a read shows its time (crew F3).
    assert!(
        c.iter().any(|x| x["text"] == "as of 18:17"),
        "the stamp is shown"
    );
    let c = comps("3b", GeOptions::default());
    assert!(
        c.iter()
            .any(|x| x["text"] == "31 Agents in 27 capabilities, 4 domains \u{b7} as of 18:18"),
        "context and stamp on one caption"
    );

    let c = comps("3d-list", GeOptions::default());
    let retire: Vec<&Value> = c.iter().filter(|x| x["label"] == "Retire").collect();
    assert_eq!(retire.len(), 3);
    assert_eq!(
        signer.open(retire[0]["id"].as_str().unwrap()).as_deref(),
        Some("Retire the agent \"Agent 2\".")
    );

    // 3c, fields: four panels (first open), a select and an input each.
    let c = comps("3c", GeOptions::default());
    let panels: Vec<&Value> = c
        .iter()
        .filter(|x| x["component"] == "MaterialExpansionPanel")
        .collect();
    assert_eq!(panels.len(), 4);
    assert_eq!(panels[0]["expanded"], true);
    assert_eq!(panels[1]["expanded"], false);
    assert_eq!(
        c.iter()
            .filter(|x| x["component"] == "MaterialInput")
            .count(),
        4
    );
    let submit = c.iter().find(|x| x["label"] == "File for review").unwrap();
    let id = submit["id"].as_str().unwrap();
    assert_eq!(
        signer.open_action_with(id, &json!({ "value": "IT Operations" })),
        Some(triton_core::a2ui::ge::GeAction::Message(
            "Set the owner of Agent 28 to \"IT Operations\". (version 08cbcd825d41)".into()
        ))
    );
    assert_eq!(
        signer.open_action_with(id, &json!({})),
        // Named by its label, never its key (crew F4).
        Some(triton_core::a2ui::ge::GeAction::Incomplete(vec![
            "New owner".into()
        ]))
    );
    // 3c, chips fallback: one button per value, no input fields.
    let c = comps(
        "3c",
        GeOptions {
            form_fields: false,
            ..GeOptions::default()
        },
    );
    assert_eq!(
        c.iter()
            .filter(|x| x["component"] == "MaterialInput")
            .count(),
        0
    );
    assert!(
        c.iter()
            .any(|x| x["component"] == "MaterialButton" && x["label"] == "IT Operations")
    );
}

/// Values are data, not Markdown: a name like `[x](https://…)` in a header,
/// a table cell or a fact never becomes a link on a Teams card.
#[test]
fn teams_card_values_are_never_markdown() {
    let evil = "[pay](https://evil.example) *now*";
    let surface = json!({ "surface": { "components": [
        { "kind": "header", "title": evil, "context": evil },
        { "kind": "table", "columns": [ { "key": "a", "label": "Agent" } ],
          "rows": [ { "cells": { "a": evil } } ] },
        { "kind": "key_value", "pairs": [ { "label": "Owner", "value": evil } ] }
    ] } });
    let card = teams_card(&surface).expect("card").to_string();
    assert!(!card.contains("\"[pay]("), "{card}");
    assert_eq!(
        card.matches("\\\\[pay\\\\](https://evil.example) \\\\*now\\\\*")
            .count(),
        4,
        "{card}"
    );
}
