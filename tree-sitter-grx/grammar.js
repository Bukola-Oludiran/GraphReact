/**
 * GraphReact .grx grammar (M15 Phase 1B).
 *
 * .grx is HTML-shaped at the outer layer:
 *
 *   - Imports: `<import src="..." />`
 *   - Graph: `<Graph id="..."> ...graph nodes... </Graph>`
 *   - Definition blocks: `<ComponentName lang="..."> raw_body </ComponentName>`
 *
 * Inside `<Graph>`:
 *   - Nodes: `<TagName id="..." attr={expr} attr="str">children</TagName>` or
 *     self-closing `<TagName id="..." />`.
 *   - Edges: `<edge source="..." target="..." attr={expr} />` (self-closing).
 *
 * Definition blocks live AT THE TOP LEVEL (outside `<Graph>`) and have
 * no `id` attribute (their tag name identifies them; the body is opaque
 * source code that runs through the engine). Their body is captured
 * verbatim — JS / Python code that the engine layer compiles.
 *
 * State-aware lexing is required for `raw_body`: the scanner consumes
 * everything up to the next `</identifier>` close tag (mirroring the
 * existing Rust lexer's `read_raw_content`, with one caveat documented
 * in `src/scanner.c`: definition bodies cannot contain `</identifier>`
 * patterns themselves — same load-bearing simplification the
 * hand-rolled lexer already makes).
 */

module.exports = grammar({
  name: 'grx',

  // External tokens supplied by `src/scanner.c`. The `raw_body` token
  // captures the verbatim body of a definition block; the scanner
  // consumes until it hits a `</identifier>` close tag, leaving the
  // close tag intact for the regular grammar to match.
  externals: ($) => [
    $.raw_body,
    $._error_sentinel,
  ],

  extras: ($) => [/\s/, $.comment],

  word: ($) => $.identifier,

  conflicts: ($) => [
    // The same `<TagName attrs>...</TagName>` shape can appear either
    // as a graph node (when nested inside `<Graph>`) or as a
    // definition block (when at top level). Tree-sitter's GLR
    // resolves by context, with the external `raw_body` token
    // applying only inside the definition arm.
    [$.graph_node_open, $._definition_open],
  ],

  rules: {
    // ----------------------------------------------------------------
    // Top-level
    // ----------------------------------------------------------------

    source_file: ($) => repeat($._top_level_item),

    _top_level_item: ($) =>
      choice($.import_decl, $.graph, $.subgraph, $.definition_block),

    // ----------------------------------------------------------------
    // Imports
    // ----------------------------------------------------------------

    import_decl: ($) =>
      seq('<', 'import', repeat($.attribute), '/>'),

    // ----------------------------------------------------------------
    // Graph
    // ----------------------------------------------------------------

    graph: ($) => seq($._graph_open, repeat($._graph_child), $._graph_close),

    _graph_open: ($) =>
      seq('<', 'Graph', repeat($.attribute), '>'),

    _graph_close: ($) => seq('<', '/', 'Graph', '>'),

    _graph_child: ($) => choice($.graph_node, $.edge_decl),

    // ----------------------------------------------------------------
    // Subgraph — PCBPnR Phase 5-D §2.2. Same child shape as Graph
    // (nodes + edge_decls). Distinguished from Graph by tag literal.
    // ----------------------------------------------------------------

    subgraph: ($) => seq($._subgraph_open, repeat($._graph_child), $._subgraph_close),

    _subgraph_open: ($) =>
      seq('<', 'Subgraph', repeat($.attribute), '>'),

    _subgraph_close: ($) => seq('<', '/', 'Subgraph', '>'),

    // Graph nodes have an `id` attribute and contain other graph
    // children. The grammar doesn't enforce the id-attribute presence
    // (the IR builder validates that); it just shapes any
    // attribute-bearing tag.
    graph_node: ($) =>
      choice(
        seq($.graph_node_open, repeat($._graph_child), $._graph_node_close),
        $.graph_node_self_close,
      ),

    graph_node_open: ($) =>
      seq('<', $.tag_name, repeat($.attribute), '>'),

    graph_node_self_close: ($) =>
      seq('<', $.tag_name, repeat($.attribute), '/>'),

    _graph_node_close: ($) => seq('<', '/', $.tag_name, '>'),

    edge_decl: ($) => seq('<', 'edge', repeat($.attribute), '/>'),

    // ----------------------------------------------------------------
    // Definition blocks (top-level, raw body)
    // ----------------------------------------------------------------

    // Definition blocks: either inline-body (open + raw_body + close)
    // OR self-closing with `src="./body.py"` for an external body
    // file (PCBPnR Phase 5-D follow-on).
    definition_block: ($) =>
      choice(
        seq($._definition_open, $.raw_body, $._definition_close),
        $._definition_self_close,
      ),

    _definition_open: ($) =>
      seq('<', $.tag_name, repeat($.attribute), '>'),

    _definition_self_close: ($) =>
      seq('<', $.tag_name, repeat($.attribute), '/>'),

    _definition_close: ($) => seq('<', '/', $.tag_name, '>'),

    // ----------------------------------------------------------------
    // Attributes
    // ----------------------------------------------------------------

    attribute: ($) =>
      seq($.attr_name, optional(seq('=', $._attr_value))),

    attr_name: ($) => $.identifier,

    _attr_value: ($) => choice($.string_literal, $.expr_literal),

    string_literal: ($) =>
      seq('"', repeat(choice(/[^"\\]/, $.escape_sequence)), '"'),

    escape_sequence: ($) => /\\./,

    // Expression literals balance `{` and `}`. Tree-sitter's regex
    // doesn't natively count nested braces, but a greedy
    // non-greedy-friendly approach with a custom rule works because
    // grx attribute expressions are bounded (one balanced pair).
    // We leave them opaque to the grammar — engine-level parsing
    // (swc / rustpython-parser) handles deeper introspection.
    expr_literal: ($) =>
      seq(
        '{',
        repeat(choice(/[^{}]/, seq('{', repeat(/[^{}]/), '}'))),
        '}',
      ),

    // ----------------------------------------------------------------
    // Lexical
    // ----------------------------------------------------------------

    tag_name: ($) => $.identifier,

    identifier: ($) => /[A-Za-z_][A-Za-z0-9_-]*/,

    comment: ($) => seq('<!--', /([^-]|-[^-]|--[^>])*/, '-->'),
  },
});
