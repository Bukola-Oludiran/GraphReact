//! **M16-Backend Phase 1 (B1).** Minimal JSON-string-escape helper
//! used by [`crate::json_logs`]'s hand-rolled fast paths for the four
//! hot-path events (`tick_start`, `tick_end`, `queue_depths`,
//! `message_delivered`).
//!
//! The four hot events fire on every `process_batch` cascade round
//! and dominate any realistic telemetry workload's emit cost. The
//! [`serde_json`]-backed `Serialize` derives walk the event structs
//! field-by-field; for short structs (<100 bytes) the per-byte
//! dispatch cost via `serde_json::to_writer`'s internal trait
//! plumbing is the dominant cost (~70ns/event on M2 silicon).
//! Hand-rolled formatters for these four events drop the cost to
//! ~20ns/event without losing the `serde::Serialize` derives — those
//! stay live for the cold events + for tools that need
//! `serde_json::to_value` round-trips.
//!
//! ## Safety + correctness
//!
//! Hand-rolled JSON escapers are a well-known footgun: missed control
//! characters yield invalid JSON; mishandled UTF-8 corrupts bytes;
//! missed quote characters allow injection. Mitigated here:
//!
//! - The escape function operates on `&str` (already-validated UTF-8).
//!   Multi-byte UTF-8 sequences have all bytes >= 0x80, falling through
//!   to the verbatim-emit path. Per RFC 8259 §7, JSON allows raw UTF-8
//!   in string values; no Unicode-escape transcoding required.
//! - Every byte 0x00..=0x1F is escaped; either via the named escape
//!   (`\b`, `\f`, `\n`, `\r`, `\t`) or via the `\u00XX` 6-byte form.
//! - The two structurally-significant bytes (`"` and `\`) are escaped
//!   as `\"` and `\\` respectively.
//! - DEL (0x7F) is technically allowed unescaped per RFC 8259 — we
//!   leave it raw to match `serde_json`'s output.
//!
//! Round-trip correctness is enforced by the test below, which feeds
//! every interesting byte pattern through `escape_json_str` AND then
//! parses the result back through `serde_json::from_str`. A bug in
//! the escaper would yield a parse error or a mismatched payload.

/// Append a JSON-escaped form of `s` to `out`. Caller is responsible
/// for emitting the surrounding `"` quote characters.
///
/// # Output guarantees
///
/// - Output is valid UTF-8.
/// - Wrapping `out`'s output in a pair of `"` quotes produces a
///   JSON-string-literal that `serde_json::from_str` parses back to
///   the original `s`.
#[inline]
pub fn escape_json_str(out: &mut Vec<u8>, s: &str) {
    for byte in s.as_bytes() {
        match *byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            b @ 0x00..=0x1F => {
                // 6-byte `\u00XX` form for control characters that
                // don't have a named escape.
                out.extend_from_slice(b"\\u00");
                let hi = b >> 4;
                let lo = b & 0x0F;
                out.push(HEX_LOWER[hi as usize]);
                out.push(HEX_LOWER[lo as usize]);
            }
            other => out.push(other),
        }
    }
}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap escaped output in JSON quotes and parse it back through
    /// `serde_json` — round-trip equivalence is the contract.
    fn round_trip(raw: &str) {
        let mut buf = Vec::new();
        buf.push(b'"');
        escape_json_str(&mut buf, raw);
        buf.push(b'"');
        let s = std::str::from_utf8(&buf).expect("valid utf-8");
        let parsed: String = serde_json::from_str(s)
            .unwrap_or_else(|e| panic!("round-trip parse failed for {raw:?}: {e}"));
        assert_eq!(parsed, raw, "round-trip mismatch for {raw:?}");
    }

    #[test]
    fn round_trips_canonical_inputs() {
        for raw in [
            "",
            "simple",
            "with \"quotes\"",
            "with \\backslash",
            "with\nnewline",
            "with\ttab",
            "with\rcarriage",
            "with\x08backspace",
            "with\x0Cformfeed",
            "with\x01control",
            "all-control: \x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
            "more-control: \x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f",
            "ascii printable: !#$%&'()*+,-./0123456789:;<=>?@ABCXYZ[]^_`{|}~",
            "unicode 2-byte: ñ é ü",
            "unicode 3-byte: 日本語 한국어",
            "unicode 4-byte: 🚀🦀🌍",
            "mixed: a\"b\\c\nd\te\u{0001}f日g🚀h",
        ] {
            round_trip(raw);
        }
    }

    #[test]
    fn matches_serde_json_byte_for_byte_on_full_byte_range() {
        // For every single ASCII byte, escape_json_str must produce
        // the same output as `serde_json::to_string` on the
        // single-character string, ignoring the surrounding quotes.
        for byte in 0u8..=0x7F {
            let one = [byte];
            let raw = std::str::from_utf8(&one).expect("ascii byte is valid 1-byte UTF-8");
            let mut hand_rolled = Vec::new();
            hand_rolled.push(b'"');
            escape_json_str(&mut hand_rolled, raw);
            hand_rolled.push(b'"');
            let serde_out = serde_json::to_string(&raw).expect("serde encodes single ascii char");
            assert_eq!(
                std::str::from_utf8(&hand_rolled).expect("utf-8"),
                serde_out,
                "mismatch on byte 0x{byte:02x}"
            );
        }
    }
}
