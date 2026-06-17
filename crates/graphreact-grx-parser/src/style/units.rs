//! CSS length-unit conversion.
//!
//! v1 accepted `mm` only and rejected every other unit with
//! `StyleError::BadUnit`. Cleanup-Pass item `m115-css-units` adds
//! support for the four CSS-standard length units that any 2D-layout
//! consumer is likely to use: `mm`, `cm`, `in`, `px`.
//!
//! The implementation is intentionally general — the style module is
//! reused across PCBPnR + future 2D-layout consumers, so unit support
//! is a property of the language, not the PCB domain. Internal
//! representation stays in millimetres (downstream `geometry={...}`
//! attrs are mm-typed); conversion happens at parse time.
//!
//! Conversion factors (CSS-standard, also matches the SVG spec):
//!
//! | Unit | Factor (× to get mm) |
//! |------|----------------------|
//! | `mm` | 1.0                  |
//! | `cm` | 10.0                 |
//! | `in` | 25.4                 |
//! | `px` | 25.4 / 96            |  (CSS reference px at 96 DPI)
//!
//! Bare numbers (no unit suffix) are still rejected — users must
//! specify a unit explicitly so unit confusion never silently
//! propagates into the geometry.

/// Inch → mm. CSS / SVG / IPC all agree on this constant.
const IN_TO_MM: f64 = 25.4;
/// CSS reference px (1/96 inch).
const PX_TO_MM: f64 = IN_TO_MM / 96.0;
/// cm → mm.
const CM_TO_MM: f64 = 10.0;

/// Parse a CSS length value to mm. Returns `None` if `value` has no
/// recognised unit suffix or the numeric portion fails to parse;
/// callers wrap the `None` with their own [`StyleError`] variant so
/// the error carries property/context info.
///
/// Recognised suffixes: `mm`, `cm`, `in`, `px`. Leading/trailing
/// whitespace is tolerated; whitespace between number and unit is
/// also tolerated (`"10 mm"` parses).
#[must_use]
pub fn parse_length_mm(value: &str) -> Option<f64> {
    let v = value.trim();
    // Suffix order: longer suffixes first so "px" doesn't shadow a
    // future "pxh"-style extension; and "mm" / "cm" before bare "m"
    // (none today, but defensive). Among current suffixes order
    // doesn't actually matter since none is a prefix of another.
    for (suffix, factor) in [
        ("mm", 1.0),
        ("cm", CM_TO_MM),
        ("in", IN_TO_MM),
        ("px", PX_TO_MM),
    ] {
        if let Some(num) = v.strip_suffix(suffix) {
            let n: f64 = num.trim().parse().ok()?;
            return Some(n * factor);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_passthrough() {
        assert_eq!(parse_length_mm("10mm"), Some(10.0));
        assert_eq!(parse_length_mm("0mm"), Some(0.0));
        assert_eq!(parse_length_mm("-5mm"), Some(-5.0));
        assert_eq!(parse_length_mm("3.5mm"), Some(3.5));
    }

    #[test]
    fn cm_to_mm() {
        assert_eq!(parse_length_mm("1cm"), Some(10.0));
        assert_eq!(parse_length_mm("2.54cm"), Some(25.4));
    }

    #[test]
    fn in_to_mm() {
        assert_eq!(parse_length_mm("1in"), Some(25.4));
        assert_eq!(parse_length_mm("0.5in"), Some(12.7));
    }

    #[test]
    fn px_to_mm() {
        // 96 CSS px = 1 inch = 25.4 mm exactly.
        let v = parse_length_mm("96px").unwrap();
        assert!((v - 25.4).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn whitespace_tolerance() {
        assert_eq!(parse_length_mm("  10mm  "), Some(10.0));
        assert_eq!(parse_length_mm("10 mm"), Some(10.0));
    }

    #[test]
    fn unrecognized_unit_returns_none() {
        assert_eq!(parse_length_mm("10pt"), None);
        assert_eq!(parse_length_mm("10em"), None);
        assert_eq!(parse_length_mm("10%"), None);
    }

    #[test]
    fn bare_number_returns_none() {
        assert_eq!(parse_length_mm("10"), None);
        assert_eq!(parse_length_mm("10.5"), None);
    }

    #[test]
    fn malformed_number_returns_none() {
        assert_eq!(parse_length_mm("abcmm"), None);
        assert_eq!(parse_length_mm("--5mm"), None);
    }
}
