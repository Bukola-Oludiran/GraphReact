//! Converters between parser-side [`SourceLoc`] (1-indexed line/column
//! in **UTF-8 bytes**) and LSP-side [`Position`] / [`Range`] (0-indexed
//! line/column in UTF-8 code units, per the encoding we negotiate in
//! `initialize`).
//!
//! ## Encoding contract
//!
//! The parser's [`SourceLoc`] column is byte-indexed: the lexer's
//! `advance()` shim ticks `column` once per raw `u8` consumed
//! (`crates/graphreact-grx-parser/src/lexer.rs::advance`). The server
//! advertises `PositionEncodingKind::Utf8` in `initialize`, which
//! defines `Position.character` as a 0-indexed byte offset within the
//! line. So the conversion is a pure index-base shift (1→0) — no
//! `char_indices()` walk, no UTF-16 surrogate accounting.
//!
//! Modern editors (VS Code 1.74+, Neovim 0.11+, Helix, Zed) honour the
//! advertisement. Older clients that only support UTF-16 would see
//! mis-pinned diagnostics on multibyte lines; tracked under M23.next.

use graphreact_grx_parser::SourceLoc;
use tower_lsp_server::ls_types::{Position, Range};

/// Convert a 1-indexed `SourceLoc` to a 0-indexed LSP `Position`.
/// Both inputs and outputs measure columns in UTF-8 bytes, so this is
/// a pure index-base shift.
#[must_use]
pub fn loc_to_position(loc: SourceLoc, _source: &str) -> Position {
    let line_idx = loc.line.saturating_sub(1);
    let col_idx = loc.column.saturating_sub(1);
    Position::new(line_idx as u32, col_idx as u32)
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
    fn position_passes_byte_columns_through() {
        // Parser's column is byte-indexed (lexer ticks per u8). A
        // SourceLoc captured on the `x` after `→` (3 bytes) would
        // arrive as column 4, NOT column 2. Round-trip:
        let src = "→x\nfoo";
        let p = loc_to_position(SourceLoc::new(1, 4), src);
        assert_eq!(p.line, 0);
        assert_eq!(p.character, 3); // 0-indexed byte column of `x`.
    }

    #[test]
    fn token_range_widens_identifier() {
        let src = "<Counter id=\"c\"/>";
        // Lexer byte-column of 'C' = 2 (1-indexed: `<` at 1, `C` at 2).
        let r = token_range_at(SourceLoc::new(1, 2), src);
        assert_eq!(r.start.line, 0);
        assert_eq!(r.start.character, 1);
        assert_eq!(r.end.character, 8); // "Counter" ends at byte 8.
    }

    #[test]
    fn position_to_byte_offset_round_trips_ascii() {
        let src = "abc\ndef";
        let off = position_to_byte_offset(src, Position::new(1, 1));
        assert_eq!(off, 5); // 'e'
    }
}
