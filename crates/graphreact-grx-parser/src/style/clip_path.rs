//! PCBPnR Phase 5-D §3 — `clip-path` sub-grammar.
//!
//! Parses the 7 supported clip-path shapes and produces a
//! `geometry={kind:..., ...}`-shaped JSON value matching PCBPnR's
//! `pcb_substrate::Geometry` (serde tag = "kind", rename_all =
//! snake_case).
//!
//! Variants:
//! - `inset(0 0 0 0)` → `{kind: "rect", w, h}` (requires width/height siblings)
//! - `inset(0 0 0 0 round Rmm)` → `{kind: "round_rect", w, h, r}` (same)
//! - `circle(Rmm at Xmm Ymm)` → `{kind: "circle", r, cx, cy}`
//! - `ellipse(Rxmm Rymm at Xmm Ymm)` → `{kind: "ellipse", rx, ry, cx, cy}`
//! - `polygon(p1, p2, ...)` → `{kind: "polygon", points: [{x, y}, ...]}`
//! - `path('M ... L ... Z')` → `{kind: "path", svg_d: "..."}`
//! - `kicad-shape("name", {...})` → `{kind: "native", source: "kicad", data: {...}}`

use serde_json::Value as JsonValue;

use super::error::StyleError;

/// Parsed `geometry` JSON value for a clip-path expression.
///
/// `inset(...)` variants don't carry width/height themselves — the
/// caller (desugarer) must supply them from sibling `width:` /
/// `height:` properties before producing the final `geometry`
/// AttrMap entry.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ClipPath {
    /// `inset(0 0 0 0)` — needs w, h from siblings.
    Rect,
    /// `inset(0 0 0 0 round Rmm)` — needs w, h from siblings.
    RoundRect { r: f64 },
    Circle { r: f64, cx: f64, cy: f64 },
    Ellipse { rx: f64, ry: f64, cx: f64, cy: f64 },
    Polygon { points: Vec<(f64, f64)> },
    Path { svg_d: String },
    Kicad { name: String, data: JsonValue },
}

impl ClipPath {
    /// Render to the final `geometry={kind:..., ...}` JSON, given
    /// optional width/height for `inset(...)` variants.
    pub(super) fn to_json(
        &self,
        width: Option<f64>,
        height: Option<f64>,
    ) -> Result<JsonValue, StyleError> {
        let mut obj = serde_json::Map::new();
        match self {
            Self::Rect => {
                let (w, h) = match (width, height) {
                    (Some(w), Some(h)) => (w, h),
                    _ => return Err(StyleError::InsetMissingWidthHeight),
                };
                obj.insert("kind".into(), JsonValue::String("rect".into()));
                obj.insert("w".into(), json_num(w));
                obj.insert("h".into(), json_num(h));
            }
            Self::RoundRect { r } => {
                let (w, h) = match (width, height) {
                    (Some(w), Some(h)) => (w, h),
                    _ => return Err(StyleError::InsetMissingWidthHeight),
                };
                obj.insert("kind".into(), JsonValue::String("round_rect".into()));
                obj.insert("w".into(), json_num(w));
                obj.insert("h".into(), json_num(h));
                obj.insert("r".into(), json_num(*r));
            }
            Self::Circle { r, cx, cy } => {
                obj.insert("kind".into(), JsonValue::String("circle".into()));
                obj.insert("r".into(), json_num(*r));
                obj.insert("cx".into(), json_num(*cx));
                obj.insert("cy".into(), json_num(*cy));
            }
            Self::Ellipse { rx, ry, cx, cy } => {
                obj.insert("kind".into(), JsonValue::String("ellipse".into()));
                obj.insert("rx".into(), json_num(*rx));
                obj.insert("ry".into(), json_num(*ry));
                obj.insert("cx".into(), json_num(*cx));
                obj.insert("cy".into(), json_num(*cy));
            }
            Self::Polygon { points } => {
                obj.insert("kind".into(), JsonValue::String("polygon".into()));
                let pts: Vec<JsonValue> = points
                    .iter()
                    .map(|(x, y)| {
                        let mut p = serde_json::Map::new();
                        p.insert("x".into(), json_num(*x));
                        p.insert("y".into(), json_num(*y));
                        JsonValue::Object(p)
                    })
                    .collect();
                obj.insert("points".into(), JsonValue::Array(pts));
            }
            Self::Path { svg_d } => {
                obj.insert("kind".into(), JsonValue::String("path".into()));
                obj.insert("svg_d".into(), JsonValue::String(svg_d.clone()));
            }
            Self::Kicad { name, data } => {
                obj.insert("kind".into(), JsonValue::String("native".into()));
                obj.insert("source".into(), JsonValue::String("kicad".into()));
                let mut data_obj = serde_json::Map::new();
                data_obj.insert("name".into(), JsonValue::String(name.clone()));
                if let JsonValue::Object(inner) = data {
                    for (k, v) in inner {
                        data_obj.insert(k.clone(), v.clone());
                    }
                }
                obj.insert("data".into(), JsonValue::Object(data_obj));
            }
        }
        Ok(JsonValue::Object(obj))
    }
}

fn json_num(n: f64) -> JsonValue {
    serde_json::Number::from_f64(n)
        .map(JsonValue::Number)
        .unwrap_or(JsonValue::Null)
}

/// Parse a `clip-path: …` value into a [`ClipPath`] variant.
pub(super) fn parse_clip_path(value: &str) -> Result<ClipPath, StyleError> {
    let value = value.trim();
    if let Some(rest) = strip_prefix(value, "inset(") {
        return parse_inset(rest);
    }
    if let Some(rest) = strip_prefix(value, "circle(") {
        return parse_circle(rest);
    }
    if let Some(rest) = strip_prefix(value, "ellipse(") {
        return parse_ellipse(rest);
    }
    if let Some(rest) = strip_prefix(value, "polygon(") {
        return parse_polygon(rest);
    }
    if let Some(rest) = strip_prefix(value, "path(") {
        return parse_path(rest);
    }
    if let Some(rest) = strip_prefix(value, "kicad-shape(") {
        return parse_kicad(rest);
    }
    Err(StyleError::MalformedClipPath {
        snippet: value.to_string(),
    })
}

fn strip_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.strip_prefix(prefix).and_then(|rest| rest.strip_suffix(')'))
}

/// `inset(0 0 0 0)` or `inset(0 0 0 0 round Rmm)` — we ignore the 4
/// inset values (they're always 0 for our use case; the rect fills
/// the parent's width/height) and only extract the optional round
/// radius.
fn parse_inset(body: &str) -> Result<ClipPath, StyleError> {
    let body = body.trim();
    // Look for ` round Rmm` suffix
    if let Some(round_idx) = body.find(" round ") {
        let _inset_part = &body[..round_idx];
        let radius_part = body[round_idx + " round ".len()..].trim();
        let r = parse_mm(radius_part, "clip-path inset round")?;
        Ok(ClipPath::RoundRect { r })
    } else {
        Ok(ClipPath::Rect)
    }
}

/// `circle(Rmm at Xmm Ymm)`
fn parse_circle(body: &str) -> Result<ClipPath, StyleError> {
    let body = body.trim();
    let Some(at_idx) = body.find(" at ") else {
        return Err(StyleError::MalformedClipPath {
            snippet: format!("circle({body}) — expected ` at X Y` after radius"),
        });
    };
    let r = parse_mm(body[..at_idx].trim(), "clip-path circle radius")?;
    let coords = body[at_idx + " at ".len()..].trim();
    let (cx, cy) = parse_xy(coords, "clip-path circle center")?;
    Ok(ClipPath::Circle { r, cx, cy })
}

/// `ellipse(Rxmm Rymm at Xmm Ymm)`
fn parse_ellipse(body: &str) -> Result<ClipPath, StyleError> {
    let body = body.trim();
    let Some(at_idx) = body.find(" at ") else {
        return Err(StyleError::MalformedClipPath {
            snippet: format!("ellipse({body}) — expected ` at X Y` after radii"),
        });
    };
    let radii_part = body[..at_idx].trim();
    let (rx, ry) = parse_xy(radii_part, "clip-path ellipse radii")?;
    let coords = body[at_idx + " at ".len()..].trim();
    let (cx, cy) = parse_xy(coords, "clip-path ellipse center")?;
    Ok(ClipPath::Ellipse { rx, ry, cx, cy })
}

/// `polygon(X1mm Y1mm, X2mm Y2mm, ...)`
fn parse_polygon(body: &str) -> Result<ClipPath, StyleError> {
    let mut points: Vec<(f64, f64)> = Vec::new();
    for pair in body.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (x, y) = parse_xy(pair, "clip-path polygon point")?;
        points.push((x, y));
    }
    if points.is_empty() {
        return Err(StyleError::MalformedClipPath {
            snippet: format!("polygon({body}) — at least one point required"),
        });
    }
    Ok(ClipPath::Polygon { points })
}

/// `path('M ... L ... Z')` — quoted SVG path string passes through verbatim.
fn parse_path(body: &str) -> Result<ClipPath, StyleError> {
    let body = body.trim();
    let trimmed = body
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| body.strip_prefix('"').and_then(|s| s.strip_suffix('"')));
    let svg_d = match trimmed {
        Some(s) => s.to_string(),
        None => {
            return Err(StyleError::MalformedClipPath {
                snippet: format!("path({body}) — SVG d-string must be quoted"),
            });
        }
    };
    Ok(ClipPath::Path { svg_d })
}

/// `kicad-shape("name", {...})` — the JSON-shaped data block parses as JSON.
fn parse_kicad(body: &str) -> Result<ClipPath, StyleError> {
    let body = body.trim();
    let Some(comma_idx) = body.find(',') else {
        return Err(StyleError::MalformedClipPath {
            snippet: format!("kicad-shape({body}) — expected `name, {{...}}`"),
        });
    };
    let name_part = body[..comma_idx].trim();
    let data_part = body[comma_idx + 1..].trim();
    let name = name_part
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| StyleError::MalformedClipPath {
            snippet: format!("kicad-shape({body}) — name must be quoted"),
        })?
        .to_string();
    let data: JsonValue = serde_json::from_str(data_part).map_err(|_| {
        StyleError::MalformedClipPath {
            snippet: format!("kicad-shape data block is not valid JSON: {data_part}"),
        }
    })?;
    Ok(ClipPath::Kicad { name, data })
}

/// Parse `Nmm` → f64. Rejects bare numbers and non-mm units.
fn parse_mm(s: &str, context: &'static str) -> Result<f64, StyleError> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix("mm") {
        num.trim().parse::<f64>().map_err(|_| StyleError::BadNumber {
            property: context.to_string(),
            value: s.to_string(),
        })
    } else {
        Err(StyleError::BadUnit {
            property: context.to_string(),
            value: s.to_string(),
        })
    }
}

/// Parse `Xmm Ymm` → (x, y).
fn parse_xy(s: &str, context: &'static str) -> Result<(f64, f64), StyleError> {
    let s = s.trim();
    let mut parts = s.split_whitespace();
    let x = parts
        .next()
        .ok_or_else(|| StyleError::MalformedClipPath {
            snippet: format!("{context} expected `X Y`, got `{s}`"),
        })?;
    let y = parts
        .next()
        .ok_or_else(|| StyleError::MalformedClipPath {
            snippet: format!("{context} expected `X Y`, got `{s}`"),
        })?;
    if parts.next().is_some() {
        return Err(StyleError::MalformedClipPath {
            snippet: format!("{context} too many parts: `{s}`"),
        });
    }
    Ok((parse_mm(x, context)?, parse_mm(y, context)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inset_rect() {
        let cp = parse_clip_path("inset(0 0 0 0)").expect("ok");
        assert_eq!(cp, ClipPath::Rect);
    }

    #[test]
    fn parses_inset_round_rect() {
        let cp = parse_clip_path("inset(0 0 0 0 round 0.05mm)").expect("ok");
        assert_eq!(cp, ClipPath::RoundRect { r: 0.05 });
    }

    #[test]
    fn parses_circle() {
        let cp = parse_clip_path("circle(0.3mm at 0mm 0mm)").expect("ok");
        assert_eq!(cp, ClipPath::Circle { r: 0.3, cx: 0.0, cy: 0.0 });
    }

    #[test]
    fn parses_ellipse() {
        let cp = parse_clip_path("ellipse(1mm 2mm at 5mm 6mm)").expect("ok");
        assert_eq!(cp, ClipPath::Ellipse { rx: 1.0, ry: 2.0, cx: 5.0, cy: 6.0 });
    }

    #[test]
    fn parses_polygon() {
        let cp =
            parse_clip_path("polygon(0mm 0mm, 14mm 0mm, 14mm 5mm, 0mm 5mm)").expect("ok");
        match cp {
            ClipPath::Polygon { points } => {
                assert_eq!(points.len(), 4);
                assert_eq!(points[0], (0.0, 0.0));
                assert_eq!(points[2], (14.0, 5.0));
            }
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn parses_path_quoted() {
        let cp = parse_clip_path("path('M 0 0 L 10 10 Z')").expect("ok");
        assert_eq!(
            cp,
            ClipPath::Path {
                svg_d: "M 0 0 L 10 10 Z".into()
            }
        );
    }

    #[test]
    fn parses_kicad_shape() {
        let cp = parse_clip_path(r#"kicad-shape("chamfered", {"chamfer":0.1})"#)
            .expect("ok");
        match cp {
            ClipPath::Kicad { name, data } => {
                assert_eq!(name, "chamfered");
                assert_eq!(data, serde_json::json!({"chamfer": 0.1}));
            }
            other => panic!("expected Kicad, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_unit_in_circle() {
        let err = parse_clip_path("circle(0.3px at 0mm 0mm)").expect_err("must reject");
        assert!(matches!(err, StyleError::BadUnit { .. }));
    }

    #[test]
    fn rejects_unknown_shape() {
        let err = parse_clip_path("trapezoid(...)").expect_err("must reject");
        assert!(matches!(err, StyleError::MalformedClipPath { .. }));
    }

    #[test]
    fn round_rect_to_json_with_dims() {
        let cp = ClipPath::RoundRect { r: 0.05 };
        let json = cp.to_json(Some(0.5), Some(0.5)).expect("ok");
        assert_eq!(json["kind"], "round_rect");
        assert_eq!(json["w"], 0.5);
        assert_eq!(json["h"], 0.5);
        assert_eq!(json["r"], 0.05);
    }

    #[test]
    fn rect_without_dims_errors() {
        let cp = ClipPath::Rect;
        let err = cp.to_json(None, None).expect_err("must error");
        assert!(matches!(err, StyleError::InsetMissingWidthHeight));
    }
}
