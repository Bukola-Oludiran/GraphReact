//! PCBPnR Phase 5-D §3 (inverse direction) — typed numeric AttrMap →
//! CSS-shape `style="…"` string.
//!
//! Mirror of [`super::desugar::desugar_style`]. Where desugar
//! consumes a `style="…"` body and writes typed compound objects
//! (`at`, `bbox`, `geometry`, `impedance`, …) into the AttrMap,
//! resugar walks those typed objects and emits the equivalent CSS
//! string.
//!
//! Symmetric purpose: PCBPnR's `pcb-grx-export` (Phase 5-D Part 3)
//! reads `.kicad_pcb` files via PCBPnR's typed structures and emits
//! `.grx` files in CSS shape. Phase 5-E's `tool:state:query` returns
//! CSS-shape `.grx` snippets to the LLM. Both downstream consumers
//! ride on this module.
//!
//! ## Canonical CSS forms
//!
//! Where the forward direction is many-to-one (e.g., `transform:
//! rotate(Ndeg)` and `rotate: Ndeg` both map to `at.rot`), the
//! reverse emits the canonical form. Choices documented inline.

use crate::ast::AttrMap;
use serde_json::Value as JsonValue;

/// Result of resugaring an AttrMap: the CSS-shape `style=` body
/// (when any CSS-eligible attrs were present) and the remaining
/// non-CSS-eligible attributes that pass through verbatim.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resugared {
    /// The body of a `style="…"` attribute, or `None` when no
    /// CSS-shape-eligible attrs were present. Properties appear in a
    /// stable canonical order so emitted `.grx` is reproducible.
    pub style: Option<String>,
    /// Attributes that don't desugar to CSS — `id`, `footprint`,
    /// `net`, custom component props, etc. Emit these verbatim
    /// alongside the `style=` attribute when rendering the element.
    pub remaining: AttrMap,
}

/// Typed numeric attribute keys that the forward desugar pass
/// produces and that the reverse pass consumes. Used to decide which
/// AttrMap entries to consume vs leave in `remaining`.
const CSS_ELIGIBLE_KEYS: &[&str] = &[
    "at",
    "bbox",
    "size",
    "geometry",
    "impedance",
    "from-layer",
    "to-layer",
    "drill",
    "diameter",
    "padding",
    "clearance",
    "preferred-layer",
    "layer-strict",
    "cluster",
    "scope",
    "layer",
    "layers",
    "layer-index",
];

/// PCBPnR Phase 5-D §3 reverse — walk `attrs` and produce a
/// CSS-shape `style="…"` body for every recognized typed numeric
/// key. Non-CSS attributes pass through in `remaining` so the
/// caller can render `<Foo id="x" footprint="0402" style="…"/>`.
///
/// The output is stable: properties appear in a canonical order
/// (position-first, then layer info, then geometry / clip-path,
/// then misc). The order matches what the forward direction would
/// re-parse identically — i.e., `desugar(resugar(x)) == x` for
/// the typed-key subset.
pub fn resugar_attrs(tag_name: &str, attrs: &AttrMap) -> Resugared {
    let mut buf = String::new();
    let mut has_geometry_property = false;

    // -- Group 1: position-shaped (position:absolute; left; top; rotate)
    //
    // Pad keeps both `at` and `size`; non-Pad has bbox (which subsumes
    // `at`'s x/y). We pull placement values from whichever object the
    // forward pass uses.
    let at_obj = attrs.get("at").and_then(JsonValue::as_object);
    let bbox_obj = attrs.get("bbox").and_then(JsonValue::as_object);
    let size_obj = attrs.get("size").and_then(JsonValue::as_object);

    let (place_x, place_y, place_rot) = match (at_obj, bbox_obj) {
        (Some(at), _) => (
            at.get("x").and_then(JsonValue::as_f64),
            at.get("y").and_then(JsonValue::as_f64),
            at.get("rot").and_then(JsonValue::as_f64),
        ),
        (None, Some(b)) => (
            b.get("x").and_then(JsonValue::as_f64),
            b.get("y").and_then(JsonValue::as_f64),
            None,
        ),
        (None, None) => (None, None, None),
    };

    // Width / height come from bbox (non-Pad) or size (Pad).
    let (dim_w, dim_h) = if tag_name == "Pad" {
        match size_obj {
            Some(s) => (
                s.get("w").and_then(JsonValue::as_f64),
                s.get("h").and_then(JsonValue::as_f64),
            ),
            None => (None, None),
        }
    } else {
        match bbox_obj {
            Some(b) => (
                b.get("w").and_then(JsonValue::as_f64),
                b.get("h").and_then(JsonValue::as_f64),
            ),
            None => (None, None),
        }
    };

    // `position: absolute` is required by the forward parser when any
    // of left/top/width/height are present.
    let needs_position =
        place_x.is_some() || place_y.is_some() || dim_w.is_some() || dim_h.is_some();
    if needs_position {
        push_prop(&mut buf, "position", "absolute");
        has_geometry_property = true;
    }
    if let Some(x) = place_x {
        push_prop(&mut buf, "left", &format_mm(x));
    }
    if let Some(y) = place_y {
        push_prop(&mut buf, "top", &format_mm(y));
    }
    if let Some(rot) = place_rot {
        // Canonical form: `rotate:Ndeg` (simpler than the alternate
        // `transform:rotate(Ndeg)` syntax).
        push_prop(&mut buf, "rotate", &format_deg(rot));
    }
    if let Some(w) = dim_w {
        push_prop(&mut buf, "width", &format_mm(w));
    }
    if let Some(h) = dim_h {
        push_prop(&mut buf, "height", &format_mm(h));
    }

    // -- Group 2: clip-path (geometry) ---------------------------------

    if let Some(geom) = attrs.get("geometry").and_then(JsonValue::as_object) {
        if let Some(clip) = resugar_geometry(geom) {
            push_prop(&mut buf, "clip-path", &clip);
            has_geometry_property = true;
        }
    }

    // -- Group 3: layer info ------------------------------------------

    if let Some(layer) = attrs.get("layer").and_then(JsonValue::as_str) {
        push_prop(&mut buf, "layer", layer);
    }
    if let Some(layers) = attrs.get("layers").and_then(JsonValue::as_str) {
        push_prop(&mut buf, "layer-band", layers);
    }
    if let Some(idx) = attrs.get("layer-index").and_then(JsonValue::as_i64) {
        push_prop(&mut buf, "z-index", &idx.to_string());
    }
    if let Some(s) = attrs.get("from-layer").and_then(JsonValue::as_str) {
        push_prop(&mut buf, "from-layer", s);
    }
    if let Some(s) = attrs.get("to-layer").and_then(JsonValue::as_str) {
        push_prop(&mut buf, "to-layer", s);
    }
    if let Some(s) = attrs.get("preferred-layer").and_then(JsonValue::as_str) {
        push_prop(&mut buf, "preferred-layer", s);
    }
    if let Some(strict) = attrs.get("layer-strict").and_then(JsonValue::as_bool) {
        push_prop(
            &mut buf,
            "pin-layer",
            if strict { "strict" } else { "prefer" },
        );
    }

    // -- Group 4: dimensional objects (drill / diameter / padding /
    //             clearance) ----------------------------------------

    if let Some(d) = attrs.get("drill").and_then(get_dim_d) {
        push_prop(&mut buf, "drill", &format_mm(d));
    }
    if let Some(d) = attrs.get("diameter").and_then(get_dim_d) {
        push_prop(&mut buf, "diameter", &format_mm(d));
    }
    if let Some(d) = attrs.get("padding").and_then(get_dim_d) {
        push_prop(&mut buf, "padding", &format_mm(d));
    }
    if let Some(d) = attrs.get("clearance").and_then(get_dim_d) {
        // Forward direction: `margin: Nmm` → `clearance.d`.
        push_prop(&mut buf, "margin", &format_mm(d));
    }

    // -- Group 5: impedance (z0 + tol) --------------------------------

    if let Some(imp) = attrs.get("impedance").and_then(JsonValue::as_object) {
        if let Some(z) = imp.get("z0").and_then(JsonValue::as_f64) {
            push_prop(&mut buf, "impedance", &format_bare(z));
        }
        if let Some(t) = imp.get("tol").and_then(JsonValue::as_f64) {
            push_prop(&mut buf, "tolerance", &format_bare(t));
        }
    }

    // -- Group 6: cluster (display:flex; gap) -------------------------

    if let Some(cluster) = attrs.get("cluster").and_then(JsonValue::as_object) {
        if let Some(gap) = cluster.get("gap").and_then(JsonValue::as_f64) {
            push_prop(&mut buf, "display", "flex");
            push_prop(&mut buf, "gap", &format_mm(gap));
        }
    }

    // -- Group 7: scope (string pass-through) -------------------------

    if let Some(scope) = attrs.get("scope").and_then(JsonValue::as_str) {
        push_prop(&mut buf, "scope", scope);
    }

    // Strip trailing "; " from the buffer.
    let trimmed = buf.trim_end_matches("; ").to_string();
    let style = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    };

    // Build the `remaining` map: every entry that wasn't consumed by
    // the CSS resugar. Skip the auto-injected `component`/`_depth`/
    // `_parentPath` IR keys (those don't belong in the source-form
    // emit).
    let mut remaining = AttrMap::new();
    for (k, v) in attrs {
        if CSS_ELIGIBLE_KEYS.contains(&k.as_str()) {
            continue;
        }
        if k == "component" || k == "_depth" || k == "_parentPath" {
            continue;
        }
        // Pad keeps `at` AND `size`; both are consumed by the resugar
        // above. Other consumed keys are already in CSS_ELIGIBLE_KEYS.
        remaining.insert(k.clone(), v.clone());
    }

    // Reference the unused flag to silence warnings if we add
    // conditional logic later. (Currently it tracks whether any
    // group 1 / group 2 property was emitted — useful for future
    // diagnostics.)
    let _ = has_geometry_property;

    Resugared { style, remaining }
}

/// Reverse of [`super::clip_path::ClipPath::to_json`]. Reads a
/// `{kind: ..., ...}` JSON object and emits the equivalent
/// `clip-path: ...` value.
fn resugar_geometry(geom: &serde_json::Map<String, JsonValue>) -> Option<String> {
    let kind = geom.get("kind")?.as_str()?;
    match kind {
        "rect" => Some("inset(0 0 0 0)".to_string()),
        "round_rect" => {
            let r = geom.get("r").and_then(JsonValue::as_f64)?;
            Some(format!("inset(0 0 0 0 round {})", format_mm(r)))
        }
        "circle" => {
            let r = geom.get("r").and_then(JsonValue::as_f64)?;
            let cx = geom.get("cx").and_then(JsonValue::as_f64)?;
            let cy = geom.get("cy").and_then(JsonValue::as_f64)?;
            Some(format!(
                "circle({} at {} {})",
                format_mm(r),
                format_mm(cx),
                format_mm(cy)
            ))
        }
        "ellipse" => {
            let rx = geom.get("rx").and_then(JsonValue::as_f64)?;
            let ry = geom.get("ry").and_then(JsonValue::as_f64)?;
            let cx = geom.get("cx").and_then(JsonValue::as_f64)?;
            let cy = geom.get("cy").and_then(JsonValue::as_f64)?;
            Some(format!(
                "ellipse({} {} at {} {})",
                format_mm(rx),
                format_mm(ry),
                format_mm(cx),
                format_mm(cy)
            ))
        }
        "polygon" => {
            use std::fmt::Write as _;
            let points = geom.get("points")?.as_array()?;
            let mut buf = String::from("polygon(");
            for (i, p) in points.iter().enumerate() {
                let x = p.get("x")?.as_f64()?;
                let y = p.get("y")?.as_f64()?;
                if i > 0 {
                    buf.push_str(", ");
                }
                let _ = write!(buf, "{} {}", format_mm(x), format_mm(y));
            }
            buf.push(')');
            Some(buf)
        }
        "path" => {
            let svg_d = geom.get("svg_d")?.as_str()?;
            // Canonical form quotes with single quotes (matches the
            // forward parser's preferred form).
            Some(format!("path('{svg_d}')"))
        }
        "native" => {
            // {kind: "native", source: "kicad", data: {name: "…", ...}}
            let source = geom.get("source")?.as_str()?;
            if source != "kicad" {
                return None;
            }
            let data = geom.get("data")?.as_object()?;
            let name = data.get("name")?.as_str()?;
            // Remaining data fields (without `name`) re-serialized as JSON.
            let mut inner = serde_json::Map::new();
            for (k, v) in data {
                if k == "name" {
                    continue;
                }
                inner.insert(k.clone(), v.clone());
            }
            let data_json = serde_json::to_string(&JsonValue::Object(inner)).ok()?;
            Some(format!("kicad-shape(\"{name}\", {data_json})"))
        }
        _ => None,
    }
}

fn get_dim_d(v: &JsonValue) -> Option<f64> {
    v.as_object()
        .and_then(|m| m.get("d"))
        .and_then(JsonValue::as_f64)
}

/// Format a number as `Nmm` with the minimum decimal places
/// required for stable round-trip. f64 → string with trailing-zero
/// stripping so `30.0` formats as `30mm` (not `30.0mm`) and `0.05`
/// formats as `0.05mm`.
fn format_mm(n: f64) -> String {
    format!("{}mm", format_number(n))
}

fn format_deg(n: f64) -> String {
    format!("{}deg", format_number(n))
}

fn format_bare(n: f64) -> String {
    format_number(n)
}

/// f64 → string with no trailing zeros and no decimal point when
/// whole. Used by every emit path so the output is stable + minimal.
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.is_finite() && n.abs() < 1e18 {
        format!("{}", n as i64)
    } else {
        // Default Rust f64 formatting: drops trailing zeros after the
        // decimal point for values like 0.05.
        format!("{n}")
    }
}

fn push_prop(buf: &mut String, name: &str, value: &str) {
    buf.push_str(name);
    buf.push_str(": ");
    buf.push_str(value);
    buf.push_str("; ");
}

#[cfg(test)]
mod tests {
    use super::super::desugar::desugar_style;
    use super::*;

    fn round_trip_property(tag: &str, css: &str) -> AttrMap {
        // Forward: CSS → numeric.
        let mut a = AttrMap::new();
        a.insert("style".into(), JsonValue::String(css.into()));
        desugar_style(tag, &mut a).expect("desugar");
        a
    }

    #[test]
    fn empty_attrs_returns_none() {
        let r = resugar_attrs("Component", &AttrMap::new());
        assert!(r.style.is_none());
        assert!(r.remaining.is_empty());
    }

    #[test]
    fn left_top_rotate_round_trip() {
        let attrs = round_trip_property(
            "Component",
            "position:absolute; left:30mm; top:5mm; rotate:90deg",
        );
        let r = resugar_attrs("Component", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("position: absolute"));
        assert!(s.contains("left: 30mm"));
        assert!(s.contains("top: 5mm"));
        assert!(s.contains("rotate: 90deg"));
    }

    #[test]
    fn subsystem_bbox_round_trip() {
        let attrs = round_trip_property(
            "Subsystem",
            "position:absolute; left:30mm; top:5mm; width:12mm; height:10mm; layer-band:F.Cu,In1.Cu",
        );
        let r = resugar_attrs("Subsystem", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("left: 30mm"));
        assert!(s.contains("top: 5mm"));
        assert!(s.contains("width: 12mm"));
        assert!(s.contains("height: 10mm"));
        assert!(s.contains("layer-band: F.Cu,In1.Cu"));
    }

    #[test]
    fn pad_size_round_trip() {
        let attrs = round_trip_property(
            "Pad",
            "position:absolute; left:0mm; top:0mm; width:0.5mm; height:0.5mm",
        );
        let r = resugar_attrs("Pad", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("left: 0mm"));
        assert!(s.contains("top: 0mm"));
        assert!(s.contains("width: 0.5mm"));
        assert!(s.contains("height: 0.5mm"));
    }

    #[test]
    fn clip_path_round_rect_round_trip() {
        let attrs = round_trip_property(
            "Pad",
            "position:absolute; left:0mm; top:0mm; width:0.5mm; height:0.5mm; clip-path: inset(0 0 0 0 round 0.05mm)",
        );
        let r = resugar_attrs("Pad", &attrs);
        let s = r.style.expect("style present");
        assert!(
            s.contains("clip-path: inset(0 0 0 0 round 0.05mm)"),
            "got: {s}"
        );
    }

    #[test]
    fn clip_path_circle_round_trip() {
        let attrs = round_trip_property(
            "Via",
            "position:absolute; left:2mm; top:2mm; clip-path: circle(0.3mm at 0mm 0mm)",
        );
        let r = resugar_attrs("Via", &attrs);
        let s = r.style.expect("style present");
        assert!(
            s.contains("clip-path: circle(0.3mm at 0mm 0mm)"),
            "got: {s}"
        );
    }

    #[test]
    fn clip_path_polygon_round_trip() {
        let attrs = round_trip_property(
            "Keepout",
            "position:absolute; left:0mm; top:-1mm; clip-path: polygon(0mm 0mm, 14mm 0mm, 14mm 5mm, 0mm 5mm)",
        );
        let r = resugar_attrs("Keepout", &attrs);
        let s = r.style.expect("style present");
        assert!(
            s.contains("clip-path: polygon(0mm 0mm, 14mm 0mm, 14mm 5mm, 0mm 5mm)"),
            "got: {s}"
        );
    }

    #[test]
    fn clip_path_path_quoted_round_trip() {
        let attrs = round_trip_property(
            "Trace",
            "position:absolute; left:0mm; top:0mm; clip-path: path('M 0 0 L 10 10 Z')",
        );
        let r = resugar_attrs("Trace", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("clip-path: path('M 0 0 L 10 10 Z')"), "got: {s}");
    }

    #[test]
    fn impedance_and_tolerance_round_trip() {
        let attrs = round_trip_property("Net", "impedance:50; tolerance:10");
        let r = resugar_attrs("Net", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("impedance: 50"));
        assert!(s.contains("tolerance: 10"));
    }

    #[test]
    fn via_drill_and_diameter_round_trip() {
        let attrs = round_trip_property(
            "Via",
            "from-layer:F.Cu; to-layer:In1.Cu; drill:0.3mm; diameter:0.6mm",
        );
        let r = resugar_attrs("Via", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("from-layer: F.Cu"));
        assert!(s.contains("to-layer: In1.Cu"));
        assert!(s.contains("drill: 0.3mm"));
        assert!(s.contains("diameter: 0.6mm"));
    }

    #[test]
    fn pin_layer_strict_round_trip() {
        let attrs = round_trip_property("Net", "pin-layer:strict");
        let r = resugar_attrs("Net", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("pin-layer: strict"));
    }

    #[test]
    fn pin_layer_prefer_round_trip() {
        let attrs = round_trip_property("Net", "pin-layer:prefer");
        let r = resugar_attrs("Net", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("pin-layer: prefer"));
    }

    #[test]
    fn scope_passthrough_round_trip() {
        let attrs = round_trip_property("Keepout", "scope:net-class(ground)");
        let r = resugar_attrs("Keepout", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("scope: net-class(ground)"));
    }

    #[test]
    fn cluster_gap_round_trip() {
        let attrs = round_trip_property("Subsystem", "display:flex; gap:0.5mm");
        let r = resugar_attrs("Subsystem", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("display: flex"));
        assert!(s.contains("gap: 0.5mm"));
    }

    #[test]
    fn z_index_round_trip() {
        let attrs = round_trip_property("Component", "z-index:2");
        let r = resugar_attrs("Component", &attrs);
        let s = r.style.expect("style present");
        assert!(s.contains("z-index: 2"));
    }

    #[test]
    fn remaining_passes_through_non_css_attrs() {
        let mut a = AttrMap::new();
        a.insert("footprint".into(), JsonValue::String("0402".into()));
        a.insert("id".into(), JsonValue::String("R1".into()));
        a.insert("at".into(), serde_json::json!({"x": 5.0, "y": 5.0}));
        let r = resugar_attrs("Component", &a);
        assert!(r.style.unwrap().contains("left: 5mm"));
        assert_eq!(
            r.remaining.get("footprint"),
            Some(&JsonValue::String("0402".into()))
        );
        assert_eq!(r.remaining.get("id"), Some(&JsonValue::String("R1".into())));
    }

    #[test]
    fn ir_injected_keys_filtered_from_remaining() {
        // The IR injects `component`, `_depth`, `_parentPath` into
        // every node's attrs. Those should NOT appear in
        // re-sugared output (they're an IR internal, not source
        // form).
        let mut a = AttrMap::new();
        a.insert("component".into(), JsonValue::String("Counter".into()));
        a.insert("_depth".into(), JsonValue::Number(0.into()));
        a.insert("_parentPath".into(), serde_json::json!(["counter"]));
        a.insert("footprint".into(), JsonValue::String("0402".into()));
        let r = resugar_attrs("Counter", &a);
        assert!(r.style.is_none());
        assert!(!r.remaining.contains_key("component"));
        assert!(!r.remaining.contains_key("_depth"));
        assert!(!r.remaining.contains_key("_parentPath"));
        assert!(r.remaining.contains_key("footprint"));
    }

    /// PCBPnR Phase 5-D §3 round-trip property gate — desugar then
    /// resugar then re-desugar must produce identical numeric forms.
    /// Tests the inverse-function property on the multi-property
    /// case the LLM is most likely to emit.
    #[test]
    fn full_round_trip_idempotency() {
        let original = "position:absolute; left:30mm; top:5mm; rotate:90deg; layer:F.Cu";
        let first = round_trip_property("Component", original);

        // Resugar the post-desugar AttrMap → CSS string → re-desugar.
        let r = resugar_attrs("Component", &first);
        let css = r.style.expect("style present");
        let mut second_attrs = r.remaining;
        second_attrs.insert("style".into(), JsonValue::String(css));
        desugar_style("Component", &mut second_attrs).expect("re-desugar");

        // The typed compound objects must match.
        assert_eq!(first.get("at"), second_attrs.get("at"));
        assert_eq!(first.get("layer"), second_attrs.get("layer"));
    }

    #[test]
    fn full_round_trip_with_clip_path() {
        let original =
            "position:absolute; left:0mm; top:0mm; width:0.5mm; height:0.5mm; clip-path: inset(0 0 0 0 round 0.05mm)";
        let first = round_trip_property("Pad", original);
        let r = resugar_attrs("Pad", &first);
        let css = r.style.expect("style present");
        let mut second_attrs = r.remaining;
        second_attrs.insert("style".into(), JsonValue::String(css));
        desugar_style("Pad", &mut second_attrs).expect("re-desugar");

        // Round-trip on size + geometry.
        assert_eq!(first.get("size"), second_attrs.get("size"));
        assert_eq!(first.get("geometry"), second_attrs.get("geometry"));
    }

    #[test]
    fn property_order_is_canonical_and_stable() {
        // Two AttrMaps with identical content should resugar to byte-
        // identical strings — the emit order is deterministic.
        let a1 = round_trip_property(
            "Component",
            "position:absolute; left:30mm; top:5mm; layer:F.Cu; rotate:90deg",
        );
        let r1 = resugar_attrs("Component", &a1);
        let r2 = resugar_attrs("Component", &a1);
        assert_eq!(r1.style, r2.style);
    }
}
