//! PCBPnR Phase 5-D §3 — CSS-shape property tokenizer.
//!
//! Splits `style="left:30mm; rotate:90deg; layer:F.Cu"` into
//! `[(name, value)]` pairs. Handles:
//! - `/* … */` block comments (stripped at tokenization)
//! - case-insensitive property names (lower-cased on emit)
//! - whitespace trimming around `:` and `;`
//! - duplicate properties (last-wins, matches CSS)
//! - semicolon-separated entries; trailing semicolon optional
//!
//! `//` line comments are NOT CSS-standard and are rejected.

use super::error::StyleError;

/// Parse a `style="…"` string body into `(property, value)` pairs.
/// Property names are lower-cased; values are trimmed of leading/
/// trailing whitespace. Duplicates use last-wins semantics (matches
/// CSS).
pub fn tokenize_style(input: &str) -> Result<Vec<(String, String)>, StyleError> {
    let stripped = strip_block_comments(input)?;
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in stripped.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some(colon_idx) = entry.find(':') else {
            return Err(StyleError::MalformedClipPath {
                snippet: entry.to_string(),
            });
        };
        let name = entry[..colon_idx].trim().to_ascii_lowercase();
        let value = entry[colon_idx + 1..].trim().to_string();
        // Last-wins on duplicates — match CSS cascade semantics.
        if let Some(existing) = out.iter_mut().find(|(n, _)| n == &name) {
            existing.1 = value;
        } else {
            out.push((name, value));
        }
    }
    Ok(out)
}

/// Strip `/* … */` block comments. Rejects `//` (not CSS-standard).
fn strip_block_comments(input: &str) -> Result<String, StyleError> {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Skip to closing */
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            if j + 1 >= bytes.len() {
                return Err(StyleError::MalformedClipPath {
                    snippet: "unterminated /* comment".to_string(),
                });
            }
            i = j + 2;
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            return Err(StyleError::MalformedClipPath {
                snippet: "`//` line comments are not CSS-standard; use `/* … */`".to_string(),
            });
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_single_property() {
        let result = tokenize_style("left: 30mm").expect("ok");
        assert_eq!(result, vec![("left".to_string(), "30mm".to_string())]);
    }

    #[test]
    fn tokenizes_multiple_properties() {
        let result = tokenize_style("position: absolute; left: 30mm; top: 5mm").expect("ok");
        assert_eq!(
            result,
            vec![
                ("position".to_string(), "absolute".to_string()),
                ("left".to_string(), "30mm".to_string()),
                ("top".to_string(), "5mm".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_case_insensitive_names() {
        let result = tokenize_style("POSITION: absolute; LEFT: 30mm").expect("ok");
        assert_eq!(result[0].0, "position");
        assert_eq!(result[1].0, "left");
    }

    #[test]
    fn strips_block_comments() {
        let result = tokenize_style("left:30mm; /* TODO: review */ layer:F.Cu").expect("ok");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "left");
        assert_eq!(result[1].0, "layer");
    }

    #[test]
    fn last_wins_on_duplicates() {
        let result = tokenize_style("left: 30mm; left: 50mm").expect("ok");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, "50mm");
    }

    #[test]
    fn rejects_double_slash_comments() {
        let err =
            tokenize_style("left: 30mm; // not allowed\nlayer: F.Cu").expect_err("must reject");
        assert!(matches!(err, StyleError::MalformedClipPath { .. }));
    }

    #[test]
    fn trailing_semicolon_ignored() {
        let result = tokenize_style("left: 30mm;").expect("ok");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn empty_input_returns_empty() {
        let result = tokenize_style("").expect("ok");
        assert!(result.is_empty());
    }

    #[test]
    fn whitespace_only_input_returns_empty() {
        let result = tokenize_style("   ").expect("ok");
        assert!(result.is_empty());
    }
}
