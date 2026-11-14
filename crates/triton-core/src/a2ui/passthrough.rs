//! Producer-built A2UI v0.9 surfaces (`surface.a2ui`, #370).
//!
//! An upstream may put native v0.9 surfaces beside its canonical
//! `components`. They are vetted **once**, by the dispatcher, when the
//! upstream answer arrives ([`vet_result`]): each surface must be an object
//! with a flat `components` list holding a `root`, must name a catalog
//! Triton vendors (`catalogId`, default [`Catalog::GeComposite`] for the
//! producers written before the field existed), and every component must
//! validate against that catalog's JSON schema. A surface that fails is
//! dropped (the others and Triton's own card still render) and reported as a
//! [`Rejection`]: schema path and component id, never a value.
//!
//! The faces then only turn the vetted list into messages ([`messages`]):
//! Gemini Enterprise takes every catalog, the MCP Apps widget the basic one.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{Value, json};

/// The a2ui.org v0.9 basic catalog (what `@a2ui/lit` renders).
pub const BASIC_CATALOG_ID: &str =
    "https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json";
/// Gemini Enterprise's composite catalog (Material components + basic).
pub const GE_COMPOSITE_CATALOG_ID: &str =
    "https://www.gstatic.com/vertexaisearch/a2ui/v0_9/gemini_enterprise_composite_catalog.json";

/// Upper bound on producer surfaces in one answer.
pub const MAX_SURFACES: usize = 8;
const MAX_DISPLAY_NAME_CHARS: usize = 64;
/// Producer-built surfaces get their own id prefix.
pub const SURFACE_PREFIX: &str = "triton-a2ui-";
const DEFAULT_PRIMARY_COLOR: &str = "#1a73e8";
const DEFAULT_DISPLAY_NAME: &str = "DataZoo Agent";

/// A catalog Triton vendors the schema of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalog {
    Basic,
    GeComposite,
}

impl Catalog {
    pub fn id(self) -> &'static str {
        match self {
            Catalog::Basic => BASIC_CATALOG_ID,
            Catalog::GeComposite => GE_COMPOSITE_CATALOG_ID,
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        [Catalog::Basic, Catalog::GeComposite]
            .into_iter()
            .find(|c| c.id() == id)
    }
}

/// Why a surface was dropped. The label is a closed set (metric-safe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Not an object, or no `components` list.
    Shape,
    /// Beyond [`MAX_SURFACES`].
    OverCap,
    /// No component with id `root`.
    NoRoot,
    /// `catalogId` names no vendored catalog.
    Catalog,
    /// A component fails its catalog schema.
    Schema,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Shape => "shape",
            DropReason::OverCap => "over_cap",
            DropReason::NoRoot => "no_root",
            DropReason::Catalog => "catalog",
            DropReason::Schema => "schema",
        }
    }
}

/// One dropped surface. Carries locations only, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Position in the producer's `surface.a2ui` list.
    pub surface_index: usize,
    pub reason: DropReason,
    /// JSON pointer into the catalog schema (`/components/Button/...`).
    pub schema_path: Option<String>,
    /// The failing component's id (ids are structural, not user data).
    pub component_id: Option<String>,
}

impl Rejection {
    fn new(surface_index: usize, reason: DropReason) -> Self {
        Self {
            surface_index,
            reason,
            schema_path: None,
            component_id: None,
        }
    }
}

/// Vet the producer surfaces of an upstream answer in place: keep the valid
/// ones (normalized to `{catalogId, components, data_model?, theme?}`) and
/// return why the others were dropped. An answer without `surface.a2ui` is
/// left untouched; one whose surfaces all fail loses the key.
pub fn vet_result(result: &mut Value) -> Vec<Rejection> {
    let Some(surface) = result.get_mut("surface").and_then(Value::as_object_mut) else {
        return Vec::new();
    };
    let Some(raw) = surface.remove("a2ui") else {
        return Vec::new();
    };
    let (kept, rejections) = vet(&raw);
    if !kept.is_empty() {
        surface.insert("a2ui".into(), Value::Array(kept));
    }
    rejections
}

/// Vet a raw `surface.a2ui` value (see [`vet_result`]).
pub fn vet(raw: &Value) -> (Vec<Value>, Vec<Rejection>) {
    let Some(list) = raw.as_array() else {
        return (Vec::new(), vec![Rejection::new(0, DropReason::Shape)]);
    };
    let mut kept = Vec::new();
    let mut rejections = Vec::new();
    for (i, s) in list.iter().enumerate() {
        if i >= MAX_SURFACES {
            rejections.push(Rejection::new(i, DropReason::OverCap));
            continue;
        }
        match vet_one(s) {
            Ok(v) => kept.push(v),
            Err(mut r) => {
                r.surface_index = i;
                rejections.push(r);
            }
        }
    }
    (kept, rejections)
}

/// The declared catalog; none declared means the GE composite catalog (the
/// only one before `catalogId` existed). `None` for an unknown id.
fn catalog_of(s: &Value) -> Option<Catalog> {
    match s.get("catalogId") {
        None | Some(Value::Null) => Some(Catalog::GeComposite),
        Some(v) => v.as_str().and_then(Catalog::from_id),
    }
}

fn vet_one(s: &Value) -> Result<Value, Rejection> {
    let components = s
        .get("components")
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty())
        .ok_or(Rejection::new(0, DropReason::Shape))?;
    let catalog = catalog_of(s).ok_or(Rejection::new(0, DropReason::Catalog))?;
    if !components
        .iter()
        .any(|c| c.get("id").and_then(Value::as_str) == Some("root"))
    {
        return Err(Rejection::new(0, DropReason::NoRoot));
    }
    for c in components {
        if let Some(schema_path) = schemas().check(catalog, c) {
            return Err(Rejection {
                surface_index: 0,
                reason: DropReason::Schema,
                schema_path: Some(schema_path),
                component_id: c.get("id").and_then(Value::as_str).map(String::from),
            });
        }
    }
    let mut out = json!({ "catalogId": catalog.id(), "components": components });
    if let Some(m) = s.get("data_model").filter(|m| !m.is_null()) {
        if !m.is_object() {
            return Err(Rejection::new(0, DropReason::Shape));
        }
        out["data_model"] = m.clone();
    }
    if let Some(t) = s.get("theme").filter(|t| t.is_object()) {
        out["theme"] = t.clone();
    }
    Ok(out)
}

/// The v0.9 messages for already-vetted surfaces whose catalog `accept`s:
/// `createSurface` + `updateComponents` (+ `updateDataModel`) each.
/// `catalog_for` maps the surface's catalog to the id declared on the face
/// (Gemini Enterprise only knows its composite catalog, a superset of basic).
pub fn messages(
    vetted: Option<&Value>,
    accept: impl Fn(Catalog) -> bool,
    catalog_for: impl Fn(Catalog) -> &'static str,
) -> Vec<Value> {
    let Some(list) = vetted.and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|s| {
            let catalog = catalog_of(s)?;
            accept(catalog).then(|| surface_messages(s, catalog_for(catalog)))
        })
        .flatten()
        .collect()
}

fn surface_messages(s: &Value, catalog_id: &str) -> Vec<Value> {
    let surface_id = format!("{SURFACE_PREFIX}{}", uuid::Uuid::new_v4());
    let mut out = vec![
        json!({ "version": "v0.9", "createSurface": {
            "surfaceId": surface_id,
            "catalogId": catalog_id,
            "theme": theme(s.get("theme")),
        }}),
        json!({ "version": "v0.9", "updateComponents": {
            "surfaceId": surface_id, "components": s["components"],
        }}),
    ];
    if let Some(model) = s.get("data_model") {
        out.push(json!({ "version": "v0.9", "updateDataModel": {
            "surfaceId": surface_id, "path": "/", "value": model,
        }}));
    }
    out
}

/// Triton's surface theme with the producer's `agentDisplayName` and
/// `primaryColor` applied when they are well-formed; other keys are ignored.
fn theme(spec: Option<&Value>) -> Value {
    let mut theme =
        json!({ "primaryColor": DEFAULT_PRIMARY_COLOR, "agentDisplayName": DEFAULT_DISPLAY_NAME });
    let Some(over) = spec.and_then(Value::as_object) else {
        return theme;
    };
    if let Some(name) = over
        .get("agentDisplayName")
        .and_then(Value::as_str)
        .map(str::trim)
        && !name.is_empty()
        && name.chars().count() <= MAX_DISPLAY_NAME_CHARS
        && !name.chars().any(char::is_control)
    {
        theme["agentDisplayName"] = json!(name);
    }
    if let Some(color) = over.get("primaryColor").and_then(Value::as_str)
        && color.len() == 7
        && color.starts_with('#')
        && color[1..].chars().all(|c| c.is_ascii_hexdigit())
    {
        theme["primaryColor"] = json!(color);
    }
    theme
}

/// Compile the vendored schemas now (the binary calls this off the boot
/// path); otherwise the first vetted answer compiles them.
pub fn warm() {
    LazyLock::force(&SCHEMAS);
}

// ---------------------------------------------------------------------------
// Vendored schemas (crates/triton-core/schemas/a2ui-v0_9, provenance in the
// README there), compiled once: one validator per catalog component.

const COMMON_TYPES: &str = include_str!("../../schemas/a2ui-v0_9/common_types.json");
const BASIC_CATALOG: &str = include_str!("../../schemas/a2ui-v0_9/basic_catalog.json");
const GE_CATALOG: &str =
    include_str!("../../schemas/a2ui-v0_9/gemini_enterprise_composite_catalog.json");
const COMMON_TYPES_ID: &str = "https://a2ui.org/specification/v0_9/common_types.json";
const CATALOG_ALIAS: &str = "https://a2ui.org/specification/v0_9/catalog.json";

static SCHEMAS: LazyLock<Schemas> = LazyLock::new(Schemas::compile);

fn schemas() -> &'static Schemas {
    &SCHEMAS
}

struct Schemas {
    basic: HashMap<String, jsonschema::Validator>,
    ge: HashMap<String, jsonschema::Validator>,
}

impl Schemas {
    fn compile() -> Self {
        let parse = |s: &str| -> Value {
            serde_json::from_str(s).expect("vendored A2UI schema is valid JSON")
        };
        let common = parse(COMMON_TYPES);
        Self {
            basic: compile_catalog(parse(BASIC_CATALOG), BASIC_CATALOG_ID, &common),
            ge: compile_catalog(parse(GE_CATALOG), GE_COMPOSITE_CATALOG_ID, &common),
        }
    }

    /// `None` when `component` is valid in `catalog`, else the schema path
    /// of the first failure.
    fn check(&self, catalog: Catalog, component: &Value) -> Option<String> {
        let validators = match catalog {
            Catalog::Basic => &self.basic,
            Catalog::GeComposite => &self.ge,
        };
        let Some(name) = component.get("component").and_then(Value::as_str) else {
            return Some("/$defs/CatalogComponentCommon/required".to_string());
        };
        let Some(v) = validators.get(name) else {
            return Some("/components".to_string());
        };
        v.iter_errors(component)
            .next()
            .map(|e| format!("/components/{name}{}", e.schema_path()))
    }
}

/// One validator per component of `catalog`. `common_types.json` refers to
/// the active catalog as the relative `catalog.json`, so that alias is
/// registered too (as the A2UI spec's own validator does).
fn compile_catalog(
    catalog: Value,
    id: &str,
    common: &Value,
) -> HashMap<String, jsonschema::Validator> {
    let mut alias = catalog.clone();
    alias["$id"] = json!(CATALOG_ALIAS);
    let resources = HashMap::from([
        (COMMON_TYPES_ID.to_string(), common.clone()),
        (CATALOG_ALIAS.to_string(), alias),
        (id.to_string(), catalog.clone()),
    ]);
    let names = catalog["components"]
        .as_object()
        .expect("catalog has components")
        .keys();
    names
        .map(|name| {
            let root = json!({ "$ref": format!("{id}#/components/{name}") });
            let v = jsonschema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .should_validate_formats(true)
                .with_retriever(Vendored(resources.clone()))
                .build(&root)
                .unwrap_or_else(|e| panic!("compile A2UI catalog {id} component {name}: {e}"));
            (name.clone(), v)
        })
        .collect()
}

/// Resolves the vendored schemas by URI; nothing is fetched.
struct Vendored(HashMap<String, Value>);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(components: Value) -> Value {
        json!({ "catalogId": BASIC_CATALOG_ID, "components": components })
    }

    #[test]
    fn a_valid_basic_surface_is_kept_and_normalized() {
        let raw = json!([basic(json!([
            { "id": "root", "component": "Column", "children": ["t"] },
            { "id": "t", "component": "Text", "text": "hi" }
        ]))]);
        let (kept, rej) = vet(&raw);
        assert!(rej.is_empty(), "{rej:?}");
        assert_eq!(kept[0]["catalogId"], BASIC_CATALOG_ID);
    }

    #[test]
    fn a_missing_catalog_id_means_the_ge_composite_catalog() {
        let raw = json!([{ "components": [
            { "id": "root", "component": "MaterialCard", "children": [] }
        ]}]);
        let (kept, rej) = vet(&raw);
        assert!(rej.is_empty(), "{rej:?}");
        assert_eq!(kept[0]["catalogId"], GE_COMPOSITE_CATALOG_ID);
    }

    #[test]
    fn a_material_component_is_not_basic() {
        let raw = json!([basic(json!([
            { "id": "root", "component": "MaterialCard", "children": [] }
        ]))]);
        let (_, rej) = vet(&raw);
        assert_eq!(rej[0].reason, DropReason::Schema);
        assert_eq!(rej[0].schema_path.as_deref(), Some("/components"));
        assert_eq!(rej[0].component_id.as_deref(), Some("root"));
    }

    #[test]
    fn a_missing_required_property_names_the_component() {
        let raw = json!([basic(json!([
            { "id": "root", "component": "Column", "children": ["b"] },
            { "id": "b", "component": "Button", "child": "root" }
        ]))]);
        let (kept, rej) = vet(&raw);
        assert!(kept.is_empty());
        assert_eq!(rej[0].component_id.as_deref(), Some("b"));
        let path = rej[0].schema_path.as_deref().unwrap();
        assert!(path.starts_with("/components/Button"), "{path}");
    }

    #[test]
    fn shape_root_catalog_and_cap_are_reported() {
        let ok = basic(json!([{ "id": "root", "component": "Text", "text": "x" }]));
        let mut raw = vec![
            json!("nope"),
            basic(json!([{ "id": "a", "component": "Text", "text": "x" }])),
            json!({ "catalogId": "https://x.test/c.json", "components": [ { "id": "root" } ] }),
        ];
        raw.extend(std::iter::repeat_n(ok, 6));
        let (kept, rej) = vet(&Value::Array(raw));
        let reasons: Vec<_> = rej.iter().map(|r| (r.surface_index, r.reason)).collect();
        assert_eq!(
            reasons,
            vec![
                (0, DropReason::Shape),
                (1, DropReason::NoRoot),
                (2, DropReason::Catalog),
                (8, DropReason::OverCap)
            ]
        );
        assert_eq!(kept.len(), 5);
    }

    #[test]
    fn vet_result_rewrites_or_removes_the_key() {
        let mut all_bad = json!({ "surface": { "components": [], "a2ui": [ "x" ] } });
        assert_eq!(vet_result(&mut all_bad).len(), 1);
        assert!(all_bad["surface"].get("a2ui").is_none());

        let mut none = json!({ "surface": { "components": [] } });
        assert!(vet_result(&mut none).is_empty());
        assert_eq!(none, json!({ "surface": { "components": [] } }));
    }

    #[test]
    fn messages_follow_the_accepted_catalogs() {
        let vetted = json!([
            { "catalogId": BASIC_CATALOG_ID, "components": [ { "id": "root" } ] },
            { "catalogId": GE_COMPOSITE_CATALOG_ID, "components": [ { "id": "root" } ],
              "data_model": { "a": 1 }, "theme": { "primaryColor": "#00ff00", "x": 1 } }
        ]);
        let basic_only = messages(Some(&vetted), |c| c == Catalog::Basic, Catalog::id);
        assert_eq!(basic_only.len(), 2);
        assert_eq!(
            basic_only[0]["createSurface"]["catalogId"],
            BASIC_CATALOG_ID
        );

        let ge = messages(Some(&vetted), |_| true, |_| GE_COMPOSITE_CATALOG_ID);
        assert_eq!(ge.len(), 5);
        assert!(ge.iter().all(|m| {
            m["createSurface"]["catalogId"]
                .as_str()
                .is_none_or(|id| id == GE_COMPOSITE_CATALOG_ID)
        }));
        assert_eq!(ge[2]["createSurface"]["theme"]["primaryColor"], "#00ff00");
        assert!(ge[2]["createSurface"]["theme"].get("x").is_none());
        assert_eq!(ge[4]["updateDataModel"]["value"]["a"], 1);
    }
}
