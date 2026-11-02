//! The official MCP and MCP Apps JSON schemas, vendored as fixtures, as
//! validators for what the real binary answers on a customer path.
//!
//! A host's MCP SDK parses every result against its revision's schema and
//! treats a result it cannot parse as a failed request. Claude and ChatGPT
//! both speak MCP 2026-07-28 (`server/discover`, per-request `_meta`), so a
//! result that is valid in a handshake-era revision can still be refused
//! there: the widget then never opens although its resource was read.
//!
//! * MCP `schema/<revision>/schema.json` from
//!   github.com/modelcontextprotocol/modelcontextprotocol (main at
//!   `d0e246ea`): 2025-06-18 (sha256 `af845e7e…2e01`), 2025-11-25
//!   (`268a5f82…97e7`), 2026-07-28 (`ef70b61f…0203`).
//! * MCP Apps 2026-01-26: `src/generated/schema.json` of
//!   github.com/modelcontextprotocol/ext-apps at `cf599cc4` (package
//!   2.0.4, sha256 `3823abde…33d9`).
//!
//! Every era is driven the way its clients drive it: the handshake eras
//! with `initialize` and the `MCP-Protocol-Version` header, 2026-07-28
//! statelessly with `server/discover` and the `_meta` envelope.

use std::sync::OnceLock;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use triton_core::a2ui::mcp_app;

use crate::mcp_app_widget::{Rig, rig, token};

const MCP_2025_06_18: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/mcp/mcp-2025-06-18.schema.json"
));
const MCP_2025_11_25: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/mcp/mcp-2025-11-25.schema.json"
));
const MCP_2026_07_28: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/mcp/mcp-2026-07-28.schema.json"
));
const MCP_APPS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/mcp/mcp-apps-2026-01-26.schema.json"
));

const SHA256: [(&str, &str); 4] = [
    (
        "2025-06-18",
        "af845e7e5b9d27107d1690f0936022546177a1403e63ffb11470135b296a2e01",
    ),
    (
        "2025-11-25",
        "268a5f82ba70fd7e4b6dc4aa1e64f116f74b4d0edcb69dc046829c79dd4e97e7",
    ),
    (
        "2026-07-28",
        "ef70b61f99b6d2e5e3b46863822eab08dff6a45bedc7a08914e0e5b133f40203",
    ),
    (
        "apps",
        "3823abdec2ab00069420812520c20f561428e1bf328c8f61189094f5160e33d9",
    ),
];

const MODERN: &str = "2026-07-28";

fn source(revision: &str) -> &'static str {
    let text = match revision {
        "2025-06-18" => MCP_2025_06_18,
        "2025-11-25" => MCP_2025_11_25,
        "2026-07-28" => MCP_2026_07_28,
        "apps" => MCP_APPS,
        other => panic!("no vendored schema {other}"),
    };
    let want = SHA256.iter().find(|(r, _)| *r == revision).unwrap().1;
    assert_eq!(
        hex::encode(Sha256::digest(text.as_bytes())),
        want,
        "the vendored {revision} schema"
    );
    text
}

fn schema(revision: &str) -> &'static Value {
    static CELLS: [OnceLock<Value>; 4] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    let k = SHA256.iter().position(|(r, _)| *r == revision).unwrap();
    CELLS[k].get_or_init(|| serde_json::from_str(source(revision)).unwrap())
}

/// Validate `value` as the definition `name` of `revision` (an MCP
/// revision, or `apps` for MCP Apps). Returns the schema errors.
fn errors(revision: &str, name: &str, value: &Value) -> Vec<String> {
    let mut root = schema(revision).clone();
    let (key, draft) = if root.get("$defs").is_some() {
        ("$defs", jsonschema::Draft::Draft202012)
    } else {
        ("definitions", jsonschema::Draft::Draft7)
    };
    assert!(
        root[key].get(name).is_some(),
        "{revision} schema has no {name}"
    );
    root["$ref"] = json!(format!("#/{key}/{name}"));
    let validator = jsonschema::options()
        .with_draft(draft)
        .build(&root)
        .unwrap_or_else(|e| panic!("{revision} schema {name}: {e}"));
    validator
        .iter_errors(value)
        .take(6)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect()
}

/// A Streamable HTTP client of one revision.
struct Client<'a> {
    rig: &'a Rig,
    revision: &'static str,
}

impl Client<'_> {
    fn modern(&self) -> bool {
        self.revision == MODERN
    }

    /// The client capabilities an MCP Apps host announces.
    fn capabilities() -> Value {
        json!({ "extensions": { "io.modelcontextprotocol/ui": {
            "mimeTypes": [mcp_app::MIME_TYPE]
        } } })
    }

    async fn result(&self, method: &str, mut params: Value) -> Value {
        if self.modern() {
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": MODERN,
                "io.modelcontextprotocol/clientInfo": { "name": "schema-test", "version": "1.0.0" },
                "io.modelcontextprotocol/clientCapabilities": Self::capabilities(),
            });
        }
        let mut req = reqwest::Client::new()
            .post(self.rig.triton.rest_url("/mcp/acme"))
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", self.revision)
            .bearer_auth(token(&self.rig.entra))
            .json(&json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }));
        if self.modern() {
            req = req.header("mcp-method", method);
        }
        let resp = req.send().await.expect("POST /mcp/acme");
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "{method}");
        let v: Value = resp.json().await.expect("json");
        assert_eq!(v["jsonrpc"], "2.0", "{v}");
        assert_eq!(v["id"], 7, "{v}");
        assert!(v.get("error").is_none(), "{} {method}: {v}", self.revision);
        v["result"].clone()
    }
}

/// Every result a host needs to show the answer card, per revision, is
/// valid against that revision's official schema, and the MCP Apps parts
/// (tool `_meta.ui`, resource `_meta.ui`, the read content) against the
/// MCP Apps schema.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_era_gets_schema_valid_results_for_the_answer_card() {
    let r = rig().await;
    let uri = mcp_app::resource_uri();
    let mut failures: Vec<String> = Vec::new();
    let mut check = |revision: &str, name: &str, value: &Value| {
        let errs = errors(revision, name, value);
        if !errs.is_empty() {
            failures.push(format!("{revision} {name}: {errs:#?}"));
        }
    };

    for revision in ["2025-06-18", "2025-11-25", MODERN] {
        let c = Client { rig: &r, revision };
        if c.modern() {
            let d = c.result("server/discover", json!({})).await;
            check(revision, "DiscoverResult", &d);
        } else {
            let init = c
                .result(
                    "initialize",
                    json!({
                        "protocolVersion": revision,
                        "capabilities": Client::capabilities(),
                        "clientInfo": { "name": "schema-test", "version": "1.0.0" }
                    }),
                )
                .await;
            check(revision, "InitializeResult", &init);
        }

        let tools = c.result("tools/list", json!({})).await;
        check(revision, "ListToolsResult", &tools);
        let card_tool = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "list_agents")
            .unwrap_or_else(|| panic!("{revision}: no list_agents: {tools}"))
            .clone();
        check("apps", "McpUiToolMeta", &card_tool["_meta"]["ui"]);

        let list = c.result("resources/list", json!({})).await;
        check(revision, "ListResourcesResult", &list);
        let entry = list["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["uri"] == uri)
            .unwrap_or_else(|| panic!("{revision}: {uri} not listed: {list}"))
            .clone();
        check("apps", "McpUiResourceMeta", &entry["_meta"]["ui"]);

        let read = c.result("resources/read", json!({ "uri": uri })).await;
        check(revision, "ReadResourceResult", &read);
        let content = &read["contents"][0];
        assert_eq!(content["mimeType"], mcp_app::MIME_TYPE, "{revision}");
        check("apps", "McpUiResourceMeta", &content["_meta"]["ui"]);

        let call = c
            .result(
                "tools/call",
                json!({ "name": "list_agents", "arguments": {} }),
            )
            .await;
        check(revision, "CallToolResult", &call);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The validators reject what they should: a 2026-07-28 resource read
/// without its cache hints, and a tool `_meta.ui` naming a non-string URI.
#[test]
fn the_vendored_validators_reject_invalid_results() {
    let read = json!({
        "resultType": "complete",
        "contents": [{ "uri": "ui://x/y.html", "mimeType": "text/html", "text": "<p>" }]
    });
    assert!(errors("2025-11-25", "ReadResourceResult", &read).is_empty());
    assert!(!errors(MODERN, "ReadResourceResult", &read).is_empty());
    assert!(!errors("apps", "McpUiToolMeta", &json!({ "resourceUri": 3 })).is_empty());
}
