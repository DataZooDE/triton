//! The neutral chart summary an upstream attaches at `_meta.chart` (#375).
//!
//! A renderer-independent description of a tool result's primary chart:
//! the chart kind, which row fields carry the category, the value and the
//! optional series, and the (capped) rows themselves. Chat adapters map it
//! to their native chart element (Teams `Chart.*`); the A2A face derives the
//! Vega-Lite spec Gemini Enterprise's `VegaChart` draws from it
//! ([`ChartSummary::to_vega_lite`]). Upstreams no longer need to emit
//! Vega-Lite only so Triton can read it back.
//!
//! The wire contract lives in `doc/upstream-agent-contract.md` ("Chart
//! summary"). Parsing is strict: a summary that breaks a rule is rejected
//! as a whole, and the caller falls back to the PNG image (the deprecated
//! `_meta.vega_specs` is no longer read, #382). Unknown keys are ignored so
//! later additions stay compatible.

use serde_json::{Map, Value};

/// The `_meta` key the summary rides under.
pub const META_KEY: &str = "chart";

/// Most rows a summary may carry. A chat card has a byte budget (Teams
/// rejects activities over ~28 KB); a larger summary is rejected rather
/// than truncated, because a silently cut series misstates the data.
pub const MAX_ROWS: usize = 200;

/// Most bytes of a summary's chart data (its points, serialized as the
/// derived Vega-Lite `data.values`). A larger summary is rejected, like one
/// over [`MAX_ROWS`].
pub const MAX_DATA_BYTES: usize = 32 * 1024;

/// Most bytes of one `content[].text` that [`find`] parses while looking for
/// the summary; a longer text is not parsed.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// The chart kind (`kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Bar,
    Line,
    Area,
    Pie,
    Donut,
    Scatter,
}

impl ChartKind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "bar" => Self::Bar,
            "line" => Self::Line,
            "area" => Self::Area,
            "pie" => Self::Pie,
            "donut" => Self::Donut,
            "scatter" => Self::Scatter,
            _ => return None,
        })
    }

    fn is_radial(self) -> bool {
        matches!(self, Self::Pie | Self::Donut)
    }
}

/// Bar orientation (`orientation`, bar only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    #[default]
    Vertical,
    Horizontal,
}

/// One validated data point: the `x` label (or number), the numeric `y`,
/// and the series label when the summary names a `series` field.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub x: Value,
    pub y: f64,
    pub series: Option<String>,
}

impl Point {
    /// `x` as a display label (Teams axis / legend text).
    pub fn x_label(&self) -> String {
        scalar_label(&self.x).unwrap_or_default()
    }
}

/// A validated `_meta.chart`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSummary {
    pub kind: ChartKind,
    pub title: Option<String>,
    /// Row field names, kept for the derived Vega-Lite encoding.
    pub x: String,
    pub y: String,
    pub series: Option<String>,
    pub x_title: Option<String>,
    pub y_title: Option<String>,
    pub unit: Option<String>,
    pub orientation: Orientation,
    pub stacked: bool,
    /// The rows as points, in row order (a row's other keys are not kept).
    pub points: Vec<Point>,
}

/// Why a `_meta.chart` was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid chart summary: {0}")]
pub struct ChartSummaryError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, ChartSummaryError> {
    Err(ChartSummaryError(msg.into()))
}

fn scalar_label(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn opt_str(o: &Map<String, Value>, key: &str) -> Result<Option<String>, ChartSummaryError> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => err(format!("`{key}` must be a string")),
    }
}

fn field(o: &Map<String, Value>, key: &str) -> Result<String, ChartSummaryError> {
    match o.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(_) => err(format!("`{key}` must be a non-empty field name")),
        None => err(format!("`{key}` is required")),
    }
}

impl ChartSummary {
    /// Validate a `_meta.chart` object.
    pub fn parse(v: &Value) -> Result<Self, ChartSummaryError> {
        let Some(o) = v.as_object() else {
            return err("must be an object");
        };
        let kind = match o.get("kind") {
            Some(Value::String(s)) => match ChartKind::parse(s) {
                Some(k) => k,
                None => return err(format!("unknown `kind` {s:?}")),
            },
            _ => return err("`kind` is required"),
        };
        let x = field(o, "x")?;
        let y = field(o, "y")?;
        let series = match o.get("series") {
            None | Some(Value::Null) => None,
            Some(_) => Some(field(o, "series")?),
        };
        let orientation = match o.get("orientation") {
            None | Some(Value::Null) => Orientation::Vertical,
            Some(Value::String(s)) if s == "vertical" => Orientation::Vertical,
            Some(Value::String(s)) if s == "horizontal" => Orientation::Horizontal,
            Some(_) => return err("`orientation` must be \"vertical\" or \"horizontal\""),
        };
        let stacked = match o.get("stacked") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return err("`stacked` must be a boolean"),
        };
        if kind.is_radial() && series.is_some() {
            return err("pie/donut charts take no `series`");
        }
        if orientation == Orientation::Horizontal && kind != ChartKind::Bar {
            return err("`orientation` applies to bar charts only");
        }
        if stacked && (kind != ChartKind::Bar || series.is_none()) {
            return err("`stacked` needs a bar chart with a `series`");
        }
        let Some(raw_rows) = o.get("rows").and_then(Value::as_array) else {
            return err("`rows` must be an array");
        };
        if raw_rows.is_empty() {
            return err("`rows` is empty");
        }
        if raw_rows.len() > MAX_ROWS {
            return err(format!(
                "`rows` has {} entries (at most {MAX_ROWS})",
                raw_rows.len()
            ));
        }
        let mut points = Vec::with_capacity(raw_rows.len());
        for (i, r) in raw_rows.iter().enumerate() {
            let Some(r) = r.as_object() else {
                return err(format!("row {i} is not an object"));
            };
            let xv = r.get(&x).cloned().unwrap_or(Value::Null);
            let x_ok = if kind == ChartKind::Scatter {
                xv.as_f64().is_some_and(f64::is_finite)
            } else {
                scalar_label(&xv).is_some()
            };
            if !x_ok {
                return err(format!("row {i}: `{x}` is missing or not a usable x value"));
            }
            let Some(yv) = r.get(&y).and_then(Value::as_f64).filter(|f| f.is_finite()) else {
                return err(format!("row {i}: `{y}` is missing or not a finite number"));
            };
            let sv = match &series {
                Some(sf) => match r.get(sf).and_then(scalar_label) {
                    Some(s) => Some(s),
                    None => return err(format!("row {i}: `{sf}` is missing")),
                },
                None => None,
            };
            points.push(Point {
                x: xv,
                y: yv,
                series: sv,
            });
        }
        let bytes = serde_json::to_vec(&data_values(&points, &x, &y, series.as_deref()))
            .map_or(usize::MAX, |b| b.len());
        if bytes > MAX_DATA_BYTES {
            return err(format!(
                "the chart data is {bytes} bytes (at most {MAX_DATA_BYTES})"
            ));
        }
        Ok(Self {
            kind,
            title: opt_str(o, "title")?,
            x,
            y,
            series,
            x_title: opt_str(o, "x_title")?,
            y_title: opt_str(o, "y_title")?,
            unit: opt_str(o, "unit")?,
            orientation,
            stacked,
            points,
        })
    }

    /// The y-axis title with its unit: `"Revenue (EUR)"`, `"EUR"`, or none.
    pub fn y_axis_title(&self) -> Option<String> {
        match (&self.y_title, &self.unit) {
            (Some(t), Some(u)) if !u.is_empty() => Some(format!("{t} ({u})")),
            (Some(t), _) => Some(t.clone()),
            (None, Some(u)) if !u.is_empty() => Some(u.clone()),
            _ => None,
        }
    }

    /// The points grouped by series label, in first-seen order. A summary
    /// without `series` yields one group labelled by `y_title` (or the `y`
    /// field name).
    pub fn series_groups(&self) -> Vec<(String, Vec<&Point>)> {
        let mut groups: Vec<(String, Vec<&Point>)> = Vec::new();
        let default = self.y_title.clone().unwrap_or_else(|| self.y.clone());
        for p in &self.points {
            let key = p.series.clone().unwrap_or_else(|| default.clone());
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, g)) => g.push(p),
                None => groups.push((key, vec![p])),
            }
        }
        groups
    }

    /// A Vega-Lite v5 spec drawing the same chart, for a consumer that
    /// renders Vega (Gemini Enterprise `VegaChart`).
    pub fn to_vega_lite(&self) -> Value {
        use serde_json::json;
        let numeric_x = self.points.iter().all(|p| p.x.is_number());
        let x_type = match self.kind {
            ChartKind::Scatter => "quantitative",
            ChartKind::Line | ChartKind::Area if numeric_x => "quantitative",
            ChartKind::Line | ChartKind::Area => "ordinal",
            _ => "nominal",
        };
        let x_enc = {
            let mut e = json!({ "field": self.x, "type": x_type, "sort": null });
            if let Some(t) = &self.x_title {
                e["title"] = json!(t);
            }
            e
        };
        let y_enc = {
            let mut e = json!({ "field": self.y, "type": "quantitative" });
            if let Some(t) = self.y_axis_title() {
                e["title"] = json!(t);
            }
            e
        };
        let mut spec = json!({
            "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
            "data": { "values": data_values(&self.points, &self.x, &self.y, self.series.as_deref()) },
        });
        if let Some(t) = &self.title {
            spec["title"] = json!(t);
        }
        let mut enc = Map::new();
        match self.kind {
            ChartKind::Pie | ChartKind::Donut => {
                spec["mark"] = if self.kind == ChartKind::Donut {
                    json!({ "type": "arc", "innerRadius": 50 })
                } else {
                    json!({ "type": "arc" })
                };
                enc.insert("theta".into(), y_enc);
                let mut c = json!({ "field": self.x, "type": "nominal", "sort": null });
                if let Some(t) = &self.x_title {
                    c["title"] = json!(t);
                }
                enc.insert("color".into(), c);
            }
            kind => {
                spec["mark"] = json!(match kind {
                    ChartKind::Bar => "bar",
                    ChartKind::Line => "line",
                    ChartKind::Area => "area",
                    _ => "point",
                });
                let (pos, val) = if self.orientation == Orientation::Horizontal {
                    ("y", "x")
                } else {
                    ("x", "y")
                };
                enc.insert(pos.into(), x_enc);
                enc.insert(val.into(), y_enc);
                if let Some(sf) = &self.series {
                    enc.insert("color".into(), json!({ "field": sf, "type": "nominal" }));
                    if kind == ChartKind::Bar && !self.stacked {
                        let off = if pos == "x" { "xOffset" } else { "yOffset" };
                        enc.insert(off.into(), json!({ "field": sf }));
                    }
                }
            }
        }
        spec["encoding"] = Value::Object(enc);
        spec
    }
}

/// The points as Vega-Lite `data.values`: one object per point with the
/// charted fields only (`x`, `y` and, with a series, `series`).
fn data_values(points: &[Point], x: &str, y: &str, series: Option<&str>) -> Vec<Value> {
    points
        .iter()
        .map(|p| {
            let mut row = Map::new();
            row.insert(x.to_string(), p.x.clone());
            row.insert(y.to_string(), Value::from(p.y));
            if let (Some(f), Some(s)) = (series, &p.series) {
                row.insert(f.to_string(), Value::String(s.clone()));
            }
            Value::Object(row)
        })
        .collect()
}

/// Apply `f` to the places of a tool result where an upstream puts its
/// chart data, first hit wins: the result itself, its `structuredContent`,
/// and, for an MCP result carried as a JSON string, the same two places of
/// each `content[].text` up to [`MAX_TEXT_BYTES`] (parsed once; strings
/// inside it are not parsed again). Data the upstream merely relays (a
/// document, a tool trace, a user's text) is never searched, so it cannot
/// plant a chart, and the cost is bounded by the cap.
fn in_known_places<T>(result: &Value, f: impl Fn(&Value) -> Option<T>) -> Option<T> {
    let own = |v: &Value| f(v).or_else(|| v.get("structuredContent").and_then(&f));
    if let Some(hit) = own(result) {
        return Some(hit);
    }
    result
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .filter(|t| t.len() <= MAX_TEXT_BYTES && t.trim_start().starts_with('{'))
        .find_map(|t| own(&serde_json::from_str::<Value>(t).ok()?))
}

/// Find `_meta.chart` in a tool result ([`in_known_places`]). `None` when no
/// summary is present; `Some(Err)` when one is present but invalid.
pub fn find(result: &Value) -> Option<Result<ChartSummary, ChartSummaryError>> {
    in_known_places(result, |v| {
        v.get("_meta")?
            .as_object()?
            .get(META_KEY)
            .map(ChartSummary::parse)
    })
}

/// The chart summary of a tool result, the one lookup every channel uses
/// ([`find`]): a valid summary is returned; a malformed one is logged (with
/// `channel`) and ignored, so the channel shows the PNG image. The
/// deprecated `_meta.vega_specs` is no longer read (#382).
pub fn resolve(result: &Value, channel: &str) -> Option<ChartSummary> {
    match find(result)? {
        Ok(summary) => Some(summary),
        Err(e) => {
            tracing::warn!(channel, error = %e, "ignoring the upstream's chart summary");
            None
        }
    }
}

/// #382: drop the `vega_spec` an upstream set on a `surface.components`
/// `report` component. That key is Triton's: the A2A face derives it from
/// the summary ([`ChartSummary::to_vega_lite`]) after dispatch, and the GE
/// builder draws it as a native `VegaChart`. An upstream's own spec would
/// skip the summary's checks and caps (and could name a remote `data.url`),
/// so the dispatcher strips it from every result. Returns how many it
/// dropped.
pub fn strip_upstream_vega_spec(result: &mut Value) -> usize {
    let Some(components) = result
        .get_mut("surface")
        .and_then(|s| s.get_mut("components"))
        .and_then(Value::as_array_mut)
    else {
        return 0;
    };
    components
        .iter_mut()
        .filter(|c| c.get("kind").and_then(Value::as_str) == Some("report"))
        .filter_map(|c| c.as_object_mut()?.remove("vega_spec"))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bar() -> Value {
        json!({
            "kind": "bar", "title": "Revenue", "x": "region", "y": "revenue",
            "rows": [ { "region": "North", "revenue": 10 }, { "region": "South", "revenue": 7.5 } ]
        })
    }

    #[test]
    fn parses_a_minimal_bar_summary() {
        let c = ChartSummary::parse(&bar()).expect("valid");
        assert_eq!(c.kind, ChartKind::Bar);
        assert_eq!(c.title.as_deref(), Some("Revenue"));
        assert_eq!(c.points.len(), 2);
        assert_eq!(c.points[1].x_label(), "South");
        assert_eq!(c.points[1].y, 7.5);
        assert_eq!(c.orientation, Orientation::Vertical);
        assert!(!c.stacked);
    }

    #[test]
    fn every_kind_parses() {
        for k in ["bar", "line", "area", "pie", "donut"] {
            let mut v = bar();
            v["kind"] = json!(k);
            assert!(ChartSummary::parse(&v).is_ok(), "{k}");
        }
        let scatter = json!({ "kind": "scatter", "x": "a", "y": "b",
            "rows": [ { "a": 1, "b": 2 }, { "a": 2.5, "b": 3 } ] });
        assert_eq!(
            ChartSummary::parse(&scatter).unwrap().kind,
            ChartKind::Scatter
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut v = bar();
        v["palette"] = json!("brand");
        v["rows"][0]["extra"] = json!({ "nested": true });
        assert!(ChartSummary::parse(&v).is_ok());
    }

    #[test]
    fn rejects_malformed_summaries() {
        let cases: Vec<(&str, Value)> = vec![
            ("not an object", json!([1, 2])),
            (
                "no kind",
                json!({ "x": "a", "y": "b", "rows": [ { "a": "x", "b": 1 } ] }),
            ),
            ("unknown kind", {
                let mut v = bar();
                v["kind"] = json!("radar");
                v
            }),
            ("no x", {
                let mut v = bar();
                v.as_object_mut().unwrap().remove("x");
                v
            }),
            ("empty y", {
                let mut v = bar();
                v["y"] = json!("");
                v
            }),
            ("rows not array", {
                let mut v = bar();
                v["rows"] = json!({});
                v
            }),
            ("rows empty", {
                let mut v = bar();
                v["rows"] = json!([]);
                v
            }),
            ("row not object", {
                let mut v = bar();
                v["rows"] = json!([1]);
                v
            }),
            ("y not a number", {
                let mut v = bar();
                v["rows"][0]["revenue"] = json!("ten");
                v
            }),
            ("y null", {
                let mut v = bar();
                v["rows"][0]["revenue"] = Value::Null;
                v
            }),
            ("x missing", {
                let mut v = bar();
                v["rows"][1].as_object_mut().unwrap().remove("region");
                v
            }),
            ("series missing in a row", {
                let mut v = bar();
                v["series"] = json!("year");
                v["rows"][0]["year"] = json!("2025");
                v
            }),
            ("pie with series", {
                let mut v = bar();
                v["kind"] = json!("pie");
                v["series"] = json!("region");
                v
            }),
            ("horizontal line", {
                let mut v = bar();
                v["kind"] = json!("line");
                v["orientation"] = json!("horizontal");
                v
            }),
            ("bad orientation", {
                let mut v = bar();
                v["orientation"] = json!("diagonal");
                v
            }),
            ("stacked without series", {
                let mut v = bar();
                v["stacked"] = json!(true);
                v
            }),
            ("title not a string", {
                let mut v = bar();
                v["title"] = json!(5);
                v
            }),
            (
                "scatter with label x",
                json!({ "kind": "scatter", "x": "a", "y": "b",
                "rows": [ { "a": "one", "b": 2 } ] }),
            ),
            ("too many rows", {
                let mut v = bar();
                v["rows"] = Value::Array(
                    (0..=MAX_ROWS)
                        .map(|i| json!({ "region": format!("r{i}"), "revenue": i }))
                        .collect(),
                );
                v
            }),
        ];
        for (name, v) in cases {
            assert!(ChartSummary::parse(&v).is_err(), "{name} must be rejected");
        }
    }

    #[test]
    fn y_axis_title_carries_the_unit() {
        let mut v = bar();
        v["y_title"] = json!("Revenue");
        v["unit"] = json!("EUR");
        assert_eq!(
            ChartSummary::parse(&v).unwrap().y_axis_title().as_deref(),
            Some("Revenue (EUR)")
        );
        v.as_object_mut().unwrap().remove("y_title");
        assert_eq!(
            ChartSummary::parse(&v).unwrap().y_axis_title().as_deref(),
            Some("EUR")
        );
    }

    #[test]
    fn series_groups_keep_first_seen_order() {
        let v = json!({ "kind": "line", "x": "m", "y": "v", "series": "s", "rows": [
            { "m": "Jan", "v": 1, "s": "B" }, { "m": "Jan", "v": 2, "s": "A" },
            { "m": "Feb", "v": 3, "s": "B" } ] });
        let c = ChartSummary::parse(&v).unwrap();
        let g = c.series_groups();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0, "B");
        assert_eq!(g[0].1.len(), 2);
        assert_eq!(g[1].0, "A");
    }

    #[test]
    fn find_reads_the_known_places_only() {
        // The result's own `_meta`, the `structuredContent`'s, and an MCP
        // result carried as a JSON string in `content[].text`.
        assert!(matches!(
            find(&json!({ "_meta": { "chart": bar() } })),
            Some(Ok(_))
        ));
        let sc = json!({ "structuredContent": { "_meta": { "chart": bar() } } });
        assert!(matches!(find(&sc), Some(Ok(_))));
        let as_text = json!({ "content": [ { "type": "text",
            "text": json!({ "_meta": { "chart": bar() } }).to_string() } ] });
        assert!(matches!(find(&as_text), Some(Ok(_))));
        assert!(find(&json!({ "_meta": { "vega_specs": [ {} ] } })).is_none());
        // A `chart` key outside `_meta` is not the summary.
        assert!(find(&json!({ "chart": bar() })).is_none());
        assert!(matches!(
            find(&json!({ "_meta": { "chart": "nope" } })),
            Some(Err(_))
        ));
    }

    /// Data the upstream relays (a document, a tool trace, a user's text)
    /// cannot plant a chart: nothing below the known places is searched.
    #[test]
    fn relayed_data_cannot_plant_a_chart() {
        let planted = json!({ "_meta": { "chart": bar() } }).to_string();
        for v in [
            json!({ "structuredContent": { "result": { "_meta": { "chart": bar() } } } }),
            json!({ "structuredContent": { "document": { "body": planted } } }),
            json!({ "_meta": { "tool_trace": [ { "data": planted } ] } }),
            json!({ "rows": [ { "note": planted } ] }),
            // A JSON string inside the parsed text is not parsed again.
            json!({ "content": [ { "type": "text",
                "text": json!({ "note": planted }).to_string() } ] }),
        ] {
            assert!(find(&v).is_none(), "{v}");
        }
    }

    /// #382: only the summary is read; a Vega-Lite spec in
    /// `_meta.vega_specs` or a `{kind: "vega"}` component is not a chart.
    #[test]
    fn resolve_reads_the_summary_only() {
        let legacy = json!({ "mark": "bar" });
        let both = json!({ "_meta": { "chart": bar(), "vega_specs": [ legacy ] } });
        assert!(resolve(&both, "t").is_some());
        let bad = json!({ "_meta": { "chart": { "kind": "radar" }, "vega_specs": [ legacy ] } });
        assert_eq!(resolve(&bad, "t"), None);
        assert_eq!(
            resolve(&json!({ "_meta": { "vega_specs": [ legacy ] } }), "t"),
            None
        );
        let component = json!({ "surface": { "components": [
            { "kind": "text", "value": "x" }, { "kind": "vega", "spec": legacy } ] } });
        assert_eq!(resolve(&component, "t"), None);
    }

    #[test]
    fn an_upstream_vega_spec_on_a_report_is_dropped() {
        let mut r = json!({ "surface": { "components": [
            { "kind": "report", "report_id": "a", "vega_spec": { "mark": "bar" } },
            { "kind": "text", "value": "x", "vega_spec": 1 },
            { "kind": "report", "report_id": "b" } ] } });
        assert_eq!(strip_upstream_vega_spec(&mut r), 1);
        let cs = r["surface"]["components"].as_array().unwrap();
        assert!(cs[0].get("vega_spec").is_none());
        // Only report components carry Triton's key; others are untouched.
        assert_eq!(cs[1]["vega_spec"], 1);
        assert_eq!(strip_upstream_vega_spec(&mut json!({ "rows": [] })), 0);
    }

    /// A text over the cap is not parsed at all (the cost of a lookup is
    /// bounded by the cap, not by what the upstream sends).
    #[test]
    fn an_oversized_text_is_not_parsed() {
        let mut big = json!({ "_meta": { "chart": bar() }, "pad": "" });
        big["pad"] = json!("x".repeat(MAX_TEXT_BYTES));
        let v = json!({ "content": [ { "type": "text", "text": big.to_string() } ] });
        assert!(find(&v).is_none());
    }

    /// The derived spec carries the charted fields only: a row's other
    /// keys (whatever the upstream left in) are not copied.
    #[test]
    fn vega_lite_data_carries_the_charted_fields_only() {
        let mut v = bar();
        v["series"] = json!("year");
        v["rows"][0]["year"] = json!("2025");
        v["rows"][1]["year"] = json!("2026");
        v["rows"][0]["internal_note"] = json!("not for the chart");
        let s = ChartSummary::parse(&v).unwrap().to_vega_lite();
        assert_eq!(
            s["data"]["values"],
            json!([
                { "region": "North", "revenue": 10.0, "year": "2025" },
                { "region": "South", "revenue": 7.5, "year": "2026" }
            ])
        );
    }

    /// A summary whose chart data exceeds [`MAX_DATA_BYTES`] is rejected,
    /// like one over [`MAX_ROWS`]: a card has a byte budget, and a cut
    /// series would misstate the data.
    #[test]
    fn a_summary_over_the_byte_cap_is_rejected() {
        let mut v = bar();
        v["rows"][0]["region"] = json!("x".repeat(MAX_DATA_BYTES));
        assert!(ChartSummary::parse(&v).is_err());
        // Bytes in keys the chart does not use do not count.
        let mut v = bar();
        v["rows"][0]["unused"] = json!("x".repeat(MAX_DATA_BYTES));
        assert!(ChartSummary::parse(&v).is_ok());
    }

    #[test]
    fn vega_lite_for_bar_line_pie_and_grouped() {
        let c = ChartSummary::parse(&bar()).unwrap();
        let s = c.to_vega_lite();
        assert_eq!(s["mark"], "bar");
        assert_eq!(s["encoding"]["x"]["field"], "region");
        assert_eq!(s["encoding"]["y"]["field"], "revenue");
        assert_eq!(s["data"]["values"].as_array().unwrap().len(), 2);
        assert_eq!(s["title"], "Revenue");

        let mut h = bar();
        h["orientation"] = json!("horizontal");
        let s = ChartSummary::parse(&h).unwrap().to_vega_lite();
        assert_eq!(s["encoding"]["y"]["field"], "region");
        assert_eq!(s["encoding"]["x"]["field"], "revenue");

        let mut p = bar();
        p["kind"] = json!("donut");
        let s = ChartSummary::parse(&p).unwrap().to_vega_lite();
        assert_eq!(s["mark"]["type"], "arc");
        assert_eq!(s["encoding"]["theta"]["field"], "revenue");
        assert_eq!(s["encoding"]["color"]["field"], "region");

        let g = json!({ "kind": "bar", "x": "m", "y": "v", "series": "s", "rows": [
            { "m": "Jan", "v": 1, "s": "A" }, { "m": "Jan", "v": 2, "s": "B" } ] });
        let s = ChartSummary::parse(&g).unwrap().to_vega_lite();
        assert_eq!(s["encoding"]["color"]["field"], "s");
        assert_eq!(s["encoding"]["xOffset"]["field"], "s");
    }
}
