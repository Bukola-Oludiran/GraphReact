// M15 Phase 1B — external scanner for the .grx grammar.
//
// Produces ONE custom token: `raw_body`, which captures the verbatim
// body of a definition block (`<ComponentName lang="..."> raw_body
// </ComponentName>`).
//
// ## State-aware lexing rationale
//
// The hand-rolled Rust lexer at
// `crates/graphreact-grx-parser/src/lexer.rs` enters a raw-content
// mode when it sees a non-reserved (not Graph / edge / import) tag at
// the top level that ends with `>` (not `/>`) and has no `id`
// attribute. In raw mode it consumes characters verbatim until the
// LITERAL `</TagName>` (knows the tag name). Tree-sitter's GLR-style
// parser doesn't have direct access to the open-tag name from inside
// an external scanner, so this scanner uses a load-bearing
// simplification:
//
//   The scanner consumes characters until the FIRST `</identifier>`
//   close-tag pattern, regardless of which identifier it names.
//
// In practice this is equivalent to the hand-rolled lexer's behaviour
// for every existing fixture (definition bodies are JS or Python
// code; none contain `</identifier>` patterns embedded in code).
// Documented as a known limitation in the parser docs: definition
// bodies cannot contain literal `</identifier>` substrings.
//
// ## Incremental reparse
//
// Tree-sitter scanners must support serialise / deserialise so the
// incremental reparser can suspend + resume scanning across edit
// boundaries. This scanner is stateless (no per-block memory across
// `scan` invocations), so the serialise / deserialise hooks are
// trivial.

#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <stddef.h>

// External token IDs — ORDER MUST MATCH the `externals: ($) => [...]`
// list in `grammar.js`.
enum TokenType {
  RAW_BODY = 0,
  ERROR_SENTINEL = 1,
};

// Stateless scanner — `create` allocates nothing, `destroy` is a
// no-op, `serialize` / `deserialize` round-trip zero bytes.
void *tree_sitter_grx_external_scanner_create(void) {
  return NULL;
}

void tree_sitter_grx_external_scanner_destroy(void *payload) {
  (void)payload;
}

unsigned tree_sitter_grx_external_scanner_serialize(void *payload,
                                                    char *buffer) {
  (void)payload;
  (void)buffer;
  return 0;
}

void tree_sitter_grx_external_scanner_deserialize(void *payload,
                                                  const char *buffer,
                                                  unsigned length) {
  (void)payload;
  (void)buffer;
  (void)length;
}

static inline bool is_identifier_start(int32_t c) {
  return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || c == '_';
}

bool tree_sitter_grx_external_scanner_scan(void *payload,
                                           TSLexer *lexer,
                                           const bool *valid_symbols) {
  (void)payload;

  if (!valid_symbols[RAW_BODY]) {
    return false;
  }

  // We have to emit at least one character of body content; a
  // zero-width raw_body is meaningless. Empty definition bodies
  // (`<Component></Component>`) end up with whitespace, so we
  // accept at least one non-`</identifier>` lookahead.
  bool has_content = false;

  while (true) {
    int32_t c = lexer->lookahead;

    // EOF mid-definition — bail; the regular grammar will surface
    // an error pointing at the unterminated block.
    if (c == 0) {
      if (has_content) {
        lexer->result_symbol = RAW_BODY;
        return true;
      }
      return false;
    }

    // Peek for `</identifier>` — the close-tag pattern that ends the
    // raw body. We don't consume the `<` if it's the start of a
    // matching close tag; the regular grammar matches `<`, `/`,
    // tag_name, `>` after the raw_body.
    if (c == '<') {
      // We need to lookahead past `<` and check if `/` and an
      // identifier follow. Tree-sitter scanners can't truly
      // "peek without advancing" — the closest is `mark_end` to
      // commit the current span before peeking further, then
      // re-checking.
      lexer->mark_end(lexer);
      lexer->advance(lexer, false);
      if (lexer->lookahead == '/') {
        lexer->advance(lexer, false);
        if (is_identifier_start(lexer->lookahead)) {
          // This is a `</identifier...` close tag. Stop the
          // raw_body here without consuming the `<`. `mark_end`
          // above already pinned the end of the body; the
          // returned symbol's content covers everything before
          // the `<`.
          if (has_content) {
            lexer->result_symbol = RAW_BODY;
            return true;
          }
          // Empty body case (e.g. `<Component></Component>`):
          // emit a zero-width raw_body anyway so the grammar's
          // `definition_block = open raw_body close` rule
          // matches without falling back to error recovery.
          lexer->result_symbol = RAW_BODY;
          return true;
        }
      }
      // Not a close tag (e.g. `<div>` inside a JS string). Continue
      // consuming; the `<` and any subsequent chars go into the body.
      // We've already advanced past `<` and possibly `/` — those
      // chars are NOW part of the consumed span but `mark_end` was
      // called above, which pinned the body END at the position of
      // `<`. We need to re-pin to the current cursor.
      has_content = true;
      // Reset mark to current (post-advance) cursor.
      // Tree-sitter's contract: mark_end advances the boundary; we
      // can re-call to extend it.
      lexer->mark_end(lexer);
      continue;
    }

    // Anything else: consume.
    lexer->advance(lexer, false);
    has_content = true;
    lexer->mark_end(lexer);
  }
}
