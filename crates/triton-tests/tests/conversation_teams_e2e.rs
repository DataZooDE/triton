//! The conversation components through a REAL `triton` binary.
//!
//! No mocks (CLAUDE.md §1): a FakeAgent upstream answers with a surface that
//! carries every conversation component; a spawned Triton renders it on the
//! Microsoft Teams webhook (Adaptive Card, validated against the official
//! schema) and on the REST A2UI v0.9 path. Then the **stateless action round
//! trip**: a chip's Action.Execute token is clicked on a SECOND Triton
//! process (another replica sharing only the correlation key), and the
//! upstream receives a NEW call whose message is the chip's complete request
//! — no card store, no multi-turn. A template submit fills its message from
//! the card inputs; a submit with a missing input is answered, not sent; a
//! tampered token is refused; the Embed opens as a URL task module.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeBotFramework;
use triton_tests::upstream_fixture::FakeAgent;

const AUDIENCE: &str = "triton-msteams-test-appid";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-conversation.yaml")
        .display()
        .to_string()
}

/// Every conversation component in one surface.
pub(crate) fn all_components_surface() -> Value {
    let open = "https://app.example.com/acme/open";
    json!({ "surface": { "components": [
        { "kind": "text", "value": "Here is what I found." },
        { "kind": "list", "title": "Agents", "items": [
            { "label": "Invoice matcher", "detail": "Procurement",
              "action": { "type": "open_url", "url": format!("{open}?object=agent%3Ainvoice-matcher") } },
            { "label": "Line-stop analyser",
              "action": { "type": "send_message", "text": "Show the line-stop analyser" } } ] },
        { "kind": "table", "columns": [ { "key": "a", "label": "Agent" }, { "key": "c", "label": "Capability" } ],
          "rows": [ { "cells": { "a": "Invoice matcher", "c": "Procurement" },
                      "action": { "type": "send_message", "text": "Show the invoice matcher" } } ] },
        { "kind": "chips", "prompt": "Send one of these:", "chips": [
            { "label": "Invoice matcher", "message": "Who owns the invoice matcher in Procurement?" },
            { "label": "Order matcher", "message": "Who owns the order matcher in Sales?" } ] },
        { "kind": "tree", "title": "Capability map", "gaps_toggle": "Only gaps", "nodes": [
            { "label": "Procurement", "counts": [ { "label": "agents", "value": "2" } ], "children": [
                { "label": "Contracts", "gap": true,
                  "action": { "type": "send_message", "text": "Propose an agent for contracts" } } ] },
            { "label": "Production", "counts": [ { "label": "agents", "value": "4" } ] } ] },
        { "kind": "notice", "tone": "warn", "text": "Data from 3 days ago." },
        { "kind": "progress", "steps": [ { "label": "Read", "state": "done" },
                                         { "label": "Match", "state": "active" } ], "fraction": 0.5 },
        { "kind": "key_value", "pairs": [ { "label": "Owner", "before": "none", "value": "Anna Weber" } ] },
        { "kind": "selection", "prompt": "Area", "style": "radio", "args_key": "area",
          "options": [ { "label": "Procurement", "value": "Procurement" } ],
          "submit": { "label": "Ask", "template": "Show agents in {area} without an owner" } },
        { "kind": "form", "title": "Filter", "submit_label": "Show", "fields": [
            { "name": "capability", "label": "Capability", "kind": "select", "required": true,
              "options": [ { "label": "Production", "value": "Production" } ] },
            { "name": "status", "label": "Status", "kind": "string", "required": true } ],
          "send_template": "Show agents in {capability} that are {status}" },
        { "kind": "embed", "url": format!("{open}?view=capability-map&embed=1"), "title": "Open the map",
          "fallback": { "kind": "notice", "text": "The map opens in a dialog." } },
        { "kind": "button", "label": "Open the analysis",
          "action": { "type": "open_url", "url": format!("{open}?view=agents") } }
    ] } })
}

fn env_with(fake: &FakeBotFramework, agent: &FakeAgent) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ])
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn jwt(fake: &FakeBotFramework) -> String {
    fake.sign_jwt(json!({
        "iss": "https://api.botframework.com",
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
        "serviceurl": fake.service_url(),
    }))
}

fn activity(kind: &str, name: Option<&str>, text: Option<&str>, value: Option<Value>) -> Value {
    let mut a = json!({
        "type": kind,
        "id": "act-1",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": { "id": "a:conv-1", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
    });
    if let Some(n) = name {
        a["name"] = json!(n);
    }
    if let Some(t) = text {
        a["text"] = json!(t);
        a["textFormat"] = json!("plain");
    }
    if let Some(v) = value {
        a["value"] = v;
    }
    a
}

fn execute(data: Value) -> Value {
    activity(
        "invoke",
        Some("adaptiveCard/action"),
        None,
        Some(
            json!({ "action": { "type": "Action.Execute", "verb": "agentAction", "data": data } }),
        ),
    )
}

async fn post(
    webhook: std::net::SocketAddr,
    fake: &FakeBotFramework,
    body: &Value,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {}", jwt(fake)))
        .json(body)
        .send()
        .await
        .expect("POST webhook")
}

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(
            start.elapsed() < deadline,
            "probe timed out after {deadline:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Depth-first search for an action object with this `type` and `title`.
fn find_action<'a>(v: &'a Value, ty: &str, title: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => {
            if m.get("type").and_then(Value::as_str) == Some(ty)
                && m.get("title").and_then(Value::as_str) == Some(title)
            {
                return Some(v);
            }
            m.values().find_map(|x| find_action(x, ty, title))
        }
        Value::Array(a) => a.iter().find_map(|x| find_action(x, ty, title)),
        _ => None,
    }
}

/// The card-global id of the input a template component names `name`
/// (Teams namespaces each component's inputs, `t<k>.<name>`); the `nth`
/// match in card order.
fn input_id(card: &Value, name: &str, nth: usize) -> String {
    fn walk(v: &Value, name: &str, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if m.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.starts_with("Input."))
                    && let Some(id) = m.get("id").and_then(Value::as_str)
                    && id.rsplit_once('.').map(|(_, n)| n) == Some(name)
                {
                    out.push(id.to_string());
                }
                m.values().for_each(|x| walk(x, name, out));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, name, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(card, name, &mut out);
    out.get(nth)
        .cloned()
        .unwrap_or_else(|| panic!("no input {name}#{nth} in {card}"))
}

fn token_of(action: &Value) -> String {
    action["data"]["ct"]
        .as_str()
        .expect("signed ct")
        .to_string()
}

/// The `message` arg of the upstream call number `i` (0-based).
fn upstream_message(agent: &FakeAgent, i: usize) -> String {
    let bodies = wait_for(Duration::from_secs(10), || {
        let b = agent.bodies_seen();
        (b.len() > i).then_some(b)
    });
    bodies[i]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("upstream call {i} carries no message: {}", bodies[i]))
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teams_renders_every_component_and_a_click_is_a_new_stateless_request() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAgent::start_returning(all_components_surface()).await;
    let replica_a =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake, &agent)).await;
    let replica_b =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake, &agent)).await;
    let a = replica_a.chat_webhook_addr.expect("webhook A");
    let b = replica_b.chat_webhook_addr.expect("webhook B");

    // 1. A typed question → the agent's surface → one Adaptive Card.
    let resp = post(
        a,
        &fake,
        &activity("message", None, Some("show me the gaps"), None),
    )
    .await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let reply = wait_for(Duration::from_secs(10), || {
        fake.captured().into_iter().next()
    });
    let card = reply.body["attachments"][0]["content"].clone();
    assert_eq!(card["type"], "AdaptiveCard", "{}", reply.body);
    assert_eq!(upstream_message(&agent, 0), "show me the gaps");

    // Valid against the official Adaptive Cards schema (minus the Teams
    // host extension, see schemas/README.md).
    let ac = super::conversation_components::adaptive_card_validator();
    let mut checked = card.clone();
    checked.as_object_mut().unwrap().remove("msteams");
    if let Err(e) = ac.validate(&checked) {
        panic!("card fails the Adaptive Card schema: {e:#}\n{card}");
    }
    let s = card.to_string();
    // No internals (turn 3, rule 4): the composed request is not shown.
    assert!(!s.contains("Will be sent as"), "{card}");
    for needle in [
        "Invoice matcher",         // list + table + chip
        "Action.OpenUrl",          // list row / button deep link
        "Action.ToggleVisibility", // tree expand + only-gaps
        "Only gaps",
        "\"style\":\"warning\"",      // notice
        "\u{25b6} Match",             // progress status line
        "none \u{2192} Anna Weber",   // key_value change
        "Input.ChoiceSet",            // selection radio + form select
        "\"style\":\"expanded\"",     // radio
        "task/fetch",                 // embed → task module
        "The map opens in a dialog.", // embed fallback inline
    ] {
        assert!(s.contains(needle), "card lacks {needle:?}: {card}");
    }

    // 2. Click a chip on REPLICA B — it never saw the card. The token alone
    //    carries the complete request; the upstream gets a NEW call with it.
    let chip = find_action(&card, "Action.Execute", "Order matcher").expect("chip action");
    let resp = post(b, &fake, &execute(json!({ "ct": token_of(chip) }))).await;
    assert!(
        resp.status().is_success(),
        "chip click on replica B: {}",
        resp.status()
    );
    assert_eq!(
        upstream_message(&agent, 1),
        "Who owns the order matcher in Sales?",
        "the click must send the chip's complete message as a new request"
    );

    // 3. A template submit: the card inputs fill the template.
    let submit = find_action(&card, "Action.Execute", "Show").expect("form submit");
    let resp = post(
        a,
        &fake,
        &execute(json!({
            "ct": token_of(submit),
            input_id(&card, "capability", 0): "Production",
            input_id(&card, "status", 0): "in pilot",
        })),
    )
    .await;
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(
        upstream_message(&agent, 2),
        "Show agents in Production that are in pilot"
    );

    // 4. A missing input is answered (which one), never sent half-filled.
    let resp = post(
        a,
        &fake,
        &execute(json!({ "ct": token_of(submit), input_id(&card, "capability", 0): "Production" })),
    )
    .await;
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["value"]
            .as_str()
            .unwrap_or_default()
            .contains("status"),
        "{body}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        agent.bodies_seen().len(),
        3,
        "an incomplete template must not reach the agent"
    );

    // 5. A tampered token is refused and never dispatched.
    let mut forged = token_of(chip);
    forged.insert(3, 'x');
    let resp = post(b, &fake, &execute(json!({ "ct": forged }))).await;
    assert_eq!(resp.status(), 401);
    assert_eq!(agent.bodies_seen().len(), 3);

    // 6. The embed's button opens a URL task module (2d prep).
    let open = find_action(&card, "Action.Submit", "Open the map").expect("embed button");
    let resp = post(
        b,
        &fake,
        &activity(
            "invoke",
            Some("task/fetch"),
            None,
            Some(json!({ "data": open["data"] })),
        ),
    )
    .await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let task: Value = resp.json().await.unwrap();
    assert_eq!(task["task"]["type"], "continue", "{task}");
    assert_eq!(
        task["task"]["value"]["url"],
        "https://app.example.com/acme/open?view=capability-map&embed=1"
    );
    assert_eq!(
        agent.bodies_seen().len(),
        3,
        "opening the embed runs no turn"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_a2ui_v09_carries_every_component_and_reverses_to_the_canonical_surface() {
    let agent = FakeAgent::start_returning(all_components_surface()).await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .header("Accept", "application/json+a2ui; version=0.9")
        .json(&json!({ "message": "show me the gaps" }))
        .send()
        .await
        .expect("POST /v1/tools/assistant");
    assert!(resp.status().is_success(), "REST {}", resp.status());
    let body: Value = resp.json().await.unwrap();
    let stream = body["result"]["stream"]
        .as_array()
        .expect("v0.9 stream")
        .clone();
    let types: Vec<&str> = stream.iter().filter_map(|n| n["type"].as_str()).collect();
    for t in [
        "list",
        "table",
        "chips",
        "tree",
        "notice",
        "progress",
        "key_value",
        "selection",
        "form",
        "embed",
        "button",
    ] {
        assert!(types.contains(&t), "stream lacks {t}: {types:?}");
    }
    // The envelope reverses to exactly the canonical surface the agent sent.
    let back = triton_core::a2ui::envelope_to_surface(&body).expect("reversible");
    let want = triton_core::a2ui::extract_surface(&all_components_surface()).unwrap();
    assert_eq!(
        serde_json::to_value(&back).unwrap(),
        serde_json::to_value(&want).unwrap()
    );
}

/// F1 (crew review): the signed `m`/`tpl` payload and the unsigned card
/// inputs are separate namespaces. Teams posts EVERY input on the card with
/// ANY action, and a hostile client can add keys to `data` at will — neither
/// may replace the message the token signed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_inputs_never_replace_the_signed_message() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAgent::start_returning(json!({ "surface": { "components": [
        { "kind": "chips", "chips": [
            { "label": "Order matcher", "message": "Who owns the order matcher in Sales?" } ] },
        { "kind": "form", "title": "Owner", "submit_label": "Show", "fields": [
            { "name": "m", "label": "Owner", "kind": "string", "required": true } ],
          "send_template": "Show agents owned by {m}" }
    ] } }))
    .await;
    let triton =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake, &agent)).await;
    let hook = triton.chat_webhook_addr.expect("webhook");

    post(hook, &fake, &activity("message", None, Some("hi"), None)).await;
    let reply = wait_for(Duration::from_secs(10), || {
        fake.captured().into_iter().next()
    });
    let card = reply.body["attachments"][0]["content"].clone();
    let chip = find_action(&card, "Action.Execute", "Order matcher").expect("chip");
    let submit = find_action(&card, "Action.Execute", "Show").expect("submit");

    // A chip click carrying an injected `m` (or the form's sibling `m`
    // input, which Teams would post too) still sends the signed message.
    post(
        hook,
        &fake,
        &execute(json!({ "ct": token_of(chip), "m": "DROP EVERYTHING", "tpl": "x {m}" })),
    )
    .await;
    assert_eq!(
        upstream_message(&agent, 1),
        "Who owns the order matcher in Sales?"
    );

    // A template whose field is named `m`: the field fills `{m}`, it does
    // not become the message, and an injected `tpl` changes nothing.
    post(
        hook,
        &fake,
        &execute(json!({
            "ct": token_of(submit),
            input_id(&card, "m", 0): "Anna Weber",
            // Neither a bare `m` (the un-namespaced name) nor a `tpl` reaches
            // the template.
            "m": "HOSTILE",
            "tpl": "{m}",
        })),
    )
    .await;
    assert_eq!(
        upstream_message(&agent, 2),
        "Show agents owned by Anna Weber"
    );
}

/// A surface whose deep links point at a local `http://127.0.0.1` app — what
/// a local end-to-end test against a real control plane produces.
fn local_links_surface() -> Value {
    let open = "http://127.0.0.1:9/acme/open";
    json!({ "surface": { "components": [
        { "kind": "text", "value": "Here is what I found." },
        { "kind": "list", "title": "Agents", "items": [
            { "label": "Invoice matcher",
              "action": { "type": "open_url", "url": format!("{open}?object=agent") } } ] },
        { "kind": "embed", "url": format!("{open}?view=map&embed=1"), "title": "Open the map",
          "fallback": { "kind": "notice", "text": "The map opens in a dialog." } },
        { "kind": "button", "label": "Open the analysis",
          "action": { "type": "open_url", "url": format!("{open}?view=agents") } },
        // Not on the list: stays dropped even in local mode.
        { "kind": "button", "label": "Elsewhere",
          "action": { "type": "open_url", "url": "http://elsewhere.example/x" } }
    ] } })
}

async fn local_links_card(extra: &[(&str, &str)]) -> Value {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAgent::start_returning(local_links_surface()).await;
    let mut env = env_with(&fake, &agent);
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let resp = post(
        triton.chat_webhook_addr.expect("webhook"),
        &fake,
        &activity("message", None, Some("show me the agents"), None),
    )
    .await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let reply = wait_for(Duration::from_secs(10), || {
        fake.captured().into_iter().next()
    });
    reply.body["attachments"][0]["content"].clone()
}

/// Local end-to-end tests run their apps on `http://127.0.0.1`. With the
/// explicit local-only allowance `TRITON_ALLOW_HTTP_LINK_HOSTS`, deep links
/// and the embed to a listed host survive, so such tests can assert them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_http_links_survive_on_an_allowed_host_in_local_mode() {
    let card = local_links_card(&[("TRITON_ALLOW_HTTP_LINK_HOSTS", "127.0.0.1,localhost")]).await;
    let s = card.to_string();
    let button = find_action(&card, "Action.OpenUrl", "Open the analysis")
        .unwrap_or_else(|| panic!("the local deep link is kept: {card}"));
    assert_eq!(button["url"], "http://127.0.0.1:9/acme/open?view=agents");
    assert!(
        s.contains("http://127.0.0.1:9/acme/open?object=agent"),
        "list row link kept: {card}"
    );
    assert!(s.contains("task/fetch"), "the embed opens: {card}");
    assert!(
        !s.contains("elsewhere.example"),
        "an unlisted http host stays dropped: {card}"
    );
}

/// Without the allowance nothing changes: an `http://` link is never an
/// action (F6), local mode or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_links_stay_dropped_without_the_allowance() {
    let card = local_links_card(&[]).await;
    let s = card.to_string();
    assert!(!s.contains("http://127.0.0.1:9"), "{card}");
    assert!(!s.contains("Action.OpenUrl"), "{card}");
}

/// The allowance is local-only: outside `TRITON_ENV=local` the binary
/// refuses to start rather than serve http links.
#[test]
fn the_http_link_allowance_refuses_to_start_outside_local() {
    let out = std::process::Command::new(triton_tests::locate_triton_binary())
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_ENV", "nonprod")
        .env("TRITON_ALLOW_HTTP_LINK_HOSTS", "127.0.0.1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn triton");
    let mut child = out;
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().expect("wait") {
            break st;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("triton started with the http allowance outside local mode");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = child.wait_with_output().expect("output");
    assert_eq!(status.code(), Some(2), "{status:?}");
    let logs = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(logs.contains("TRITON_ALLOW_HTTP_LINK_HOSTS"), "{logs}");
}

/// A Teams turn routed to an upstream that answers `404`, and a card
/// reply whose `get_theme` upstream answers `404`. The user sees the
/// neutral notice resp. the ordinary unbranded card — never the raw error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teams_degrades_an_upstream_404_and_a_missing_get_theme() {
    // 1. The turn's own tool answers 404.
    let fake = FakeBotFramework::start().await;
    let ghost = FakeAgent::start_not_found().await;
    // The async courier (the deployment's mode) answers a failed turn
    // with a notice; the inline path acks and stays silent.
    let mut env = env_with(&fake, &ghost);
    env.insert("TRITON_MSTEAMS_ASYNC".to_string(), "1".to_string());
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let resp = post(
        triton.chat_webhook_addr.expect("webhook"),
        &fake,
        &activity("message", None, Some("show me the agents"), None),
    )
    .await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let reply = wait_for(Duration::from_secs(10), || {
        fake.captured()
            .into_iter()
            .find(|c| c.body["type"] == "message")
    });
    let s = reply.body.to_string();
    assert!(
        !s.contains("404") && !s.contains("Not Found") && !s.contains("upstream"),
        "raw error reached Teams: {s}"
    );
    assert!(
        reply.body["text"]
            .as_str()
            .is_some_and(|t| t.contains("try again")),
        "the neutral notice: {s}"
    );

    // 2. The answer is fine, `get_theme` answers 404: the card still comes.
    let fake = FakeBotFramework::start().await;
    let agent = FakeAgent::start_returning(all_components_surface()).await;
    let mut env = env_with(&fake, &agent);
    env.insert(
        "TRITON_STATIC_UPSTREAMS".to_string(),
        format!(
            "assistant={},get_theme={}",
            agent.host_port(),
            ghost.host_port()
        ),
    );
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let resp = post(
        triton.chat_webhook_addr.expect("webhook"),
        &fake,
        &activity("message", None, Some("show me the gaps"), None),
    )
    .await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let reply = wait_for(Duration::from_secs(10), || {
        fake.captured()
            .into_iter()
            .find(|c| c.body["type"] == "message")
    });
    let card = &reply.body["attachments"][0]["content"];
    assert_eq!(card["type"], "AdaptiveCard", "{}", reply.body);
    let s = card.to_string();
    assert!(s.contains("Invoice matcher"), "{s}");
    assert!(!s.contains("404") && !s.contains("Not Found"), "{s}");
}
