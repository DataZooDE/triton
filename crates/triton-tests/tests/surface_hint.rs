//! #352: Triton tells the upstream agent which channel a turn will be rendered
//! on (`X-Triton-Channel`). The hint is non-authoritative rendering advice;
//! the mapper still degrades whatever comes back.
//!
//! No mocks: a real spawned `triton`, a real recording upstream (`FakeAgent`)
//! and real inbound requests over TCP.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::json;
use triton_tests::{TritonProcess, upstream_fixture::FakeAgent};

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("condition not met within {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spawn `triton` fronting `agent` as tool `assistant`, plus `extra` env.
async fn triton_with(agent: &FakeAgent, extra: &[(&str, String)]) -> TritonProcess {
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    for (k, v) in extra {
        env.insert((*k).to_string(), v.clone());
    }
    TritonProcess::spawn_with_env(Duration::from_secs(5), env).await
}

/// The channel header of the agent's first recorded **dispatch**.
///
/// Only requests that carry `X-Triton-Tool` or `X-Triton-MCP` count: every
/// Triton dispatch sends one of them. Under the parallel suite an ephemeral
/// port can be recycled, so a stray request meant for another test's dropped
/// fake (e.g. a courier POST from a process still shutting down) may land
/// here. Ignoring those keeps the test about Triton's dispatches, and a real
/// dispatch without the channel header still fails it.
fn first_channel(agent: &FakeAgent) -> Option<String> {
    let headers = wait_for(Duration::from_secs(5), || {
        agent
            .headers_seen()
            .into_iter()
            .find(|h| h.contains_key("X-Triton-Tool") || h.contains_key("X-Triton-MCP"))
    });
    headers
        .get("X-Triton-Channel")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_dispatch_carries_channel_header() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(&agent, &[]).await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": "hi" }))
        .send()
        .await
        .expect("POST /v1/tools/assistant");
    assert!(resp.status().is_success(), "{}", resp.status());

    assert_eq!(first_channel(&agent).as_deref(), Some("rest"));
}

/// The surface every chat-channel driver below returns from the upstream.
fn text_surface() -> serde_json::Value {
    json!({ "surface": { "components": [ { "kind": "text", "value": "hello" } ] } })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_sse_stream_carries_channel_header() {
    let agent = FakeAgent::start_streaming().await;
    let triton = triton_with(&agent, &[]).await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .header("Accept", "text/event-stream")
        .json(&json!({ "message": "hi" }))
        .send()
        .await
        .expect("POST /v1/tools/assistant (SSE)");
    assert!(resp.status().is_success(), "{}", resp.status());
    let _ = resp.text().await;

    let headers = agent.headers_seen();
    assert_eq!(
        headers[0].get("accept").and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "the streaming path must be the one under test"
    );
    assert_eq!(first_channel(&agent).as_deref(), Some("rest"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_tools_call_carries_channel_header() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(&agent, &[]).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/", triton.mcp_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "assistant", "arguments": { "message": "hi" } }
        }))
        .send()
        .await
        .expect("POST mcp tools/call");
    assert!(resp.status().is_success(), "{}", resp.status());

    assert_eq!(first_channel(&agent).as_deref(), Some("mcp"));
}

/// `resources/read` and `updateModelContext` are upstream calls too. They get
/// the header; their bodies are relayed unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_resource_relays_carry_channel_header() {
    let agent = FakeAgent::start_mcp_apps().await;
    let triton = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "local".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("peacock={}", agent.host_port()),
            ),
        ]),
    )
    .await;
    let rpc = |body: serde_json::Value| {
        let url = format!("http://{}/", triton.mcp_addr);
        async move {
            reqwest::Client::new()
                .post(url)
                .bearer_auth("dev-token")
                .json(&body)
                .send()
                .await
                .expect("POST mcp")
        }
    };
    rpc(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/read",
                "params": { "uri": "ui://peacock/r1" } }),
    )
    .await;
    let record = json!({ "report_id": "r1", "salient_summary": "ok" });
    rpc(
        json!({ "jsonrpc": "2.0", "id": 2, "method": "updateModelContext",
                "params": { "uri": "ui://peacock/r1", "record": record } }),
    )
    .await;

    let verbs = agent.mcp_verbs_seen();
    let channels = agent.channels_seen();
    assert_eq!(
        verbs.len(),
        2,
        "both relays must reach the upstream: {verbs:?}"
    );
    assert_eq!(channels, vec![Some("mcp".to_string()); 2]);
    assert_eq!(
        agent.bodies_seen()[1],
        record,
        "updateModelContext must stay byte-for-byte"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_message_send_carries_channel_header() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(&agent, &[]).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/message:send", triton.a2a_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "parts": [ { "data": { "tool": "assistant", "args": { "message": "hi" } } } ]
        }))
        .send()
        .await
        .expect("POST /message:send");
    assert!(resp.status().is_success(), "{}", resp.status());

    assert_eq!(first_channel(&agent).as_deref(), Some("a2a"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whatsapp_cloud_turn_carries_channel_header() {
    let turn = crate::surface_baseline::drive_whatsapp_cloud(text_surface()).await;
    assert_eq!(first_channel(&turn.agent).as_deref(), Some("whatsapp"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telegram_turn_carries_channel_header() {
    let turn = crate::surface_baseline::drive_telegram(text_surface()).await;
    assert_eq!(first_channel(&turn.agent).as_deref(), Some("telegram"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_turn_carries_channel_header() {
    let turn = crate::surface_baseline::drive_signal(text_surface()).await;
    assert_eq!(first_channel(&turn.agent).as_deref(), Some("signal"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discord_slash_command_carries_channel_header() {
    let turn = crate::surface_baseline::drive_discord(text_surface()).await;
    assert_eq!(first_channel(&turn.agent).as_deref(), Some("discord"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_chat_turn_carries_channel_header() {
    let turn = crate::surface_baseline::drive_google_chat(text_surface()).await;
    assert_eq!(first_channel(&turn.agent).as_deref(), Some("googlechat"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn msteams_turn_carries_channel_header() {
    let turn = crate::surface_baseline::drive_msteams(text_surface()).await;
    assert_eq!(first_channel(&turn.agent).as_deref(), Some("msteams"));
}

/// The identity resolver (`identity.kind: upstream`) is dispatched under
/// `messenger:telegram:identity`; the channel is still where the user is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_resolver_dispatch_carries_the_users_channel() {
    use triton_tests::chat_courier_fixture::FakeTelegramApi;
    const SECRET: &str = "webhook-secret-for-test";
    let resolver = FakeAgent::start_returning(json!({
        "sub": "telegram:42", "scopes": ["chat"], "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let telegram = FakeTelegramApi::start().await;
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-telegram-upstream-identity.yaml");
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "local".to_string()),
            (
                "TRITON_MANIFEST_PATH".to_string(),
                manifest.display().to_string(),
            ),
            ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!(
                    "assistant={},resolve_identity={}",
                    agent.host_port(),
                    resolver.host_port()
                ),
            ),
        ]),
    )
    .await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", SECRET)
        .json(&json!({
            "update_id": 100,
            "message": {
                "message_id": 1,
                "from": { "id": 42, "is_bot": false, "first_name": "Mallory" },
                "chat": { "id": 42, "type": "private" },
                "date": 1_700_000_000,
                "text": "hi"
            }
        }))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    assert_eq!(first_channel(&resolver).as_deref(), Some("telegram"));
    assert_eq!(first_channel(&agent).as_deref(), Some("telegram"));
}

/// A chat dashboard rasterised by an upstream `render_a2ui_to_png` is
/// rendered for the user's channel, not a generic `chat`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rasterizer_delegate_carries_the_users_channel() {
    use triton_tests::chat_courier_fixture::FakeTelegramApi;
    const SECRET: &str = "secret-resolved-from-vault";
    let telegram = FakeTelegramApi::start().await;
    let renderer = FakeAgent::start_returning(json!({
        // "\x89PNG\r\n\x1a\n" — just enough for the adapter to ship it.
        "png_base64": "iVBORw0KGgo="
    }))
    .await;
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-vault-resolver.yaml");
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "local".to_string()),
            (
                "TRITON_MANIFEST_PATH".to_string(),
                manifest.display().to_string(),
            ),
            ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
            (
                "TRITON_RASTERIZE_UPSTREAM".to_string(),
                "render_a2ui_to_png".to_string(),
            ),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("render_a2ui_to_png={}", renderer.host_port()),
            ),
            ("TRITON_TG_WEBHOOK_SECRET".to_string(), SECRET.to_string()),
            (
                "TRITON_TG_BOT_TOKEN".to_string(),
                "12345:resolved-bot-token".to_string(),
            ),
            (
                "TRITON_TG_SENDERS".to_string(),
                r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#.to_string(),
            ),
            (
                "TRITON_TG_CORRELATION_KEY".to_string(),
                "correlation-key-from-vault".to_string(),
            ),
        ]),
    )
    .await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", SECRET)
        .json(&json!({
            "update_id": 100,
            "message": {
                "message_id": 1,
                "from": { "id": 42, "is_bot": false, "first_name": "Alice" },
                "chat": { "id": 42, "type": "private" },
                "date": 1_700_000_000,
                "text": "/demo"
            }
        }))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    assert_eq!(first_channel(&renderer).as_deref(), Some("telegram"));
}

fn fixture(name: &str) -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
        .display()
        .to_string()
}

/// WhatsApp Web (Baileys bridge) is the same `whatsapp` channel as Cloud API.
/// Mapping `echo` upstream skips the in-process `echo`, so the bridge's
/// default route reaches the recording agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whatsapp_bridge_turn_carries_channel_header() {
    use triton_tests::whatsapp_bridge_fixture::FakeWhatsAppBridge;
    let agent = FakeAgent::start_returning(text_surface()).await;
    let bridge = FakeWhatsAppBridge::start(vec![
        json!({ "type": "message", "from": "4915112345678", "text": "hi" }),
    ])
    .await;
    let _proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "local".to_string()),
            (
                "TRITON_MANIFEST_PATH".to_string(),
                fixture("manifest-whatsapp-bridge-test.yaml"),
            ),
            ("TRITON_WHATSAPP_BRIDGE_ADDR".to_string(), bridge.tcp_uri()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("echo={}", agent.host_port()),
            ),
        ]),
    )
    .await;
    assert_eq!(first_channel(&agent).as_deref(), Some("whatsapp"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twilio_whatsapp_turn_carries_channel_header() {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use triton_tests::chat_courier_fixture::FakeTwilioApi;
    const AUTH_TOKEN: &str = "twilio-auth-token-for-test";
    let agent = FakeAgent::start_returning(text_surface()).await;
    let twilio = FakeTwilioApi::start().await;
    let port = triton_tests::free_tcp_port();
    let webhook_url = format!("http://127.0.0.1:{port}/twilio-whatsapp/webhook");
    let _proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "local".to_string()),
            (
                "TRITON_MANIFEST_PATH".to_string(),
                fixture("manifest-twilio-whatsapp-test.yaml"),
            ),
            ("TRITON_TWILIO_API_BASE".to_string(), twilio.url()),
            ("TRITON_CHAT_WEBHOOK_PORT".to_string(), port.to_string()),
            (
                "TRITON_TWILIO_WHATSAPP_PUBLIC_URL".to_string(),
                webhook_url.clone(),
            ),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("echo={}", agent.host_port()),
            ),
        ]),
    )
    .await;

    let mut form = vec![
        ("Body", "hi"),
        ("From", "whatsapp:+491701234567"),
        ("MessageSid", "SM00000000000000000000000000000000"),
        ("To", "whatsapp:+14155238886"),
    ];
    // Twilio signs the URL followed by the form pairs sorted by key.
    form.sort_by_key(|(k, _)| *k);
    let mut signed = webhook_url.clone();
    for (k, v) in &form {
        signed.push_str(k);
        signed.push_str(v);
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(AUTH_TOKEN.as_bytes()).unwrap();
    mac.update(signed.as_bytes());
    let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    let resp = reqwest::Client::new()
        .post(&webhook_url)
        .header("X-Twilio-Signature", sig)
        .form(&form)
        .send()
        .await
        .expect("POST twilio webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    assert_eq!(first_channel(&agent).as_deref(), Some("twilio_whatsapp"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discord_gateway_turn_carries_channel_header() {
    use triton_tests::discord_gateway_fixture::FakeDiscordGateway;
    let agent = FakeAgent::start_returning(text_surface()).await;
    let gateway = FakeDiscordGateway::start(vec![json!({
        "id": "m1",
        "channel_id": "chan-1",
        "author": { "id": "7777", "bot": false, "username": "Alice" },
        "content": "hi",
        "timestamp": "2026-05-25T10:00:00.000000+00:00"
    })])
    .await;
    let _proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "local".to_string()),
            (
                "TRITON_MANIFEST_PATH".to_string(),
                fixture("manifest-discord-gateway-test.yaml"),
            ),
            (
                "TRITON_DISCORD_GATEWAY_URL".to_string(),
                gateway.gateway_url(),
            ),
            ("TRITON_DISCORD_API_BASE".to_string(), gateway.rest_base()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("echo={}", agent.host_port()),
            ),
        ]),
    )
    .await;
    assert_eq!(first_channel(&agent).as_deref(), Some("discord"));
}

/// A proactive `/v1/outbound` send to Teams re-dispatches `get_theme` while
/// it renders. That call is made for the delivery channel, so it carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbound_delivery_redispatch_carries_the_delivery_channel() {
    use std::time::{SystemTime, UNIX_EPOCH};
    use triton_tests::TestIssuer;
    use triton_tests::chat_courier_fixture::FakeBotFramework;
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let theme = FakeAgent::start_returning(json!({ "brand": { "name": "Acme" } })).await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "local".to_string()),
            (
                "TRITON_MANIFEST_PATH".to_string(),
                fixture("manifest-msteams-async-test.yaml"),
            ),
            ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
            ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
            (
                "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
                "127.0.0.1".to_string(),
            ),
            ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
            (
                "TRITON_OIDC_AUDIENCE".to_string(),
                "agents-local".to_string(),
            ),
            (
                "TRITON_OUTBOUND_AUDIENCE".to_string(),
                "outbound-local".to_string(),
            ),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("get_theme={}", theme.host_port()),
            ),
        ]),
    )
    .await;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let token = issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "async-ops-courier",
        "aud": "outbound-local",
        "exp": now + 60,
        "iat": now,
        "tenant": "acme",
        "scope": "outbound:send",
    }));
    let sealed = triton_correlation::encode_with_cap(
        "__conversation_ref",
        &json!({
            "service_url": fake.service_url(),
            "conversation_id": "a:conv-1",
            "bot_id": "28:bot-1",
            "user_id": "29:1abc",
            "tenant_id": "acme",
        }),
        b"correlation-key-for-test",
        4096,
    )
    .expect("seal the reference");
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(token)
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "your scenario finished" },
            "reference": { "channel": "msteams", "ref": sealed },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(resp.status(), 202, "{:?}", resp.text().await.ok());

    let tools = wait_for(Duration::from_secs(5), || {
        let v = theme.tools_seen();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(tools[0].as_deref(), Some("get_theme"));
    assert_eq!(first_channel(&theme).as_deref(), Some("msteams"));
}

// ── #352 step 3: the capability descriptor in `_triton.surface` ─────────────

/// The JSON body of the agent's first recorded dispatch (see
/// [`first_channel`] for why only dispatches count).
fn first_body(agent: &FakeAgent) -> serde_json::Value {
    wait_for(Duration::from_secs(5), || {
        let headers = agent.headers_seen();
        let bodies = agent.bodies_seen();
        headers
            .iter()
            .position(|h| h.contains_key("X-Triton-Tool") || h.contains_key("X-Triton-MCP"))
            .and_then(|i| bodies.get(i).cloned())
    })
}

fn surface_of(agent: &FakeAgent) -> serde_json::Value {
    let body = first_body(agent);
    let surface = body["_triton"]["surface"].clone();
    assert!(surface.is_object(), "no _triton.surface in {body}");
    assert_eq!(surface["v"], 1, "descriptor is versioned: {surface}");
    surface
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whatsapp_turn_carries_its_surface_descriptor() {
    let turn = crate::surface_baseline::drive_whatsapp_cloud(text_surface()).await;
    let body = first_body(&turn.agent);
    assert_eq!(
        body["message"], "revenue please",
        "the original args are untouched"
    );
    let s = surface_of(&turn.agent);
    assert_eq!(s["channel"], "whatsapp");
    assert_eq!(s["buttons"], json!({ "supported": true, "max": 3 }));
    assert_eq!(s["tables"], "text");
    assert_eq!(s["markdown"], "basic");
    assert_eq!(s["max_text_bytes"], 4096);
    assert!(
        s["guidance"]
            .as_str()
            .is_some_and(|g| g.starts_with("WhatsApp")),
        "{s}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_turn_advertises_no_buttons() {
    let turn = crate::surface_baseline::drive_signal(text_surface()).await;
    let s = surface_of(&turn.agent);
    assert_eq!(s["channel"], "signal");
    assert_eq!(s["buttons"], json!({ "supported": false, "max": null }));
    assert_eq!(s["markdown"], "none");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn msteams_turn_advertises_native_tables_and_charts() {
    let turn = crate::surface_baseline::drive_msteams(text_surface()).await;
    let s = surface_of(&turn.agent);
    assert_eq!(s["channel"], "msteams");
    assert_eq!(s["tables"], "native");
    assert_eq!(s["charts"], "native");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_chat_turn_advertises_streaming() {
    let turn = crate::surface_baseline::drive_google_chat(text_surface()).await;
    let s = surface_of(&turn.agent);
    assert_eq!(s["channel"], "googlechat");
    assert_eq!(s["streaming"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_turn_advertises_the_full_surface() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(&agent, &[]).await;
    reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": "hi" }))
        .send()
        .await
        .expect("POST");
    let s = surface_of(&agent);
    assert_eq!(s["channel"], "rest");
    assert_eq!(s["tables"], "native");
    assert_eq!(s["buttons"]["supported"], true);
    assert_eq!(
        s["max_text_bytes"],
        serde_json::Value::Null,
        "no Triton cap"
    );
}

/// The kill-switch drops the body field for an agent that rejects unknown
/// args. The header is not part of the body, so it stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hint_kill_switch_drops_the_body_field_but_keeps_the_header() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(
        &agent,
        &[("TRITON_STATIC_UPSTREAM_SURFACE_HINT", "off".to_string())],
    )
    .await;
    reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": "hi" }))
        .send()
        .await
        .expect("POST");
    assert_eq!(first_body(&agent), json!({ "message": "hi" }));
    assert_eq!(first_channel(&agent).as_deref(), Some("rest"));
}

/// Triton owns `_triton`: a caller can't forge the descriptor (or smuggle
/// anything else under the reserved key) to the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_supplied_triton_key_is_replaced() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(&agent, &[]).await;
    reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .json(&json!({
            "message": "hi",
            "_triton": { "surface": { "channel": "email" }, "smuggled": true }
        }))
        .send()
        .await
        .expect("POST");
    let body = first_body(&agent);
    assert_eq!(body["_triton"]["surface"]["channel"], "rest", "{body}");
    assert!(body["_triton"].get("smuggled").is_none(), "{body}");
}

/// Non-object args can't carry a field: the body is forwarded unchanged and
/// the header alone carries the channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_object_args_are_forwarded_unchanged() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(&agent, &[]).await;
    reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .json(&json!(["a", 1]))
        .send()
        .await
        .expect("POST");
    assert_eq!(first_body(&agent), json!(["a", 1]));
    assert_eq!(first_channel(&agent).as_deref(), Some("rest"));
}

// ── #352 step 4: operator-tunable guidance ──────────────────────────────────

/// What a binary expected to refuse boot did: its exit code (`None` if it
/// was still running at the deadline and had to be killed, i.e. it booted)
/// and everything it logged.
struct BootOutcome {
    code: Option<i32>,
    logs: String,
}

/// Start the binary and wait (bounded) for it to exit. A red test must fail,
/// not hang: a binary that wrongly boots is killed at the deadline.
fn boot_and_wait_for_exit(env: &[(&str, String)]) -> BootOutcome {
    use std::io::Read;
    let mut cmd = std::process::Command::new(triton_tests::locate_triton_binary());
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn triton");
    let deadline = Instant::now() + Duration::from_secs(10);
    let code = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut logs = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut logs);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut logs);
    }
    BootOutcome { code, logs }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_guidance_overrides_the_channel_default() {
    let turn = crate::surface_baseline::drive_whatsapp_cloud_with(
        "manifest-surface-guidance-whatsapp.yaml",
        text_surface(),
    )
    .await;
    let s = surface_of(&turn.agent);
    assert_eq!(s["guidance"], "Operator says: two sentences, no lists.");
    assert_eq!(s["buttons"]["max"], 3, "only the guidance is overridden");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_guidance_overrides_the_mcp_default() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(
        &agent,
        &[(
            "TRITON_SURFACE_GUIDANCE_MCP",
            "MCP host: answer in one paragraph.".to_string(),
        )],
    )
    .await;
    reqwest::Client::new()
        .post(format!("http://{}/", triton.mcp_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "assistant", "arguments": { "message": "hi" } }
        }))
        .send()
        .await
        .expect("POST mcp");
    let s = surface_of(&agent);
    assert_eq!(s["channel"], "mcp");
    assert_eq!(s["guidance"], "MCP host: answer in one paragraph.");
}

#[test]
fn over_long_manifest_guidance_refuses_boot() {
    let out = boot_and_wait_for_exit(&[
        ("TRITON_ENV", "local".to_string()),
        (
            "TRITON_MANIFEST_PATH",
            fixture("manifest-surface-guidance-too-long.yaml"),
        ),
    ]);
    assert_eq!(out.code, Some(2), "boot must be refused");
    let logs = out.logs;
    assert!(logs.contains("surface_guidance"), "say why: {logs}");
}

#[test]
fn over_long_env_guidance_refuses_boot() {
    let out = boot_and_wait_for_exit(&[
        ("TRITON_ENV", "local".to_string()),
        ("TRITON_SURFACE_GUIDANCE_REST", "x".repeat(1001)),
    ]);
    assert_eq!(out.code, Some(2), "boot must be refused");
    let logs = out.logs;
    assert!(
        logs.contains("TRITON_SURFACE_GUIDANCE_REST"),
        "say why: {logs}"
    );
}

// ── #352 step 6: discovery for proactive workers ────────────────────────────

/// A background worker composing a proactive push writes the message before
/// any dispatch exists to hint it, so it reads the delivery channel's
/// descriptor from `GET /v1/manifest`. This is also the email test: email
/// never dispatches upstream, so discovery is where its `subject_line` shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_manifest_endpoint_lists_every_channel_surface() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(&agent, &[]).await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(triton.rest_url("/v1/manifest"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/manifest")
        .json()
        .await
        .expect("json");

    assert_eq!(
        body["loaded"], false,
        "listed even with no manifest: {body}"
    );
    let surfaces = &body["surfaces"];
    assert_eq!(surfaces["email"]["subject_line"], true, "{surfaces}");
    assert_eq!(surfaces["whatsapp"]["buttons"]["max"], 3);
    assert_eq!(surfaces["signal"]["buttons"]["supported"], false);
    assert_eq!(surfaces["rest"]["channel"], "rest");
    for (channel, s) in surfaces.as_object().expect("surfaces map") {
        assert_eq!(s["v"], 1, "{channel}");
        assert_eq!(&s["channel"], channel, "keyed by its own channel");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_manifest_endpoint_shows_guidance_overrides() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(
        &agent,
        &[(
            "TRITON_MANIFEST_PATH",
            fixture("manifest-surface-guidance-whatsapp.yaml"),
        )],
    )
    .await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(triton.rest_url("/v1/manifest"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/manifest")
        .json()
        .await
        .expect("json");
    assert_eq!(body["loaded"], true);
    assert_eq!(
        body["surfaces"]["whatsapp"]["guidance"],
        "Operator says: two sentences, no lists."
    );
}
