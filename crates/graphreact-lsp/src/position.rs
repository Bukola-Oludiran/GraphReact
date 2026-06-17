//! Converters between parser-side [`SourceLoc`] (1-indexed line/column
//! in characters) and LSP-side [`Position`] / [`Range`] (0-indexed
//! line/column in UTF-8 code units).
//!
//! The server advertises `PositionEncodingKind::Utf8` in the
//! `initialize` handshake so we can stay in byte space and avoid the
//! UTF-16 conversion tax the risks list flagged. Newer LSP clients
//! (VS Code 1.74+, Neovim 0.11+, Helix, Zed) honour the advertisement;
//! older clients fall back to UTF-16 which we approximate by
//! treating the character index as a code-unit count — good enough for
//! ASCII-only `.grx`, mildly off for fixtures that embed `→` or `🦀`
//! glyphs in definition bodies. The risks doc already calls this out.

use graphreact_grx_parser::SourceLoc;
use tower_lsp_server::ls_types::{Position, Range};

/// Convert a 1-indexed `SourceLoc` to a 0-indexed LSP `Position`.
/// The column is mapped to UTF-8 byte offset of the line by walking
/// the source string when supplied; without source we degrade to the
/// raw column value (correct for ASCII-only inputs).
#[must_use]
pub fn loc_to_position(loc: SourceLoc, source: &str) -> Position {
    let line_idx = loc.line.saturating_sub(1);
    let col_idx = loc.column.saturating_sub(1);
    let byte_col = find_byte_col_for_char_col(source, line_idx, col_idx);
    Position::new(line_idx as u32, byte_col as u32)
}

/// Build a zero-width LSP `Range` at the given source location. Most
/// of our diagnostics + hover handlers want this — they pin a caret
/// to a tag and let the editor widen via word-boundary heuristics.
#[must_use]
pub fn loc_to_zero_range(loc: SourceLoc, source: &str) -> Range {
    let pos = loc_to_position(loc, source);
    Range::new(pos, pos)
}

/// Build an LSP `Range` covering `[start_loc, end_loc)`. Used when we
/// want the range to span a token rather than just point at it (e.g.
/// the splice-cycle diagnostic highlights the chain text).
#[must_use]
pub fn locs_to_range(start: SourceLoc, end: SourceLoc, source: &str) -> Range {
    Range::new(loc_to_position(start, source), loc_to_position(end, source))
}

/// Widen a zero-width range to cover the identifier-shaped word at
/// the given offset. Most LSP clients render single-character ranges
/// as tiny markers; widening to the token gives a visible underline
/// on diagnostics.
#[must_use]
pub fn token_range_at(loc: SourceLoc, source: &str) -> Range {
    let start = loc_to_position(loc, source);
    // Find the end column of the token starting at this byte offset.
    let line_text = nth_line(source, start.line as usize).unwrap_or("");
    let col = start.character as usize;
    let bytes = line_text.as_bytes();
    let mut end = col;
    while end < bytes.len() {
        let b = bytes[end];
        // Token characters: ASCII alphanumeric, `_`, `-`, `:`, `.`,
        // `/`. Stops at whitespace, quotes, angle brackets.
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.' | b'/') {
            end += 1;
        } else {
            break;
        }
    }
    let end_pos = Position::new(start.line, end as u32);
    Range::new(start, end_pos)
}

/// Given a 0-indexed `(line, char_col)`, return the byte offset of the
/// `char_col`-th character on that line of `source`. Used to translate
/// 1-indexed line/column-in-characters (the parser's units) into
/// LSP-compatible byte columns.
fn find_byte_col_for_char_col(source: &str, line_idx: usize, char_col: usize) -> usize {
    let Some(line) = nth_line(source, line_idx) else {
        return char_col;
    };
    let mut bytes_seen = 0usize;
    for (i, ch) in line.char_indices() {
        if bytes_seen == char_col {
            return i;
        }
        bytes_seen += 1;
        let _ = ch;
    }
    // char_col is past end of line — clamp to line byte length.
    line.len().min(char_col)
}

/// Return the `n`-th 0-indexed line of `source`, without the trailing
/// newline. None when out of range.
fn nth_line(source: &str, n: usize) -> Option<&str> {
    source.lines().nth(n)
}

/// Map a 0-indexed LSP `Position` back to a byte offset inside
/// `source`. Useful when we need to inspect the character under the
/// cursor for hover / completion / goto-definition.
#[must_use]
pub fn position_to_byte_offset(source: &str, pos: Position) -> usize {
    let mut byte = 0usize;
    let mut line_no = 0u32;
    for line in source.split_inclusive('\n') {
        if line_no == pos.line {
            let col = pos.character as usize;
            // pos.character is already a byte column (server uses
            // UTF-8 encoding); clamp to the line length.
            return byte + col.min(line.len());
        }
        byte += line.len();
        line_no += 1;
    }
    source.len().min(byte + pos.character as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_zero_indexes() {
        let p = loc_to_position(SourceLoc::new(1, 1), "abc");
        assert_eq!(p.line, 0);
        assert_eq!(p.character, 0);
    }

    #[test]
    fn position_handles_multibyte_lines() {
        let src = "→x\nfoo";
        // line 1 col 2 should land just after the arrow's 3 bytes.
        let p = loc_to_position(SourceLoc::new(1, 2), src);
        assert_eq!(p.line, 0);
        assert_eq!(p.character, 3);
    }

    #[test]
    fn token_range_widens_identifier() {
        let src = "<Counter id=\"c\"/>";
        // Caret at col 2 (= char position of 'C').
        let r = token_range_at(SourceLoc::new(1, 2), src);
        assert_eq!(r.start.line, 0);
        assert_eq!(r.start.character, 1);
        assert_eq!(r.end.character, 8); // "Counter" ends at col 8
    }

    #[test]
    fn position_to_byte_offset_round_trips_ascii() {
        let src = "abc\ndef";
        let off = position_to_byte_offset(src, Position::new(1, 1));
        assert_eq!(off, 5); // 'e'
    }
}
