//! The official A2A JSON schemas, vendored as fixtures, as validators for
//! the A2A integration tests.
//!
//! * A2A 0.3.0 — `specification/json/a2a.json` at tag v0.3.0 of
//!   github.com/a2aproject/A2A (sha256 `97d6e243…5f17`).
//! * A2A 1.0.0 — the published JSON schema generated from `a2a.proto`,
//!   <https://a2a-protocol.org/v1.0.0/spec/a2a.json> (sha256 `6b6560c7…8d62`).
//!
//! The 0.3.0 schema allows unknown keys, and Gemini Enterprise refused a
//! card it could not parse, so [`check03`] also validates a STRICT variant:
//! every definition that declares properties and says nothing about other
//! keys gets `additionalProperties: false` — the object may use only keys
//! the 0.3.0 schema names. The 1.0.0 definitions are strict already; the
//! bundle refers to its definitions by file name
//! (`lf.a2a.v1.Task.jsonschema.json`), which a retriever resolves here.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const V03: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/a2a/a2a-v0.3.0.schema.json"
));
const V10: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/a2a/a2a-v1.0.0.schema.json"
));
const V03_SHA256: &str = "97d6e2435336836cd1d41dffacf83a1a97902b62b826ef14ec5704db85c95f17";
const V10_SHA256: &str = "6b6560c726289734799b7d5883be84e4cc0452600736db0f811341bac43b8d62";
const BASE: &str = "https://a2a.schema.test/";

fn sha256(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn errors(validator: &jsonschema::Validator, value: &Value) -> Vec<String> {
    validator
        .iter_errors(value)
        .take(6)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect()
}

fn v03(strict: bool) -> &'static Value {
    static PLAIN: OnceLock<Value> = OnceLock::new();
    static STRICT: OnceLock<Value> = OnceLock::new();
    let cell = if strict { &STRICT } else { &PLAIN };
    cell.get_or_init(|| {
        assert_eq!(sha256(V03), V03_SHA256, "the vendored 0.3.0 schema");
        let mut schema: Value = serde_json::from_str(V03).unwrap();
        if strict && let Some(defs) = schema["definitions"].as_object_mut() {
            for def in defs.values_mut() {
                if def.get("properties").is_some() && def.get("additionalProperties").is_none() {
                    def["additionalProperties"] = json!(false);
                }
            }
        }
        schema
    })
}

/// Validate `value` as the 0.3.0 definition `name`, against the official
/// schema and its strict variant. Panics with the schema errors.
pub fn check03(name: &str, value: &Value) {
    for strict in [false, true] {
        let mut root = v03(strict).clone();
        root["$ref"] = json!(format!("#/definitions/{name}"));
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft7)
            .build(&root)
            .unwrap_or_else(|e| panic!("0.3.0 schema {name}: {e}"));
        let errs = errors(&validator, value);
        assert!(
            errs.is_empty(),
            "{name} (A2A 0.3.0 schema{}): {errs:#?}\nvalue: {value:#}",
            if strict { ", keys it names only" } else { "" }
        );
    }
}

fn v10_definitions() -> &'static HashMap<String, Value> {
    static DEFS: OnceLock<HashMap<String, Value>> = OnceLock::new();
    DEFS.get_or_init(|| {
        assert_eq!(sha256(V10), V10_SHA256, "the vendored 1.0.0 schema");
        let schema: Value = serde_json::from_str(V10).unwrap();
        let mut out = HashMap::new();
        for (title, def) in schema["definitions"].as_object().unwrap() {
            let name = title.replace(' ', "");
            let file = if ["Struct", "Timestamp", "Value"].contains(&name.as_str()) {
                format!("google.protobuf.{name}.jsonschema.json")
            } else {
                format!("lf.a2a.v1.{name}.jsonschema.json")
            };
            let mut def = def.clone();
            def["$id"] = json!(format!("{BASE}{file}"));
            out.insert(file, def);
        }
        out
    })
}

struct V10Retriever;

impl jsonschema::Retrieve for V10Retriever {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let file = uri.as_str().rsplit('/').next().unwrap_or_default();
        v10_definitions()
            .get(file)
            .cloned()
            .ok_or_else(|| format!("no 1.0.0 definition `{file}`").into())
    }
}

/// Validate `value` as the 1.0.0 definition `name` (its proto name, e.g.
/// `AgentCard`, `Task`, `StreamResponse`). Panics with the schema errors.
pub fn check10(name: &str, value: &Value) {
    let file = format!("lf.a2a.v1.{name}.jsonschema.json");
    let schema = v10_definitions()
        .get(&file)
        .unwrap_or_else(|| panic!("no 1.0.0 definition {name}"))
        .clone();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .with_retriever(V10Retriever)
        .build(&schema)
        .unwrap_or_else(|e| panic!("1.0.0 schema {name}: {e}"));
    let errs = errors(&validator, value);
    assert!(
        errs.is_empty(),
        "{name} (A2A 1.0.0 schema): {errs:#?}\nvalue: {value:#}"
    );
}

/// The validators reject what they should: an unknown key under the strict
/// 0.3 variant and under 1.0, a wrong 1.0 state name.
#[test]
fn the_validators_bite() {
    let task03 = json!({"kind":"task","id":"t","contextId":"c","status":{"state":"working"}});
    check03("Task", &task03);
    let mut extra = task03.clone();
    extra["surprise"] = json!(1);
    assert!(std::panic::catch_unwind(|| check03("Task", &extra)).is_err());
    let task10 = json!({"id":"t","contextId":"c","status":{"state":"TASK_STATE_WORKING"}});
    check10("Task", &task10);
    let wrong = json!({"id":"t","contextId":"c","status":{"state":"working"}});
    assert!(std::panic::catch_unwind(|| check10("Task", &wrong)).is_err());
    let mut extra10 = task10.clone();
    extra10["kind"] = json!("task");
    assert!(std::panic::catch_unwind(|| check10("Task", &extra10)).is_err());
}
