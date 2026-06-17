//! PCBPnR Phase 5-D §3 — CSS property → typed AttrMap desugarer.
//!
//! Consumes the `attrs["style"]` key off a `NodeDecl` and produces
//! the typed numeric AttrMap form. Property→numeric table follows
//! the spec at §3.3 of the requirements doc verbatim.

use crate::ast::AttrMap;
use serde_json::Value as JsonValue;

use super::clip_path::{parse_clip_path, ClipPath};
use super::css_lexer::tokenize_style;
use super::error::StyleError;

/// Set of property names that desugar to numeric attributes (and
/// would conflict with the equivalent attribute being present
/// elsewhere on the same element). Exposed via
/// [`crate::style::recognized_style_properties`] for the M23 LSP so
/// its completion table is sourced from the same constant the
/// desugarer enforces.
pub(super) const RECOGNIZED_PROPS: &[&str] = &[
    "position",
    "left",
    "top",
    "rotate",
    "transform",
    "width",
    "height",
    "layer",
    "layer-band",
    "z-index",
    "from-layer",
    "to-layer",
    "drill",
    "diameter",
    "padding",
    "margin",
    "impedance",
    "tolerance",
    "preferred-layer",
    "pin-layer",
    "display",
    "gap",
    "scope",
    "clip-path",
];

/// Map a CSS property name to the equivalent numeric attribute key
/// that downstream consumers read. Used for conflict detection
/// (§3.7 — both forms on same element = parse error).
fn numeric_target_key(prop: &str, tag: &str) -> Option<&'static str> {
    match (prop, tag) {
        ("left" | "top" | "rotate" | "transform", _) => Some("at"),
        ("width" | "height", "Pad") => Some("size"),
        ("width" | "height", _) => Some("bbox"),
        ("layer", _) => Some("layer"),
        ("layer-band", _) => Some("layers"),
        ("z-index", _) => Some("layer-index"),
        ("from-layer", _) => Some("from-layer"),
        ("to-layer", _) => Some("to-layer"),
        ("drill", _) => Some("drill"),
        ("diameter", _) => Some("diameter"),
        ("padding", _) => Some("padding"),
        ("margin", _) => Some("clearance"),
        ("impedance" | "tolerance", _) => Some("impedance"),
        ("preferred-layer", _) => Some("preferred-layer"),
        ("pin-layer", _) => Some("layer-strict"),
        ("display" | "gap", _) => Some("cluster"),
        ("scope", _) => Some("scope"),
        ("clip-path", _) => Some("geometry"),
        _ => None,
    }
}

/// PCBPnR Phase 5-D §3 — desugar a `style="…"` attribute on
/// `attrs` into the typed numeric form. Mutates `attrs` in place:
/// removes the `style` key and writes the desugared numeric keys.
///
/// `tag_name` discriminates width/height target (Pad → `size`,
/// other tags → `bbox`).
///
/// # Errors
///
/// Returns [`StyleError`] for unknown properties, bad units, missing
/// `position: absolute` when `left:`/`top:` are present, malformed
/// `clip-path` shapes, or conflicting numeric/style forms on the
/// same element.
pub fn desugar_style(tag_name: &str, attrs: &mut AttrMap) -> Result<(), StyleError> {
    // Pull out and consume the style key. Return early when absent —
    // the desugarer is a no-op for `.grx` files that never use the
    // CSS shape.
    let style_string = match attrs.remove("style") {
        Some(JsonValue::String(s)) => s,
        Some(other) => {
            return Err(StyleError::BadNumber {
                property: "style".to_string(),
                value: format!("{other}"),
            });
        }
        None => return Ok(()),
    };

    let props = tokenize_style(&style_string)?;
    if props.is_empty() {
        return Ok(());
    }

    // Index by name for quick lookup of dependent properties
    // (width/height for clip-path inset, drill/diameter for via, etc.).
    let mut by_name: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::new();
    for (k, v) in &props {
        by_name.insert(k.as_str(), v.as_str());
    }

    // Validate recognized properties + suggest "did you mean"
    // alternates for unknowns.
    for (name, _) in &props {
        if !RECOGNIZED_PROPS.contains(&name.as_str()) {
            return Err(StyleError::UnknownProperty {
                property: name.clone(),
                suggestion: closest_match(name),
            });
        }
    }

    // §3 — `position: absolute` is required when left/top/width/height
    // are present (rejects missing-position-absolute case).
    let has_geometry = props.iter().any(|(p, _)| {
        matches!(
            p.as_str(),
            "left" | "top" | "width" | "height" | "clip-path"
        )
    });
    if has_geometry && by_name.get("position").copied() != Some("absolute") {
        // Per §3.3 we ALLOW omitting position when only `clip-path:
        // circle(...)` is present (it's self-sufficient). Only flag
        // when left/top/width/height are used.
        let strict_geometry = props.iter().any(|(p, _)| {
            matches!(p.as_str(), "left" | "top" | "width" | "height")
        });
        if strict_geometry {
            return Err(StyleError::MissingPositionAbsolute);
        }
    }

    // Detect conflicts (§3.7) BEFORE we start writing to attrs.
    for (prop, _) in &props {
        if let Some(numeric_key) = numeric_target_key(prop.as_str(), tag_name) {
            if attrs.contains_key(numeric_key) {
                return Err(StyleError::ConflictingForms {
                    style_prop: prop.clone(),
                    numeric_key: numeric_key.to_string(),
                });
            }
        }
    }

    // Assemble compound objects (at, bbox/size, impedance) before
    // inserting so we set them once each.
    let mut at_x: Option<f64> = None;
    let mut at_y: Option<f64> = None;
    let mut at_rot: Option<f64> = None;
    let mut dim_w: Option<f64> = None;
    let mut dim_h: Option<f64> = None;
    let mut imp_z0: Option<f64> = None;
    let mut imp_tol: Option<f64> = None;
    let mut cluster_gap: Option<f64> = None;
    let mut clip: Option<ClipPath> = None;

    // Single pass for all properties.
    for (prop, value) in &props {
        match prop.as_str() {
            "position" => {
                // `position: absolute` is the only recognized value.
                if value.trim() != "absolute" {
                    return Err(StyleError::UnknownProperty {
                        property: format!("position: {value}"),
                        suggestion: Some("position: absolute".to_string()),
                    });
                }
            }
            "left" => at_x = Some(parse_mm(prop, value)?),
            "top" => at_y = Some(parse_mm(prop, value)?),
            "rotate" => at_rot = Some(parse_deg(prop, value)?),
            "transform" => {
                let val = value.trim();
                if let Some(inner) =
                    val.strip_prefix("rotate(").and_then(|s| s.strip_suffix(')'))
                {
                    at_rot = Some(parse_deg(prop, inner.trim())?);
                } else {
                    return Err(StyleError::UnknownTransform {
                        transform_kind: val.to_string(),
                    });
                }
            }
            "width" => dim_w = Some(parse_mm(prop, value)?),
            "height" => dim_h = Some(parse_mm(prop, value)?),
            "layer" => {
                attrs.insert("layer".into(), JsonValue::String(value.trim().to_string()));
            }
            "layer-band" => {
                attrs.insert("layers".into(), JsonValue::String(value.trim().to_string()));
            }
            "z-index" => {
                let n: i64 = value.trim().parse().map_err(|_| StyleError::BadNumber {
                    property: "z-index".to_string(),
                    value: value.clone(),
                })?;
                attrs.insert("layer-index".into(), JsonValue::Number(n.into()));
            }
            "from-layer" => {
                attrs.insert(
                    "from-layer".into(),
                    JsonValue::String(value.trim().to_string()),
                );
            }
            "to-layer" => {
                attrs.insert(
                    "to-layer".into(),
                    JsonValue::String(value.trim().to_string()),
                );
            }
            "drill" => {
                let d = parse_mm(prop, value)?;
                attrs.insert("drill".into(), json_dim_obj("d", d));
            }
            "diameter" => {
                let d = parse_mm(prop, value)?;
                attrs.insert("diameter".into(), json_dim_obj("d", d));
            }
            "padding" => {
                let d = parse_mm(prop, value)?;
                attrs.insert("padding".into(), json_dim_obj("d", d));
            }
            "margin" => {
                let d = parse_mm(prop, value)?;
                attrs.insert("clearance".into(), json_dim_obj("d", d));
            }
            "impedance" => {
                imp_z0 = Some(parse_bare_number(prop, value)?);
            }
            "tolerance" => {
                imp_tol = Some(parse_bare_number(prop, value)?);
            }
            "preferred-layer" => {
                attrs.insert(
                    "preferred-layer".into(),
                    JsonValue::String(value.trim().to_string()),
                );
            }
            "pin-layer" => {
                let v = match value.trim() {
                    "strict" => true,
                    "prefer" => false,
                    other => {
                        return Err(StyleError::UnknownProperty {
                            property: format!("pin-layer: {other}"),
                            suggestion: Some(
                                "pin-layer: strict OR pin-layer: prefer".into(),
                            ),
                        });
                    }
                };
                attrs.insert("layer-strict".into(), JsonValue::Bool(v));
            }
            "display" => {
                // display: flex is the only recognized value
                if value.trim() != "flex" {
                    return Err(StyleError::UnknownProperty {
                        property: format!("display: {value}"),
                        suggestion: Some("display: flex".into()),
                    });
                }
            }
            "gap" => {
                cluster_gap = Some(parse_mm(prop, value)?);
            }
            "scope" => {
                attrs.insert("scope".into(), JsonValue::String(value.trim().to_string()));
            }
            "clip-path" => {
                clip = Some(parse_clip_path(value)?);
            }
            _ => unreachable!("RECOGNIZED_PROPS gate handles unknowns"),
        }
    }

    // Emit `at` object if any of x/y/rot were set.
    if at_x.is_some() || at_y.is_some() || at_rot.is_some() {
        let mut at = serde_json::Map::new();
        if let Some(x) = at_x {
            at.insert("x".into(), json_num(x));
        }
        if let Some(y) = at_y {
            at.insert("y".into(), json_num(y));
        }
        if let Some(r) = at_rot {
            at.insert("rot".into(), json_num(r));
        }
        attrs.insert("at".into(), JsonValue::Object(at));
    }

    // Emit `bbox` (non-Pad) or `size` (Pad) for width/height.
    if dim_w.is_some() || dim_h.is_some() {
        let mut dim = serde_json::Map::new();
        // bbox carries x/y too (taken from at_x/at_y if present); size doesn't.
        let target_key = if tag_name == "Pad" {
            if let Some(w) = dim_w {
                dim.insert("w".into(), json_num(w));
            }
            if let Some(h) = dim_h {
                dim.insert("h".into(), json_num(h));
            }
            "size"
        } else {
            // Subsystem / other: bbox carries x/y/w/h.
            if let Some(x) = at_x {
                dim.insert("x".into(), json_num(x));
            }
            if let Some(y) = at_y {
                dim.insert("y".into(), json_num(y));
            }
            if let Some(w) = dim_w {
                dim.insert("w".into(), json_num(w));
            }
            if let Some(h) = dim_h {
                dim.insert("h".into(), json_num(h));
            }
            "bbox"
        };
        // For non-Pad: bbox subsumes `at` (placement attrs collapse
        // into bbox.x/bbox.y). Pad keeps both `at` and `size`.
        if target_key == "bbox" && attrs.contains_key("at") {
            attrs.remove("at");
        }
        attrs.insert(target_key.into(), JsonValue::Object(dim));
    }

    // Impedance — both z0 + tol live under one object.
    if imp_z0.is_some() || imp_tol.is_some() {
        let mut imp = serde_json::Map::new();
        if let Some(z) = imp_z0 {
            imp.insert("z0".into(), json_num(z));
        }
        if let Some(t) = imp_tol {
            imp.insert("tol".into(), json_num(t));
        }
        attrs.insert("impedance".into(), JsonValue::Object(imp));
    }

    // Cluster — display: flex; gap: Nmm
    if let Some(gap) = cluster_gap {
        let mut obj = serde_json::Map::new();
        obj.insert("gap".into(), json_num(gap));
        attrs.insert("cluster".into(), JsonValue::Object(obj));
    }

    // Geometry from clip-path — consumes width/height for inset
    // variants.
    if let Some(cp) = clip {
        let geom = cp.to_json(dim_w, dim_h)?;
        attrs.insert("geometry".into(), geom);
    }

    Ok(())
}

fn parse_mm(prop: &str, value: &str) -> Result<f64, StyleError> {
    super::units::parse_length_mm(value).ok_or_else(|| StyleError::BadUnit {
        property: prop.to_string(),
        value: value.to_string(),
    })
}

fn parse_deg(prop: &str, value: &str) -> Result<f64, StyleError> {
    let v = value.trim();
    if let Some(num) = v.strip_suffix("deg") {
        num.trim().parse::<f64>().map_err(|_| StyleError::BadNumber {
            property: prop.to_string(),
            value: value.to_string(),
        })
    } else {
        // Bare number for `rotate` allowed (CSS uses degrees by default in some specs)
        v.parse::<f64>().map_err(|_| StyleError::BadNumber {
            property: prop.to_string(),
            value: value.to_string(),
        })
    }
}

fn parse_bare_number(prop: &str, value: &str) -> Result<f64, StyleError> {
    value
        .trim()
        .parse::<f64>()
        .map_err(|_| StyleError::BadNumber {
            property: prop.to_string(),
            value: value.to_string(),
        })
}

fn json_num(n: f64) -> JsonValue {
    serde_json::Number::from_f64(n)
        .map(JsonValue::Number)
        .unwrap_or(JsonValue::Null)
}

fn json_dim_obj(key: &str, n: f64) -> JsonValue {
    let mut obj = serde_json::Map::new();
    obj.insert(key.into(), json_num(n));
    JsonValue::Object(obj)
}

/// Levenshtein-distance closest match from the recognized property
/// list. Returns the closest if its distance is ≤2 (rejects matches
/// that are too far apart to be a typo).
fn closest_match(needle: &str) -> Option<String> {
    let mut best: Option<(&str, usize)> = None;
    for prop in RECOGNIZED_PROPS {
        let d = levenshtein(needle, prop);
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((prop, d));
        }
    }
    best.filter(|(_, d)| *d <= 2).map(|(p, _)| p.to_string())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (m, n) = (a.chars().count(), b.chars().count());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0; n + 1];
    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(tag: &str, style: &str) -> AttrMap {
        let mut attrs = AttrMap::new();
        attrs.insert("style".into(), JsonValue::String(style.into()));
        desugar_style(tag, &mut attrs).expect("desugar ok");
        attrs
    }

    #[test]
    fn left_top_rotate_to_at() {
        let attrs = run(
            "Component",
            "position:absolute; left:30mm; top:5mm; rotate:90deg",
        );
        let at = attrs.get("at").expect("at present");
        assert_eq!(at["x"], 30.0);
        assert_eq!(at["y"], 5.0);
        assert_eq!(at["rot"], 90.0);
        assert!(!attrs.contains_key("style"), "style key removed");
    }

    #[test]
    fn transform_rotate_to_at_rot() {
        let attrs = run(
            "Component",
            "position:absolute; left:0mm; top:0mm; transform:rotate(45deg)",
        );
        assert_eq!(attrs["at"]["rot"], 45.0);
    }

    #[test]
    fn width_height_pad_to_size() {
        let attrs = run(
            "Pad",
            "position:absolute; left:0mm; top:0mm; width:0.5mm; height:0.5mm",
        );
        assert_eq!(attrs["size"]["w"], 0.5);
        assert_eq!(attrs["size"]["h"], 0.5);
        // Pad keeps both at and size.
        assert!(attrs.contains_key("at"));
    }

    #[test]
    fn width_height_subsystem_to_bbox() {
        let attrs = run(
            "Subsystem",
            "position:absolute; left:30mm; top:5mm; width:12mm; height:10mm",
        );
        assert_eq!(attrs["bbox"]["x"], 30.0);
        assert_eq!(attrs["bbox"]["y"], 5.0);
        assert_eq!(attrs["bbox"]["w"], 12.0);
        assert_eq!(attrs["bbox"]["h"], 10.0);
        // bbox subsumes at on non-Pad
        assert!(!attrs.contains_key("at"));
    }

    #[test]
    fn layer_band_to_layers_csv() {
        let attrs = run("Subsystem", "layer-band:F.Cu,In1.Cu");
        assert_eq!(attrs["layers"], "F.Cu,In1.Cu");
    }

    #[test]
    fn impedance_and_tolerance_collapse() {
        let attrs = run("Net", "impedance:50; tolerance:10");
        assert_eq!(attrs["impedance"]["z0"], 50.0);
        assert_eq!(attrs["impedance"]["tol"], 10.0);
    }

    #[test]
    fn pin_layer_strict_to_bool() {
        let attrs = run("Net", "pin-layer:strict");
        assert_eq!(attrs["layer-strict"], true);
    }

    #[test]
    fn pin_layer_prefer_to_false() {
        let attrs = run("Net", "pin-layer:prefer");
        assert_eq!(attrs["layer-strict"], false);
    }

    #[test]
    fn via_drill_diameter() {
        let attrs = run("Via", "from-layer:F.Cu; to-layer:In1.Cu; drill:0.3mm; diameter:0.6mm");
        assert_eq!(attrs["from-layer"], "F.Cu");
        assert_eq!(attrs["to-layer"], "In1.Cu");
        assert_eq!(attrs["drill"]["d"], 0.3);
        assert_eq!(attrs["diameter"]["d"], 0.6);
    }

    #[test]
    fn clip_path_round_rect_for_pad() {
        let attrs = run(
            "Pad",
            "position:absolute; left:0mm; top:0mm; width:0.5mm; height:0.5mm; \
             clip-path: inset(0 0 0 0 round 0.05mm)",
        );
        let geom = &attrs["geometry"];
        assert_eq!(geom["kind"], "round_rect");
        assert_eq!(geom["w"], 0.5);
        assert_eq!(geom["h"], 0.5);
        assert_eq!(geom["r"], 0.05);
    }

    #[test]
    fn clip_path_polygon_for_keepout() {
        let attrs = run(
            "Keepout",
            "position:absolute; left:0mm; top:-1mm; \
             clip-path: polygon(0mm 0mm, 14mm 0mm, 14mm 5mm, 0mm 5mm); \
             scope:net-class(ground)",
        );
        let geom = &attrs["geometry"];
        assert_eq!(geom["kind"], "polygon");
        assert_eq!(geom["points"].as_array().unwrap().len(), 4);
        assert_eq!(attrs["scope"], "net-class(ground)");
    }

    #[test]
    fn case_insensitive_property_names() {
        let attrs = run("Component", "POSITION: absolute; LEFT: 30mm; TOP: 5mm");
        assert_eq!(attrs["at"]["x"], 30.0);
        assert_eq!(attrs["at"]["y"], 5.0);
    }

    #[test]
    fn block_comments_stripped() {
        let attrs = run(
            "Component",
            "position:absolute; left:30mm; /* TODO: review */ top:5mm",
        );
        assert_eq!(attrs["at"]["x"], 30.0);
        assert_eq!(attrs["at"]["y"], 5.0);
    }

    #[test]
    fn bad_unit_errors() {
        // `pt` is not on the accepted list (mm/cm/in/px) — must reject.
        let mut attrs = AttrMap::new();
        attrs.insert(
            "style".into(),
            JsonValue::String("position:absolute; left:30pt".into()),
        );
        let err = desugar_style("Component", &mut attrs).expect_err("must reject");
        assert!(matches!(err, StyleError::BadUnit { property, .. } if property == "left"));
    }

    #[test]
    fn css_units_cm_in_px_canonicalize_to_mm() {
        // 1cm = 10mm
        let a = run(
            "Component",
            "position:absolute; left:1cm; top:0mm",
        );
        assert!((a["at"]["x"].as_f64().unwrap() - 10.0).abs() < 1e-9);

        // 1in = 25.4mm
        let b = run(
            "Component",
            "position:absolute; left:1in; top:0mm",
        );
        assert!((b["at"]["x"].as_f64().unwrap() - 25.4).abs() < 1e-9);

        // 96px = 25.4mm (CSS reference px @ 96 DPI)
        let c = run(
            "Component",
            "position:absolute; left:96px; top:0mm",
        );
        assert!((c["at"]["x"].as_f64().unwrap() - 25.4).abs() < 1e-9);
    }

    #[test]
    fn css_units_clip_path_polygon_mixed_units() {
        // The clip_path mini-grammar shares parse_length_mm — verify
        // mixing cm + mm inside polygon coordinates works.
        let attrs = run(
            "Keepout",
            "position:absolute; left:0mm; top:0mm; \
             clip-path: polygon(0mm 0mm, 1cm 0mm, 1cm 5mm, 0mm 5mm)",
        );
        let pts = attrs["geometry"]["points"].as_array().unwrap();
        assert_eq!(pts.len(), 4);
        // Second point's x should be 10mm (canonicalized from 1cm).
        assert!((pts[1]["x"].as_f64().unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_property_with_suggestion() {
        let mut attrs = AttrMap::new();
        attrs.insert("style".into(), JsonValue::String("imprdance:50".into()));
        let err = desugar_style("Net", &mut attrs).expect_err("must reject");
        match err {
            StyleError::UnknownProperty { property, suggestion } => {
                assert_eq!(property, "imprdance");
                assert_eq!(suggestion.as_deref(), Some("impedance"));
            }
            other => panic!("expected UnknownProperty, got {other:?}"),
        }
    }

    #[test]
    fn both_forms_conflict() {
        let mut attrs = AttrMap::new();
        attrs.insert("at".into(), serde_json::json!({"x": 5}));
        attrs.insert(
            "style".into(),
            JsonValue::String("position:absolute; left:30mm".into()),
        );
        let err = desugar_style("Component", &mut attrs).expect_err("must conflict");
        assert!(matches!(err, StyleError::ConflictingForms { .. }));
    }

    #[test]
    fn missing_position_absolute_when_left_present() {
        let mut attrs = AttrMap::new();
        attrs.insert("style".into(), JsonValue::String("left:30mm".into()));
        let err = desugar_style("Component", &mut attrs).expect_err("must require position");
        assert!(matches!(err, StyleError::MissingPositionAbsolute));
    }

    #[test]
    fn no_op_when_no_style_attr() {
        let mut attrs = AttrMap::new();
        attrs.insert("footprint".into(), JsonValue::String("0402".into()));
        desugar_style("Component", &mut attrs).expect("ok");
        assert_eq!(attrs.len(), 1);
        assert!(attrs.contains_key("footprint"));
    }

    #[test]
    fn z_index_to_layer_index() {
        let attrs = run("Component", "z-index:2");
        assert_eq!(attrs["layer-index"], 2);
    }

    #[test]
    fn cluster_gap_for_subsystem() {
        let attrs = run("Subsystem", "display:flex; gap:0.5mm");
        assert_eq!(attrs["cluster"]["gap"], 0.5);
    }
}
