//! Tokenizer for `.grx` files. Direct port of
//! `graphreact/src/core/parser/lexer.ts` semantics.
//!
//! Two top-level modes:
//!
//! * [`Lexer::tokenize`] — graph-only (single `<Graph>` block, no
//!   imports / definition blocks). Used by the legacy
//!   `Parser::parse` entry point; we keep it for parity-test
//!   coverage.
//! * [`Lexer::tokenize_file`] — full Phase-7 format: top-level
//!   `<import>` + `<Graph>` + non-reserved `<ComponentName>...code...</ComponentName>`
//!   raw-content blocks.
//!
//! State-aware lexing: when a non-reserved tag is encountered at the
//! top level (in file mode) and ends with `>` (not `/>`) without an
//! `id` attribute, the lexer switches into raw-content mode and
//! consumes characters verbatim until the matching `</ComponentName>`.

use rustc_hash::FxHashSet;

use crate::error::{GrxParseError, GrxResult, SourceLoc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    OpenTag,
    CloseTag,
    SelfClose,
    TagEnd,
    AttrName,
    AttrEquals,
    StringLiteral,
    ExprLiteral,
    Comment,
    RawContent,
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenType,
    pub value: String,
    pub loc: SourceLoc,
}

/// Tags that are part of the graph markup (not definition blocks).
fn reserved_tags() -> FxHashSet<&'static str> {
    let mut s = FxHashSet::default();
    s.insert("Graph");
    s.insert("edge");
    s.insert("import");
    // PCBPnR Phase 5-D §2.2/§2.3 — these top-level/structural tags
    // are NOT definition blocks even though they have no `id` attr.
    // Mark them reserved so the lexer doesn't treat their body as
    // raw component-body source.
    s.insert("Subgraph");
    s.insert("Subsystem");
    s
}

pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    line: usize,
    column: usize,
    tokens: Vec<Token>,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
            line: 1,
            column: 1,
            tokens: Vec::new(),
        }
    }

    /// Graph-only tokenization (no imports / definition blocks).
    pub fn tokenize(mut self) -> GrxResult<Vec<Token>> {
        while self.pos < self.input.len() {
            self.skip_whitespace();
            if self.pos >= self.input.len() {
                break;
            }
            if self.match_str("<!--") {
                self.read_comment()?;
            } else if self.match_str("</") {
                self.read_close_tag()?;
            } else if self.match_str("<") {
                self.read_open_tag()?;
            } else {
                self.skip_until_byte(b'<');
            }
        }
        self.tokens.push(Token {
            kind: TokenType::Eof,
            value: String::new(),
            loc: self.loc(),
        });
        Ok(self.tokens)
    }

    /// Phase-7 full-file tokenization.
    pub fn tokenize_file(mut self) -> GrxResult<Vec<Token>> {
        let reserved = reserved_tags();
        while self.pos < self.input.len() {
            self.skip_whitespace();
            if self.pos >= self.input.len() {
                break;
            }
            if self.match_str("<!--") {
                self.read_comment()?;
            } else if self.match_str("</") {
                self.read_close_tag()?;
            } else if self.match_str("<") {
                let saved_token_len = self.tokens.len();
                self.read_open_tag()?;
                if let Some(open_tag) = self.tokens.get(saved_token_len) {
                    if open_tag.kind == TokenType::OpenTag {
                        let tag_name = open_tag.value.clone();
                        if let Some(last_token) = self.tokens.last() {
                            if !reserved.contains(tag_name.as_str())
                                && last_token.kind == TokenType::TagEnd
                            {
                                let has_id_attr = self.tokens[saved_token_len..].iter().any(|t| {
                                    t.kind == TokenType::AttrName && t.value == "id"
                                });
                                if !has_id_attr {
                                    self.read_raw_content(&tag_name)?;
                                }
                            }
                        }
                    }
                }
            } else {
                self.skip_until_byte(b'<');
            }
        }
        self.tokens.push(Token {
            kind: TokenType::Eof,
            value: String::new(),
            loc: self.loc(),
        });
        Ok(self.tokens)
    }

    fn loc(&self) -> SourceLoc {
        SourceLoc::new(self.line, self.column)
    }

    fn match_str(&self, s: &str) -> bool {
        self.input[self.pos..]
            .iter()
            .zip(s.as_bytes())
            .all(|(a, b)| a == b)
            && self.input.len() - self.pos >= s.len()
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let ch = self.peek()?;
        self.pos += 1;
        if ch == b'\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(ch)
    }

    fn skip_whitespace(&mut self) {
        while let Some(ch) = self.peek() {
            if ch.is_ascii_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn skip_until_byte(&mut self, target: u8) {
        while let Some(ch) = self.peek() {
            if ch == target {
                break;
            }
            self.advance();
        }
    }

    fn read_comment(&mut self) -> GrxResult<()> {
        let start_loc = self.loc();
        for _ in 0..4 {
            self.advance();
        }
        let mut value = String::new();
        while self.pos < self.input.len() {
            if self.match_str("-->") {
                for _ in 0..3 {
                    self.advance();
                }
                self.tokens.push(Token {
                    kind: TokenType::Comment,
                    value: value.trim().to_string(),
                    loc: start_loc,
                });
                return Ok(());
            }
            if let Some(ch) = self.advance() {
                value.push(ch as char);
            }
        }
        Err(GrxParseError::lex("unterminated comment", start_loc))
    }

    fn read_close_tag(&mut self) -> GrxResult<()> {
        let start_loc = self.loc();
        self.advance();
        self.advance();
        self.skip_whitespace();
        let name = self.read_identifier()?;
        self.skip_whitespace();
        if self.peek() != Some(b'>') {
            return Err(GrxParseError::lex(
                format!("expected `>` after closing tag name `{name}`"),
                self.loc(),
            ));
        }
        self.advance();
        self.tokens.push(Token {
            kind: TokenType::CloseTag,
            value: name,
            loc: start_loc,
        });
        Ok(())
    }

    fn read_open_tag(&mut self) -> GrxResult<()> {
        let start_loc = self.loc();
        self.advance();
        self.skip_whitespace();
        let name = self.read_identifier()?;
        self.tokens.push(Token {
            kind: TokenType::OpenTag,
            value: name,
            loc: start_loc,
        });
        loop {
            self.skip_whitespace();
            if self.match_str("/>") {
                self.advance();
                self.advance();
                self.tokens.push(Token {
                    kind: TokenType::SelfClose,
                    value: "/>".to_string(),
                    loc: self.loc(),
                });
                return Ok(());
            }
            if self.peek() == Some(b'>') {
                self.advance();
                self.tokens.push(Token {
                    kind: TokenType::TagEnd,
                    value: ">".to_string(),
                    loc: self.loc(),
                });
                return Ok(());
            }
            if self.pos >= self.input.len() {
                return Err(GrxParseError::lex("unterminated tag", start_loc));
            }
            self.read_attribute()?;
        }
    }

    fn read_attribute(&mut self) -> GrxResult<()> {
        let name_loc = self.loc();
        let name = self.read_identifier()?;
        self.tokens.push(Token {
            kind: TokenType::AttrName,
            value: name.clone(),
            loc: name_loc,
        });
        self.skip_whitespace();
        if self.peek() != Some(b'=') {
            return Ok(()); // boolean attr
        }
        self.advance();
        self.tokens.push(Token {
            kind: TokenType::AttrEquals,
            value: "=".to_string(),
            loc: self.loc(),
        });
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => self.read_string_literal()?,
            Some(b'{') => self.read_expr_literal()?,
            _ => {
                return Err(GrxParseError::lex(
                    format!("expected `\"` or `{{` after `=` in attribute `{name}`"),
                    self.loc(),
                ))
            }
        }
        Ok(())
    }

    fn read_identifier(&mut self) -> GrxResult<String> {
        let mut name = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'-' {
                self.advance();
                name.push(ch as char);
            } else {
                break;
            }
        }
        if name.is_empty() {
            return Err(GrxParseError::lex("expected identifier", self.loc()));
        }
        Ok(name)
    }

    fn read_string_literal(&mut self) -> GrxResult<()> {
        let start_loc = self.loc();
        self.advance(); // opening "
        let mut value = String::new();
        while let Some(ch) = self.peek() {
            if ch == b'"' {
                self.advance();
                self.tokens.push(Token {
                    kind: TokenType::StringLiteral,
                    value,
                    loc: start_loc,
                });
                return Ok(());
            }
            if ch == b'\\' {
                self.advance();
                if let Some(esc) = self.advance() {
                    value.push(esc as char);
                }
                continue;
            }
            self.advance();
            value.push(ch as char);
        }
        Err(GrxParseError::lex("unterminated string literal", start_loc))
    }

    fn read_expr_literal(&mut self) -> GrxResult<()> {
        let start_loc = self.loc();
        self.advance(); // opening {
        let mut value = String::new();
        let mut depth = 1_i32;
        while let Some(ch) = self.peek() {
            if ch == b'{' {
                depth += 1;
            }
            if ch == b'}' {
                depth -= 1;
            }
            if depth > 0 {
                self.advance();
                value.push(ch as char);
            } else {
                self.advance();
                self.tokens.push(Token {
                    kind: TokenType::ExprLiteral,
                    value: value.trim().to_string(),
                    loc: start_loc,
                });
                return Ok(());
            }
        }
        Err(GrxParseError::lex(
            "unterminated expression literal",
            start_loc,
        ))
    }

    fn read_raw_content(&mut self, tag_name: &str) -> GrxResult<()> {
        let start_loc = self.loc();
        let close_tag = format!("</{tag_name}>");
        let mut content = String::new();
        while self.pos < self.input.len() {
            if self.match_str(&close_tag) {
                for _ in 0..close_tag.len() {
                    self.advance();
                }
                self.tokens.push(Token {
                    kind: TokenType::RawContent,
                    value: content.trim().to_string(),
                    loc: start_loc,
                });
                self.tokens.push(Token {
                    kind: TokenType::CloseTag,
                    value: tag_name.to_string(),
                    loc: self.loc(),
                });
                return Ok(());
            }
            if let Some(ch) = self.advance() {
                content.push(ch as char);
            }
        }
        Err(GrxParseError::lex(
            format!("unterminated definition block <{tag_name}>"),
            start_loc,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(input: &str) -> Vec<Token> {
        Lexer::new(input).tokenize().expect("ok")
    }

    fn lex_file(input: &str) -> Vec<Token> {
        Lexer::new(input).tokenize_file().expect("ok")
    }

    #[test]
    fn lexer_handles_open_close_tags() {
        let tokens = lex("<Graph id=\"x\"></Graph>");
        let kinds: Vec<TokenType> = tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenType::OpenTag,
                TokenType::AttrName,
                TokenType::AttrEquals,
                TokenType::StringLiteral,
                TokenType::TagEnd,
                TokenType::CloseTag,
                TokenType::Eof,
            ]
        );
    }

    #[test]
    fn lexer_handles_self_closing() {
        let tokens = lex("<edge source=\"a\" target=\"b\" />");
        let kinds: Vec<TokenType> = tokens.iter().map(|t| t.kind).collect();
        assert_eq!(*kinds.last().unwrap(), TokenType::Eof);
        assert!(kinds.contains(&TokenType::SelfClose));
    }

    #[test]
    fn lexer_handles_expr_attribute() {
        let tokens = lex("<X v={42} />");
        let expr = tokens.iter().find(|t| t.kind == TokenType::ExprLiteral).unwrap();
        assert_eq!(expr.value, "42");
    }

    #[test]
    fn lexer_handles_balanced_expr_braces() {
        let tokens = lex("<X v={ {a:1, b:[2,3]} } />");
        let expr = tokens.iter().find(|t| t.kind == TokenType::ExprLiteral).unwrap();
        assert_eq!(expr.value, "{a:1, b:[2,3]}");
    }

    #[test]
    fn lexer_skips_comments_outside_tags() {
        let tokens = lex("<!-- top --> <Graph id=\"x\"></Graph>");
        assert_eq!(tokens[0].kind, TokenType::Comment);
        assert_eq!(tokens[0].value, "top");
    }

    #[test]
    fn lexer_string_with_escape() {
        let tokens = lex("<X label=\"a\\\"b\" />");
        let s = tokens.iter().find(|t| t.kind == TokenType::StringLiteral).unwrap();
        assert_eq!(s.value, "a\"b");
    }

    #[test]
    fn lexer_unterminated_tag_errors() {
        let result = Lexer::new("<X").tokenize();
        assert!(result.is_err());
    }

    #[test]
    fn lexer_file_mode_captures_definition_block() {
        let src = r"<Foo>
const x = 1;
return { accepts: ['a'] };
</Foo>";
        let tokens = lex_file(src);
        let raw = tokens
            .iter()
            .find(|t| t.kind == TokenType::RawContent)
            .expect("raw content");
        assert!(raw.value.contains("const x = 1"));
        assert!(raw.value.contains("accepts"));
    }

    #[test]
    fn lexer_file_mode_does_not_consume_imports_as_raw() {
        let src = r#"<import src="./foo.grx" />
<Graph id="g"></Graph>"#;
        let tokens = lex_file(src);
        let raws: Vec<_> = tokens.iter().filter(|t| t.kind == TokenType::RawContent).collect();
        assert!(raws.is_empty());
    }

    #[test]
    fn lexer_tracks_line_column() {
        let tokens = lex("\n  <Graph id=\"x\"></Graph>");
        let open = tokens.iter().find(|t| t.kind == TokenType::OpenTag).unwrap();
        assert_eq!(open.loc.line, 2);
        assert_eq!(open.loc.column, 3);
    }
}
