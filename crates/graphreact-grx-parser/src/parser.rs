//! Recursive-descent parser for `.grx` files.
//!
//! Direct port of `graphreact/src/core/parser/parser.ts`. Produces an
//! [`ast::GrxFile`] (Phase-7 multi-file format) or a single
//! [`ast::GraphDecl`] for the legacy single-graph entry point.

use serde_json::Value as JsonValue;

use crate::ast::{
    AttrMap, EdgeDecl, GraphDecl, GrxFile, GrxNode, ImportDirective, NodeDecl,
    NodeDefinitionBlock,
};
use crate::error::{GrxParseError, GrxResult, SourceLoc};
use crate::lexer::{Lexer, Token, TokenType};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Convenience: tokenize + parse a single `<Graph>` document.
    pub fn parse_str(input: &str) -> GrxResult<GraphDecl> {
        let tokens = Lexer::new(input).tokenize()?;
        Parser::new(tokens).parse_graph()
    }

    /// Convenience: tokenize + parse a Phase-7 multi-section file.
    pub fn parse_file(input: &str) -> GrxResult<GrxFile> {
        let tokens = Lexer::new(input).tokenize_file()?;
        Parser::new(tokens).parse_grx_file()
    }

    pub fn parse_grx_file(&mut self) -> GrxResult<GrxFile> {
        let mut imports = Vec::new();
        let mut definitions = Vec::new();
        let mut graph: Option<GraphDecl> = None;

        while self.pos < self.tokens.len() {
            self.skip_comments();
            let token = self.peek().clone();
            match token.kind {
                TokenType::Eof => break,
                TokenType::OpenTag if token.value == "import" => {
                    imports.push(self.parse_import()?);
                }
                TokenType::OpenTag if token.value == "Graph" => {
                    if graph.is_some() {
                        return Err(GrxParseError::MultipleGraphs { loc: token.loc });
                    }
                    graph = Some(self.parse_graph()?);
                }
                TokenType::OpenTag => {
                    definitions.push(self.parse_definition_block()?);
                }
                _ => {
                    // Stray close tags or anything else: skip.
                    self.advance();
                }
            }
        }

        Ok(GrxFile {
            imports,
            graph,
            definitions,
        })
    }

    /// Parse a single `<Graph>` block (no imports, no definition
    /// blocks). The legacy entry point — kept for parity with TS
    /// `Parser.parse(input)`.
    pub fn parse_graph(&mut self) -> GrxResult<GraphDecl> {
        self.skip_comments();
        let open = self.expect(TokenType::OpenTag, "root")?;
        if open.value != "Graph" {
            return Err(GrxParseError::BadRoot {
                got: open.value.clone(),
                loc: open.loc,
            });
        }
        let attrs = self.parse_attributes()?;
        let id = match attrs.get("id") {
            Some(JsonValue::String(s)) => s.clone(),
            _ => {
                return Err(GrxParseError::MissingGraphId { loc: open.loc });
            }
        };
        self.expect(TokenType::TagEnd, "Graph")?;
        let children = self.parse_children("Graph")?;
        let close = self.expect(TokenType::CloseTag, "Graph")?;
        if close.value != "Graph" {
            return Err(GrxParseError::MismatchedClose {
                expected: "Graph".to_string(),
                got: close.value.clone(),
                loc: close.loc,
            });
        }
        Ok(GraphDecl {
            id,
            children,
            loc: open.loc,
        })
    }

    fn parse_import(&mut self) -> GrxResult<ImportDirective> {
        let open = self.expect(TokenType::OpenTag, "import")?;
        let attrs = self.parse_attributes()?;
        let src = match attrs.get("src") {
            Some(JsonValue::String(s)) => s.clone(),
            _ => return Err(GrxParseError::MissingImportSrc { loc: open.loc }),
        };
        let next = self.peek();
        match next.kind {
            TokenType::SelfClose => {
                self.advance();
            }
            _ => return Err(GrxParseError::NonSelfClosingImport { loc: next.loc }),
        }
        Ok(ImportDirective { src, loc: open.loc })
    }

    fn parse_definition_block(&mut self) -> GrxResult<NodeDefinitionBlock> {
        let open = self.expect(TokenType::OpenTag, "definition")?;
        let component = open.value.clone();
        // M13-phase3a: definition blocks accept a (small) attribute set
        // — currently just `lang="js"` (default) / `lang="python"`. The
        // lexer's raw-content trigger ("non-reserved tag, no `id`
        // attr") still fires on `lang`-only attribute lists since
        // there's no `id`. Other attributes are tolerated but ignored
        // for now; phase 3+ may use them (e.g., `version="3.12+"` for
        // CPython feature gates).
        let attrs = self.parse_attributes()?;
        let lang = match attrs.get("lang") {
            Some(JsonValue::String(s)) => crate::ast::ComponentLang::from_attr_str(s).ok_or_else(
                || GrxParseError::parse(
                    format!(
                        "unknown `lang` value `{s}` for definition `<{component}>` — \
                         supported: \"js\" (default) / \"python\""
                    ),
                    open.loc,
                ),
            )?,
            Some(_) => {
                return Err(GrxParseError::parse(
                    format!("`lang` attribute on `<{component}>` must be a string literal"),
                    open.loc,
                ));
            }
            None => crate::ast::ComponentLang::default(),
        };
        self.expect(TokenType::TagEnd, &component)?;
        let mut code = String::new();
        if self.peek().kind == TokenType::RawContent {
            code.clone_from(&self.advance().value);
        }
        if self.peek().kind == TokenType::CloseTag && self.peek().value == component {
            self.advance();
        }
        Ok(NodeDefinitionBlock {
            component,
            code,
            lang,
            loc: open.loc,
        })
    }

    fn parse_children(&mut self, parent: &str) -> GrxResult<Vec<GrxNode>> {
        let mut children = Vec::new();
        while self.pos < self.tokens.len() {
            self.skip_comments();
            let token = self.peek().clone();
            match token.kind {
                TokenType::Eof | TokenType::CloseTag => break,
                TokenType::OpenTag if token.value == "edge" => {
                    children.push(GrxNode::Edge(self.parse_edge()?));
                }
                TokenType::OpenTag => {
                    children.push(GrxNode::Node(self.parse_node()?));
                }
                _ => {
                    return Err(GrxParseError::parse(
                        format!(
                            "unexpected token {:?} `{}` inside `<{parent}>`",
                            token.kind, token.value
                        ),
                        token.loc,
                    ));
                }
            }
        }
        Ok(children)
    }

    fn parse_node(&mut self) -> GrxResult<NodeDecl> {
        let open = self.expect(TokenType::OpenTag, "node")?;
        let component = open.value.clone();
        let mut attrs = self.parse_attributes()?;
        let id = match attrs.shift_remove("id") {
            Some(JsonValue::String(s)) => s,
            _ => {
                return Err(GrxParseError::MissingNodeId {
                    component: component.clone(),
                    loc: open.loc,
                });
            }
        };
        let next = self.peek().clone();
        if next.kind == TokenType::SelfClose {
            self.advance();
            return Ok(NodeDecl {
                component,
                id,
                attrs,
                children: Vec::new(),
                self_closing: true,
                loc: open.loc,
            });
        }
        self.expect(TokenType::TagEnd, &component)?;
        let children = self.parse_children(&component)?;
        let close = self.expect(TokenType::CloseTag, &component)?;
        if close.value != component {
            return Err(GrxParseError::MismatchedClose {
                expected: component.clone(),
                got: close.value.clone(),
                loc: close.loc,
            });
        }
        Ok(NodeDecl {
            component,
            id,
            attrs,
            children,
            self_closing: false,
            loc: open.loc,
        })
    }

    fn parse_edge(&mut self) -> GrxResult<EdgeDecl> {
        let open = self.expect(TokenType::OpenTag, "edge")?;
        let mut attrs = self.parse_attributes()?;
        let source = match attrs.shift_remove("source") {
            Some(JsonValue::String(s)) => s,
            _ => return Err(GrxParseError::MissingEdgeEndpoints { loc: open.loc }),
        };
        let target = match attrs.shift_remove("target") {
            Some(JsonValue::String(s)) => s,
            _ => return Err(GrxParseError::MissingEdgeEndpoints { loc: open.loc }),
        };
        let next = self.peek().clone();
        match next.kind {
            TokenType::SelfClose => {
                self.advance();
            }
            TokenType::TagEnd => {
                self.advance();
                let close = self.expect(TokenType::CloseTag, "edge")?;
                if close.value != "edge" {
                    return Err(GrxParseError::MismatchedClose {
                        expected: "edge".to_string(),
                        got: close.value.clone(),
                        loc: close.loc,
                    });
                }
            }
            _ => {
                return Err(GrxParseError::parse(
                    "expected `/>` or `>` after `<edge>` attributes",
                    next.loc,
                ));
            }
        }
        Ok(EdgeDecl {
            source,
            target,
            attrs,
            loc: open.loc,
        })
    }

    fn parse_attributes(&mut self) -> GrxResult<AttrMap> {
        let mut attrs = AttrMap::new();
        while self.pos < self.tokens.len() {
            let token = self.peek().clone();
            if matches!(token.kind, TokenType::TagEnd | TokenType::SelfClose) {
                break;
            }
            if token.kind != TokenType::AttrName {
                break;
            }
            let name = self.advance().value.clone();
            if self.peek().kind == TokenType::AttrEquals {
                self.advance();
                let value_token = self.peek().clone();
                match value_token.kind {
                    TokenType::StringLiteral => {
                        attrs.insert(name, JsonValue::String(self.advance().value.clone()));
                    }
                    TokenType::ExprLiteral => {
                        let expr = self.advance().value.clone();
                        attrs.insert(name, parse_expr_value(&expr));
                    }
                    _ => {
                        return Err(GrxParseError::parse(
                            format!("expected string or expression value for attribute `{name}`"),
                            value_token.loc,
                        ));
                    }
                }
            } else {
                attrs.insert(name, JsonValue::Bool(true));
            }
        }
        Ok(attrs)
    }

    fn skip_comments(&mut self) {
        while self.pos < self.tokens.len() && self.peek().kind == TokenType::Comment {
            self.pos += 1;
        }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> &Token {
        let t = &self.tokens[self.pos];
        self.pos += 1;
        t
    }

    fn expect(&mut self, kind: TokenType, context: &str) -> GrxResult<Token> {
        let token = self.peek().clone();
        if token.kind != kind {
            return Err(GrxParseError::parse(
                format!(
                    "expected {kind:?} but got {:?} `{}` (in {context})",
                    token.kind, token.value
                ),
                token.loc,
            ));
        }
        self.advance();
        Ok(token)
    }
}

/// Parse a `{...}` expression value. Tries JSON first, falls back to
/// the raw string (matches TS `parseExprValue`).
fn parse_expr_value(expr: &str) -> JsonValue {
    serde_json::from_str(expr).unwrap_or_else(|_| JsonValue::String(expr.to_string()))
}

/// Convenience: get a `SourceLoc` for the start of the file.
pub fn start_loc() -> SourceLoc {
    SourceLoc::new(1, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(input: &str) -> GraphDecl {
        Parser::parse_str(input).expect("ok")
    }

    fn file(input: &str) -> GrxFile {
        Parser::parse_file(input).expect("ok")
    }

    #[test]
    fn parses_minimal_graph() {
        let g = graph("<Graph id=\"x\"></Graph>");
        assert_eq!(g.id, "x");
        assert_eq!(g.children.len(), 0);
    }

    #[test]
    fn parses_self_closing_node() {
        let g = graph("<Graph id=\"x\"><Foo id=\"a\" /></Graph>");
        assert_eq!(g.children.len(), 1);
        if let GrxNode::Node(n) = &g.children[0] {
            assert!(n.self_closing);
            assert_eq!(n.component, "Foo");
            assert_eq!(n.id, "a");
        } else {
            panic!("expected node");
        }
    }

    #[test]
    fn parses_nested_nodes() {
        let g = graph(
            "<Graph id=\"x\"><Outer id=\"o\"><Inner id=\"i\" /></Outer></Graph>",
        );
        let outer = match &g.children[0] {
            GrxNode::Node(n) => n,
            GrxNode::Edge(_) => panic!("expected node"),
        };
        assert_eq!(outer.children.len(), 1);
    }

    #[test]
    fn parses_edge_with_ttl() {
        let g = graph(
            "<Graph id=\"x\"><A id=\"a\" /><B id=\"b\" /><edge source=\"a\" target=\"b\" ttl={5} /></Graph>",
        );
        let edge = match g.children.last().unwrap() {
            GrxNode::Edge(e) => e,
            GrxNode::Node(_) => panic!("expected edge"),
        };
        assert_eq!(edge.source, "a");
        assert_eq!(edge.target, "b");
        assert_eq!(edge.attrs.get("ttl"), Some(&JsonValue::Number(5.into())));
    }

    #[test]
    fn parses_node_with_string_and_expr_attrs() {
        let g = graph(
            "<Graph id=\"x\"><Counter id=\"c\" label=\"hi\" initialCount={10} /></Graph>",
        );
        let n = match &g.children[0] {
            GrxNode::Node(n) => n,
            GrxNode::Edge(_) => panic!("expected node"),
        };
        assert_eq!(n.attrs.get("label"), Some(&JsonValue::String("hi".to_string())));
        assert_eq!(
            n.attrs.get("initialCount"),
            Some(&JsonValue::Number(10.into()))
        );
    }

    #[test]
    fn parses_boolean_attribute() {
        let g = graph("<Graph id=\"x\"><Foo id=\"f\" enabled /></Graph>");
        let n = match &g.children[0] {
            GrxNode::Node(n) => n,
            GrxNode::Edge(_) => panic!("expected node"),
        };
        assert_eq!(n.attrs.get("enabled"), Some(&JsonValue::Bool(true)));
    }

    #[test]
    fn parser_rejects_root_other_than_graph() {
        let r = Parser::parse_str("<Foo id=\"x\" />");
        assert!(matches!(r, Err(GrxParseError::BadRoot { .. })));
    }

    #[test]
    fn parser_rejects_missing_graph_id() {
        let r = Parser::parse_str("<Graph></Graph>");
        assert!(matches!(r, Err(GrxParseError::MissingGraphId { .. })));
    }

    #[test]
    fn parser_rejects_missing_node_id() {
        let r = Parser::parse_str("<Graph id=\"x\"><Foo /></Graph>");
        assert!(matches!(r, Err(GrxParseError::MissingNodeId { .. })));
    }

    #[test]
    fn parser_rejects_missing_edge_endpoints() {
        let r = Parser::parse_str("<Graph id=\"x\"><edge source=\"a\" /></Graph>");
        assert!(matches!(r, Err(GrxParseError::MissingEdgeEndpoints { .. })));
    }

    #[test]
    fn parser_rejects_mismatched_close_tag() {
        let r = Parser::parse_str("<Graph id=\"x\"><A id=\"a\"></B></Graph>");
        assert!(r.is_err());
    }

    #[test]
    fn parses_multifile_format() {
        let src = r#"<import src="./sensors.grx" />
<import src="./outputs.grx" />
<Graph id="g">
  <Logger id="log" />
</Graph>"#;
        let f = file(src);
        assert_eq!(f.imports.len(), 2);
        assert_eq!(f.imports[0].src, "./sensors.grx");
        assert_eq!(f.imports[1].src, "./outputs.grx");
        assert!(f.graph.is_some());
    }

    #[test]
    fn parses_definition_block() {
        let src = r"<Foo>
const x = 1;
return { accepts: ['poll'] };
</Foo>";
        let f = file(src);
        assert_eq!(f.definitions.len(), 1);
        assert_eq!(f.definitions[0].component, "Foo");
        assert!(f.definitions[0].code.contains("const x = 1"));
        // M13-phase3a: lang defaults to Js for back-compat with
        // pre-existing fixtures that don't carry the attribute.
        assert_eq!(f.definitions[0].lang, crate::ast::ComponentLang::Js);
    }

    #[test]
    fn parses_definition_block_with_lang_python() {
        // M13-phase3a: definitions accept `lang="python"` and the IR
        // tags them so the runtime routes them through the CPython
        // engine. The lexer's raw-content trigger ("non-reserved tag,
        // no `id` attr") still fires because there's no `id`.
        let src = "<Sensor lang=\"python\">\ndef init():\n    return {'accepts': ['poll']}\n</Sensor>";
        let f = file(src);
        assert_eq!(f.definitions.len(), 1);
        assert_eq!(f.definitions[0].component, "Sensor");
        assert_eq!(f.definitions[0].lang, crate::ast::ComponentLang::Python);
        assert!(f.definitions[0].code.contains("def init"));
    }

    #[test]
    fn parses_definition_block_with_lang_js_explicit() {
        // Explicit lang="js" works the same as the default.
        let src = "<Logger lang=\"js\">\nreturn { accepts: ['x'] };\n</Logger>";
        let f = file(src);
        assert_eq!(f.definitions[0].lang, crate::ast::ComponentLang::Js);
    }

    #[test]
    fn parser_rejects_unknown_lang_value() {
        // Surface unknown languages at parse time with a clear
        // message — the parser is engine-agnostic but rejects values
        // that no future engine can plausibly handle.
        let src = "<Sensor lang=\"ruby\">\nputs 'hi'\n</Sensor>";
        let r = Parser::parse_file(src);
        match r {
            Err(GrxParseError::Parse { message, .. }) => {
                assert!(
                    message.contains("unknown `lang`"),
                    "expected unknown-lang message, got: {message}"
                );
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn parses_combined_imports_definitions_and_graph() {
        let src = r#"<import src="./a.grx" />
<Logger>
  return { accepts: ['x'] };
</Logger>
<Graph id="g">
  <Logger id="log" />
</Graph>"#;
        let f = file(src);
        assert_eq!(f.imports.len(), 1);
        assert_eq!(f.definitions.len(), 1);
        assert!(f.graph.is_some());
    }

    #[test]
    fn parser_rejects_multiple_graphs() {
        let src = "<Graph id=\"a\"></Graph><Graph id=\"b\"></Graph>";
        let r = Parser::parse_file(src);
        assert!(matches!(r, Err(GrxParseError::MultipleGraphs { .. })));
    }

    #[test]
    fn parser_rejects_non_self_closing_import() {
        let src = "<import src=\"./x\"></import>";
        let r = Parser::parse_file(src);
        assert!(matches!(r, Err(GrxParseError::NonSelfClosingImport { .. })));
    }

    #[test]
    fn parser_handles_comments_between_top_level_blocks() {
        let src = r#"<!-- header -->
<import src="./a.grx" />
<!-- between -->
<Graph id="g"></Graph>"#;
        let f = file(src);
        assert_eq!(f.imports.len(), 1);
        assert!(f.graph.is_some());
    }

    #[test]
    fn ast_walk_yields_depth_and_path() {
        let g = graph(
            "<Graph id=\"g\"><A id=\"a\"><B id=\"b\"><C id=\"c\" /></B></A></Graph>",
        );
        let walked: Vec<_> = g.walk_nodes().collect();
        assert_eq!(walked.len(), 3);
        assert_eq!(walked[0].depth, 0);
        assert_eq!(walked[0].node.id, "a");
        assert_eq!(walked[1].depth, 1);
        assert_eq!(walked[1].node.id, "b");
        assert_eq!(walked[2].depth, 2);
        assert_eq!(walked[2].node.id, "c");
        assert_eq!(walked[2].parent_path, vec!["a", "b", "c"]);
    }
}
