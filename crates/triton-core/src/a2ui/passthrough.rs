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
    /// A control fires a `triton.*` event: those are Triton's own card's,
    /// which the MCP widget sends as user turns after Triton's checks.
    ReservedEvent,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Shape => "shape",
            DropReason::OverCap => "over_cap",
            DropReason::NoRoot => "no_root",
            DropReason::Catalog => "catalog",
            DropReason::Schema => "schema",
            DropReason::ReservedEvent => "reserved_event",
        }
    }
}

/// One dropped surface (or, for [`DropReason::OverCap`], every surface past
/// the cap). Carries locations only, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Position in the producer's `surface.a2ui` list (the first one past
    /// the cap for `OverCap`).
    pub surface_index: usize,
    pub reason: DropReason,
    /// Surfaces this rejection stands for: 1, or the excess for `OverCap`.
    pub count: usize,
    /// JSON pointer into the catalog schema (`/components/Button/...`).
    pub schema_path: Option<String>,
    /// Position of the failing component in the surface's `components`.
    pub component_index: Option<usize>,
    /// The failing component's id, if it is a plain identifier (see
    /// [`loggable_id`]); a producer id is still producer text.
    pub component_id: Option<String>,
}

impl Rejection {
    fn new(reason: DropReason) -> Self {
        Self {
            surface_index: 0,
            reason,
            count: 1,
            schema_path: None,
            component_index: None,
            component_id: None,
        }
    }
}

/// Longest component id that is logged as is.
const MAX_LOGGED_ID: usize = 64;

/// A component id fit for a log line: a plain identifier (letters, digits,
/// `_ . : -`) of at most [`MAX_LOGGED_ID`] chars, else a placeholder, so an
/// id never smuggles user data or log noise into the operator's logs.
fn loggable_id(id: Option<&str>) -> Option<String> {
    let id = id?;
    let plain = !id.is_empty()
        && id.len() <= MAX_LOGGED_ID
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'));
    Some(if plain {
        id.to_string()
    } else {
        "<unprintable>".to_string()
    })
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

/// Vet a raw `surface.a2ui` value (see [`vet_result`]). Surfaces past
/// [`MAX_SURFACES`] are not looked at: they are one `OverCap` rejection.
pub fn vet(raw: &Value) -> (Vec<Value>, Vec<Rejection>) {
    let Some(list) = raw.as_array() else {
        return (Vec::new(), vec![Rejection::new(DropReason::Shape)]);
    };
    let mut kept = Vec::new();
    let mut rejections = Vec::new();
    for (i, s) in list.iter().enumerate().take(MAX_SURFACES) {
        match vet_one(s) {
            Ok(v) => kept.push(v),
            Err(r) => rejections.push(Rejection {
                surface_index: i,
                ..r
            }),
        }
    }
    if list.len() > MAX_SURFACES {
        rejections.push(Rejection {
            surface_index: MAX_SURFACES,
            count: list.len() - MAX_SURFACES,
            ..Rejection::new(DropReason::OverCap)
        });
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

fn has_root(components: &[Value]) -> bool {
    components
        .iter()
        .any(|c| c.get("id").and_then(Value::as_str) == Some("root"))
}

/// The event-name prefix Triton reserves for its own card's controls.
pub const RESERVED_EVENT_PREFIX: &str = "triton.";

/// Whether any action under `v` fires a `triton.*` event.
fn fires_reserved_event(v: &Value) -> bool {
    match v {
        Value::Object(m) => {
            m.get("event")
                .and_then(|e| e.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|n| n.starts_with(RESERVED_EVENT_PREFIX))
                || m.values().any(fires_reserved_event)
        }
        Value::Array(a) => a.iter().any(fires_reserved_event),
        _ => false,
    }
}

fn vet_one(s: &Value) -> Result<Value, Rejection> {
    let components = s
        .get("components")
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty())
        .ok_or(Rejection::new(DropReason::Shape))?;
    let catalog = catalog_of(s).ok_or(Rejection::new(DropReason::Catalog))?;
    if !has_root(components) {
        return Err(Rejection::new(DropReason::NoRoot));
    }
    for (i, c) in components.iter().enumerate() {
        if let Some(schema_path) = schemas().check(catalog, c) {
            return Err(Rejection {
                schema_path: Some(schema_path),
                component_index: Some(i),
                component_id: loggable_id(c.get("id").and_then(Value::as_str)),
                ..Rejection::new(DropReason::Schema)
            });
        }
    }
    if let Some((i, c)) = components
        .iter()
        .enumerate()
        .find(|(_, c)| fires_reserved_event(c))
    {
        return Err(Rejection {
            component_index: Some(i),
            component_id: loggable_id(c.get("id").and_then(Value::as_str)),
            ..Rejection::new(DropReason::ReservedEvent)
        });
    }
    // `updateDataModel.value` may be any JSON value in v0.9.
    let mut out = json!({ "catalogId": catalog.id(), "components": components });
    if let Some(m) = s.get("data_model").filter(|m| !m.is_null()) {
        out["data_model"] = m.clone();
    }
    if let Some(t) = s.get("theme").filter(|t| t.is_object()) {
        out["theme"] = t.clone();
    }
    Ok(out)
}

/// The v0.9 messages for vetted surfaces written for one of `catalogs`:
/// `createSurface` (declaring `catalog_id`, the catalog the face's renderer
/// knows) + `updateComponents` (+ `updateDataModel`) each. A surface with no
/// `root` or an unknown catalog is skipped, so an unvetted value (a task
/// record stored before vetting existed) can still never go out broken.
pub fn messages(vetted: Option<&Value>, catalogs: &[Catalog], catalog_id: &str) -> Vec<Value> {
    let Some(list) = vetted.and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter(|s| {
            catalog_of(s).is_some_and(|c| catalogs.contains(&c))
                && s.get("components")
                    .and_then(Value::as_array)
                    .is_some_and(|c| has_root(c))
        })
        .flat_map(|s| surface_messages(s, catalog_id))
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
        raw.extend(std::iter::repeat_n(ok, 97));
        let (kept, rej) = vet(&Value::Array(raw));
        let reasons: Vec<_> = rej
            .iter()
            .map(|r| (r.surface_index, r.reason, r.count))
            .collect();
        assert_eq!(
            reasons,
            vec![
                (0, DropReason::Shape, 1),
                (1, DropReason::NoRoot, 1),
                (2, DropReason::Catalog, 1),
                (8, DropReason::OverCap, 92)
            ],
            "everything past the cap is ONE rejection"
        );
        assert_eq!(kept.len(), 5);
    }

    #[test]
    fn malformed_surfaces_and_components_are_shape_or_schema() {
        let cases = [
            (json!({ "components": [] }), DropReason::Shape),
            (json!({ "components": "x" }), DropReason::Shape),
            (
                json!({ "catalogId": 7, "components": [ { "id": "root" } ] }),
                DropReason::Catalog,
            ),
            (basic(json!([ { "id": "root" } ])), DropReason::Schema),
            (
                basic(json!([ { "id": "root", "component": "Text", "text": "x" }, "str" ])),
                DropReason::Schema,
            ),
        ];
        for (surface, want) in cases {
            let (kept, rej) = vet(&json!([surface.clone()]));
            assert!(kept.is_empty(), "{surface}");
            assert_eq!(rej[0].reason, want, "{surface}");
        }
    }

    #[test]
    fn any_json_data_model_is_kept() {
        for model in [json!([1, 2]), json!("s"), json!(3), json!({ "a": 1 })] {
            let raw = json!([{ "catalogId": BASIC_CATALOG_ID, "data_model": model.clone(),
                "components": [ { "id": "root", "component": "Text", "text": "x" } ] }]);
            let (kept, rej) = vet(&raw);
            assert!(rej.is_empty(), "{model}: {rej:?}");
            let msgs = messages(
                Some(&Value::Array(kept)),
                &[Catalog::Basic],
                BASIC_CATALOG_ID,
            );
            assert_eq!(msgs[2]["updateDataModel"]["value"], model);
        }
    }

    #[test]
    fn only_plain_component_ids_are_logged() {
        let id = |raw: &str| {
            let (_, rej) = vet(&json!([basic(json!([
                { "id": "root", "component": "Column", "children": [] },
                { "id": raw, "component": "NoSuchWidget" }
            ]))]));
            (rej[0].component_index, rej[0].component_id.clone())
        };
        assert_eq!(id("row-1.b:2_x"), (Some(1), Some("row-1.b:2_x".into())));
        for odd in ["jane@example.com", "a b", &"x".repeat(65), "line\nbreak"] {
            assert_eq!(id(odd), (Some(1), Some("<unprintable>".into())), "{odd}");
        }
    }

    /// `triton.*` events are Triton's own card's (the widget sends them as
    /// user turns after Triton's checks): a producer surface firing one is
    /// dropped, wherever the action sits.
    #[test]
    fn a_producer_cannot_fire_tritons_own_events() {
        for name in ["triton.send_message", "triton.open_url", "triton.anything"] {
            let raw = json!([basic(json!([
                { "id": "root", "component": "Column", "children": ["b"] },
                { "id": "bl", "component": "Text", "text": "Go" },
                { "id": "b", "component": "Button", "child": "bl",
                  "action": { "event": { "name": name, "context": { "text": "x" } } } }
            ]))]);
            let (kept, rej) = vet(&raw);
            assert!(kept.is_empty(), "{name}");
            assert_eq!(rej[0].reason, DropReason::ReservedEvent, "{name}");
            assert_eq!(rej[0].component_id.as_deref(), Some("b"));
        }
        let own = json!([basic(json!([
            { "id": "root", "component": "Column", "children": ["b"] },
            { "id": "bl", "component": "Text", "text": "Go" },
            { "id": "b", "component": "Button", "child": "bl",
              "action": { "event": { "name": "tritonic.apply" } } }
        ]))]);
        assert!(
            vet(&own).1.is_empty(),
            "only the `triton.` prefix is reserved"
        );
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
    fn messages_declare_the_face_catalog_and_skip_unrenderable_surfaces() {
        let vetted = json!([
            { "catalogId": BASIC_CATALOG_ID, "components": [ { "id": "root" } ] },
            { "components": [ { "id": "root" } ],
              "data_model": { "a": 1 }, "theme": { "primaryColor": "#00ff00", "x": 1 } },
            { "components": [ { "id": "no-root" } ] },
            { "catalogId": "https://x.test/c.json", "components": [ { "id": "root" } ] }
        ]);
        let ge = messages(
            Some(&vetted),
            &[Catalog::Basic, Catalog::GeComposite],
            GE_COMPOSITE_CATALOG_ID,
        );
        assert_eq!(
            ge.len(),
            5,
            "two surfaces; no-root and unknown catalog skipped"
        );
        let basic_only = messages(Some(&vetted), &[Catalog::Basic], BASIC_CATALOG_ID);
        assert_eq!(basic_only.len(), 2, "only the basic surface");
        assert_eq!(
            basic_only[0]["createSurface"]["catalogId"],
            BASIC_CATALOG_ID
        );
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
