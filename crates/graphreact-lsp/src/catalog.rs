//! Tag / attribute / property catalogs used by hover + completion.
//!
//! The CSS-property catalog is sourced from
//! [`graphreact_grx_parser::style::recognized_style_properties`] so
//! that this layer never drifts from the desugarer. Tag + attribute
//! tables ARE duplicated here because the parser is open-vocabulary
//! (it accepts any tag name except a small set of structural ones);
//! we publish the well-known tags and the canonical attributes for
//! the small set the runtime + PCBPnR layers care about. Unknown tags
//! still complete from the live IR's open vocabulary.

use graphreact_grx_parser::style::{clip_path_shape_names, recognized_style_properties};

/// One catalog entry: the surface symbol plus a Markdown blob the
/// hover / completion renderers can hand straight to the editor.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub name: &'static str,
    pub kind: EntryKind,
    pub markdown: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    StructuralTag,
    PcbTag,
    BodyLangTag,
    Attribute,
    StyleProperty,
    ClipPathShape,
}

/// Structural tags the parser reserves: `<Graph>`, `<Subgraph>`,
/// `<Subsystem>`, `<import>`, `<edge>`. Drives goto-definition target
/// recognition (e.g. `<Subsystem>` triggers ref-resolution) and the
/// hover doc strings.
pub const STRUCTURAL_TAGS: &[CatalogEntry] = &[
    CatalogEntry {
        name: "Graph",
        kind: EntryKind::StructuralTag,
        markdown: "**`<Graph>`** — the runtime topology root. Exactly one per `.grx` file. Requires `id=\"…\"`.",
    },
    CatalogEntry {
        name: "Subgraph",
        kind: EntryKind::StructuralTag,
        markdown: "**`<Subgraph>`** — a named, reusable graph fragment. Addressable via `<Subsystem ref=\"<alias>:<name>\" id=\"…\"/>` from importers. Requires `name=\"…\"`.",
    },
    CatalogEntry {
        name: "Subsystem",
        kind: EntryKind::StructuralTag,
        markdown: "**`<Subsystem>`** — a splice placeholder. The IR builder replaces it with the referenced `<Subgraph>`'s children prefixed by the local `id`. Requires `ref=\"<alias>:<name>\"` (or bare `<name>` for in-file refs) and `id=\"…\"`.",
    },
    CatalogEntry {
        name: "import",
        kind: EntryKind::StructuralTag,
        markdown: "**`<import>`** — pull subgraphs and definitions from another `.grx` file. Requires `src=\"…\"`; optional `as=\"<alias>\"` qualifies imported subgraph names.",
    },
    CatalogEntry {
        name: "edge",
        kind: EntryKind::StructuralTag,
        markdown: "**`<edge>`** — explicit (non-structural) edge between two nodes. Requires `source=\"…\"` and `target=\"…\"`.",
    },
];

/// PCBPnR-flavoured tags hardcoded in the style desugar/resugar tag
/// dispatch. Not enforced by the parser (it accepts any component
/// name); listed here for hover + completion docs.
pub const PCB_TAGS: &[CatalogEntry] = &[
    CatalogEntry {
        name: "Component",
        kind: EntryKind::PcbTag,
        markdown: "**`<Component>`** — a placed footprint on the board. Carries `footprint`, `at`, `layer`, `body_bbox`, and optional component body code (`lang=\"…\"`).",
    },
    CatalogEntry {
        name: "Net",
        kind: EntryKind::PcbTag,
        markdown: "**`<Net>`** — a named electrical net. Carries `impedance`, `tolerance`, `preferred-layer`, `pin-layer`.",
    },
    CatalogEntry {
        name: "Pin",
        kind: EntryKind::PcbTag,
        markdown: "**`<Pin>`** — a pin on a component, bound to a net.",
    },
    CatalogEntry {
        name: "Pad",
        kind: EntryKind::PcbTag,
        markdown: "**`<Pad>`** — a copper pad on a footprint. Width/height desugar to `size` (not `bbox`).",
    },
];

/// Body-language hint for `lang=` attribute value completion on
/// `<Component>`-style definition blocks.
pub const BODY_LANGS: &[CatalogEntry] = &[
    CatalogEntry {
        name: "js",
        kind: EntryKind::BodyLangTag,
        markdown: "**`lang=\"js\"`** — JavaScript/TypeScript body, routed to the JSC engine.",
    },
    CatalogEntry {
        name: "python",
        kind: EntryKind::BodyLangTag,
        markdown: "**`lang=\"python\"`** — Python body, routed to the CPython engine.",
    },
    CatalogEntry {
        name: "go",
        kind: EntryKind::BodyLangTag,
        markdown: "**`lang=\"go\"`** — Go body, pre-compiled via `graphreact-codegen-go` and loaded through `libloading`.",
    },
];

/// Per-tag attribute catalog. Keyed on tag name; the LSP completion
/// path looks up the live tag (or falls back to a generic list).
#[must_use]
pub fn attributes_for_tag(tag: &str) -> &'static [&'static str] {
    match tag {
        "Graph" => &["id"],
        "Subgraph" => &["name"],
        "Subsystem" => &["ref", "id", "style"],
        "import" => &["src", "as"],
        "edge" => &["source", "target", "ttl", "kind"],
        "Component" => &[
            "id",
            "footprint",
            "lang",
            "src",
            "layer",
            "style",
        ],
        "Net" => &["id", "impedance", "tolerance", "style"],
        "Pin" => &["id", "net", "pad", "style"],
        "Pad" => &["id", "net", "clearance", "style"],
        // Unknown tag: offer the universal set.
        _ => &["id", "style"],
    }
}

/// Style-property catalog, sourced from the parser-side
/// `recognized_style_properties` so it never drifts.
#[must_use]
pub fn style_properties() -> &'static [&'static str] {
    recognized_style_properties()
}

/// Clip-path shape catalog, sourced from the parser-side
/// `clip_path_shape_names`.
#[must_use]
pub fn clip_path_shapes() -> &'static [&'static str] {
    clip_path_shape_names()
}

/// Hover Markdown for a single style property. Falls back to a
/// generic blurb for properties without a curated description.
#[must_use]
pub fn style_property_markdown(prop: &str) -> String {
    let blurb = match prop {
        "position" => "CSS-shape `position: absolute` — required when `left`/`top`/`width`/`height` are present.",
        "left" => "X-axis placement (`<length>` with unit `mm`, `cm`, `in`, `px`). Desugars to `at.x`.",
        "top" => "Y-axis placement. Desugars to `at.y`.",
        "rotate" => "Rotation in degrees (`<number>deg`). Desugars to `at.rot`.",
        "transform" => "Currently only `rotate(<deg>)` accepted. Desugars to `at.rot`.",
        "width" => "Element width (`<length>`). Desugars to `size.w` on `<Pad>`, `bbox.w` elsewhere.",
        "height" => "Element height. Desugars to `size.h` on `<Pad>`, `bbox.h` elsewhere.",
        "layer" => "PCB copper layer name (`F.Cu`, `B.Cu`, `In1.Cu`…). Verbatim string.",
        "layer-band" => "Comma-separated layer band, e.g. `F.Cu,In1.Cu`. Desugars to `layers`.",
        "z-index" => "Layer-stack index. Desugars to `layer-index` (signed integer).",
        "from-layer" => "Via's start layer. Verbatim string.",
        "to-layer" => "Via's end layer. Verbatim string.",
        "drill" => "Via drill diameter (`<length>`).",
        "diameter" => "Via copper diameter (`<length>`).",
        "padding" => "Inner padding (`<length>`).",
        "margin" => "Outer clearance (`<length>`). Desugars to `clearance`.",
        "impedance" => "Net target impedance in ohms (bare number).",
        "tolerance" => "Allowed impedance tolerance percentage (bare number).",
        "preferred-layer" => "Routing preference: copper layer name.",
        "pin-layer" => "`strict` to pin the net to its preferred layer; `prefer` to allow router escape.",
        "display" => "`display: flex` activates flex-shape cluster layout.",
        "gap" => "Cluster gap between flex children (`<length>`).",
        "scope" => "Keepout scope, e.g. `net-class(ground)`.",
        "clip-path" => "Geometry shape — `inset`, `circle`, `ellipse`, `polygon`, `path`, `kicad-shape`.",
        _ => "Recognised `style=` property.",
    };
    format!("**`{prop}`** — {blurb}")
}

/// Hover Markdown for a `clip-path` shape function.
#[must_use]
pub fn clip_path_shape_markdown(shape: &str) -> String {
    let blurb = match shape {
        "inset" => "Rectangle inset — `inset(<t> <r> <b> <l>)` or `inset(<t> <r> <b> <l> round <r>)` for rounded corners. Uses sibling `width:`/`height:` for the overall box.",
        "circle" => "Circle — `circle(<r> at <cx> <cy>)`.",
        "ellipse" => "Ellipse — `ellipse(<rx> <ry> at <cx> <cy>)`.",
        "polygon" => "Polygon — `polygon(p1, p2, …)` with `<length> <length>` points.",
        "path" => "Raw SVG `d=` path — `path('M … L … Z')`.",
        "kicad-shape" => "Pass-through to a KiCad-native shape — `kicad-shape(\"<name>\", { … })`.",
        _ => "Clip-path shape.",
    };
    format!("**`{shape}(...)`** — {blurb}")
}

/// Hover Markdown for a well-known tag, if any.
#[must_use]
pub fn tag_markdown(tag: &str) -> Option<&'static str> {
    for entry in STRUCTURAL_TAGS
        .iter()
        .chain(PCB_TAGS.iter())
    {
        if entry.name == tag {
            return Some(entry.markdown);
        }
    }
    None
}

/// Hover Markdown for an attribute on a given tag — falls back to a
/// generic note when the (tag, attr) pair is open-vocab.
#[must_use]
pub fn attribute_markdown(tag: &str, attr: &str) -> String {
    match (tag, attr) {
        ("Graph", "id") | (_, "id") => {
            "**`id`** — unique node identifier within the graph. Required for every well-formed node.".to_string()
        }
        ("Subgraph", "name") => {
            "**`name`** — the subgraph's export key. `<Subsystem ref=\"…\">` references resolve against this name.".to_string()
        }
        ("Subsystem", "ref") => {
            "**`ref`** — splice target. Form: `<alias>:<name>` (cross-file) or `<name>` (in-file).".to_string()
        }
        ("import", "src") => {
            "**`src`** — path to the imported `.grx` file, resolved relative to this file's directory.".to_string()
        }
        ("import", "as") => {
            "**`as`** — alias prefix for the imported file's subgraph names. Must match `[a-zA-Z_][a-zA-Z0-9_]*`.".to_string()
        }
        ("edge", "source") => "**`source`** — id of the edge's source node.".to_string(),
        ("edge", "target") => "**`target`** — id of the edge's target node.".to_string(),
        (_, "style") => {
            "**`style`** — CSS-shape attribute string. Desugared into typed numeric attributes (`at`, `size`, `bbox`, `geometry`, …).".to_string()
        }
        (_, "lang") => {
            "**`lang`** — body language (`js`, `python`, `go`). Defaults to `js` when omitted.".to_string()
        }
        (_, "footprint") => "**`footprint`** — KiCad footprint library reference.".to_string(),
        (tag, attr) => format!("**`{attr}`** — attribute on `<{tag}>`."),
    }
}
