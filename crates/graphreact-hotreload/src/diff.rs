//! IR diff classifier for hot reload.
//!
//! Takes two `GrxIr` snapshots (before + after) and classifies what
//! changed:
//!
//! - **Compatible**: only definition-body source changed for some
//!   set of components, and the change is structurally safe to swap
//!   without remounting (preserves `useState`, pending message
//!   queue). In this initial phase the test is conservative —
//!   any body change → Incompatible. Phase 3 upgrades this to a
//!   per-language hook-signature comparison so a string-literal
//!   tweak stays Compatible.
//! - **Incompatible**: the change requires drain + unmount + remount
//!   for the affected nodes (new node id, removed node id, component
//!   rename, prop type change, lang change, body changed).
//! - **TopologyOnly**: only edges differ. Nodes are unchanged, no
//!   body changes. Re-edge in place via `TopologyEvent`s.
//! - **Mixed**: combination. Each `NodeDelta` / `EdgeDelta` carries
//!   the per-item classification so the orchestrator can apply each
//!   in the right mode.
//!
//! The classifier is pure — no scheduler interaction, no I/O. Easy to
//! golden-test against fixture pairs.

use std::collections::BTreeMap;

use graphreact_grx_parser::{
    ast::{AttrMap, ComponentLang},
    ir::{EdgeKind, IrDefinition},
    GrxIr, IrEdge, IrNode,
};
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use crate::hook_signature::{extract as extract_hook_signature, HookSignature};

/// Wire-stable encoding of an edge kind so we can use it as a
/// composite key across edge-diff lookups (the parser's
/// [`EdgeKind`] doesn't derive `Ord`, but a `u8` does).
fn edge_kind_key(k: EdgeKind) -> u8 {
    match k {
        EdgeKind::Structural => 0,
        EdgeKind::Explicit => 1,
    }
}

/// Top-level diff outcome. The orchestrator consumes this to drive
/// per-node + per-edge apply paths.
#[derive(Debug, Clone, PartialEq)]
pub enum ReloadDelta {
    /// No diff — safely no-op.
    NoChange,
    /// All changes are body-only and per-component compatible.
    Compatible(Vec<NodeDelta>),
    /// At least one change requires drain + remount.
    Incompatible(Vec<NodeDelta>),
    /// Only edges differ.
    TopologyOnly(Vec<EdgeDelta>),
    /// Combination — drives a mixed-mode apply.
    Mixed {
        compat: Vec<NodeDelta>,
        incompat: Vec<NodeDelta>,
        topology: Vec<EdgeDelta>,
    },
}

/// Per-node change record.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeDelta {
    pub node_id: String,
    pub component: String,
    pub kind: NodeDeltaKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeDeltaKind {
    /// Component body source changed; classifier judged it safe to
    /// swap without remount (compatible reload).
    BodyChangedCompatible {
        component: String,
        new_code: String,
        new_lang: ComponentLang,
    },
    /// Component body source changed but the change requires
    /// remount (e.g. body-hash differs and the conservative
    /// classifier flags it; Phase 3 will upgrade so this only
    /// fires when hook signatures actually diverge).
    BodyChangedIncompatible {
        component: String,
        old_code: String,
        new_code: String,
        new_lang: ComponentLang,
    },
    /// Node id is new.
    Added,
    /// Node id was removed.
    Removed,
    /// Node id exists in both but its component name changed (e.g.
    /// `<Logger id="x" />` → `<Auditor id="x" />`).
    ComponentRenamed { old: String, new: String },
    /// Props on the node changed in a way that requires remount.
    /// Today: any attr value change at all (conservative). Phase 3
    /// can refine to "only changes in attrs marked structural in
    /// the manifest force remount."
    PropsChanged,
    /// `lang` attribute changed (Js ↔ Python). Always requires
    /// remount — different engine entirely.
    LangChanged {
        old: ComponentLang,
        new: ComponentLang,
    },
}

/// Per-edge change record.
#[derive(Debug, Clone, PartialEq)]
pub enum EdgeDelta {
    Added {
        source: String,
        target: String,
        attrs: AttrMap,
        kind: EdgeKind,
    },
    Removed {
        source: String,
        target: String,
        kind: EdgeKind,
    },
    AttrsChanged {
        source: String,
        target: String,
        kind: EdgeKind,
        new_attrs: AttrMap,
    },
}

/// Per-component cache of body hashes plus per-language hook
/// signatures. Reused across multiple `classify` calls so unchanged
/// definitions don't get rehashed plus reparsed every reload.
///
/// **M15 Phase 3** added the `hook_sigs` cache so the per-language
/// AST extraction (swc plus rustpython-parser, both moderately
/// expensive) is amortised across reload events. Keyed by body-hash
/// so identical bodies seen under different component names share
/// the cached signature.
#[derive(Default)]
pub struct DiffCache {
    body_hashes: FxHashMap<String, [u8; 32]>,
    /// Cache of `(body_hash, lang) -> Result<HookSignature, ()>`. The
    /// `Err(())` branch is for sources that failed to extract — we
    /// don't reparse them on every event. Phase-3-shape; if Phase 1
    /// (tree-sitter) lands the cache key migrates to the tree id.
    hook_sigs: FxHashMap<([u8; 32], ComponentLang), Result<HookSignature, ()>>,
}

impl DiffCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn body_hash(&mut self, key: &str, code: &str) -> [u8; 32] {
        if let Some(h) = self.body_hashes.get(key) {
            return *h;
        }
        let mut hasher = Sha256::new();
        hasher.update(code.as_bytes());
        let h: [u8; 32] = hasher.finalize().into();
        self.body_hashes.insert(key.to_string(), h);
        h
    }

    /// **M15 Phase 3.** Cached hook-signature extraction. `Ok(sig)` =
    /// the body's hook structure parsed cleanly; `Err(())` = parse
    /// failure or dynamic dispatch (callers fall back to conservative
    /// classification).
    fn hook_signature(
        &mut self,
        body_hash: [u8; 32],
        lang: ComponentLang,
        code: &str,
    ) -> Result<HookSignature, ()> {
        if let Some(sig) = self.hook_sigs.get(&(body_hash, lang)) {
            return sig.clone();
        }
        let sig = extract_hook_signature(code, lang).map_err(|_| ());
        self.hook_sigs.insert((body_hash, lang), sig.clone());
        sig
    }
}

/// Classify a reload delta from two IR snapshots.
///
/// `cache` is a per-process diff cache that survives across reloads;
/// pass a fresh `DiffCache::new()` if caching across reloads isn't
/// desired (the cache only memoizes hash computation — it never
/// affects the diff result).
pub fn classify(before: &GrxIr, after: &GrxIr, cache: &mut DiffCache) -> ReloadDelta {
    // 1) Per-definition body diff. A definition's body change can
    //    affect every node that uses that component.
    let before_defs: BTreeMap<&str, &IrDefinition> = before
        .definitions
        .iter()
        .map(|d| (d.component.as_str(), d))
        .collect();
    let after_defs: BTreeMap<&str, &IrDefinition> = after
        .definitions
        .iter()
        .map(|d| (d.component.as_str(), d))
        .collect();

    // Per-component body classification. Used when iterating nodes
    // to derive the per-node NodeDelta::BodyChanged* variant.
    let mut body_class: FxHashMap<String, BodyClassification> = FxHashMap::default();
    for component in before_defs.keys().chain(after_defs.keys()) {
        let key: String = (*component).to_string();
        if body_class.contains_key(&key) {
            continue;
        }
        let class = match (before_defs.get(component), after_defs.get(component)) {
            (Some(b), Some(a)) => classify_definition_change(b, a, cache),
            (Some(_), None) => BodyClassification::DefinitionRemoved,
            (None, Some(_)) => BodyClassification::DefinitionAdded,
            (None, None) => continue,
        };
        body_class.insert(key, class);
    }

    // 2) Per-node diff. Match by node id.
    let before_nodes: BTreeMap<&str, &IrNode> =
        before.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let after_nodes: BTreeMap<&str, &IrNode> =
        after.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut compat: Vec<NodeDelta> = Vec::new();
    let mut incompat: Vec<NodeDelta> = Vec::new();

    for (id, before_node) in &before_nodes {
        match after_nodes.get(id) {
            None => {
                incompat.push(NodeDelta {
                    node_id: (*id).to_string(),
                    component: before_node.component.clone(),
                    kind: NodeDeltaKind::Removed,
                });
            }
            Some(after_node) => {
                if before_node.component != after_node.component {
                    incompat.push(NodeDelta {
                        node_id: (*id).to_string(),
                        component: after_node.component.clone(),
                        kind: NodeDeltaKind::ComponentRenamed {
                            old: before_node.component.clone(),
                            new: after_node.component.clone(),
                        },
                    });
                    continue;
                }
                if attrs_differ_structurally(&before_node.attrs, &after_node.attrs) {
                    incompat.push(NodeDelta {
                        node_id: (*id).to_string(),
                        component: after_node.component.clone(),
                        kind: NodeDeltaKind::PropsChanged,
                    });
                    continue;
                }
                // Component matches; props match. Defer to the
                // per-component body classification.
                if let Some(class) = body_class.get(&after_node.component) {
                    if let Some(delta_kind) = node_kind_from_body_class(class) {
                        let delta = NodeDelta {
                            node_id: (*id).to_string(),
                            component: after_node.component.clone(),
                            kind: delta_kind.clone(),
                        };
                        if matches!(delta_kind, NodeDeltaKind::BodyChangedCompatible { .. }) {
                            compat.push(delta);
                        } else {
                            incompat.push(delta);
                        }
                    }
                }
            }
        }
    }

    // Newly-added nodes.
    for (id, after_node) in &after_nodes {
        if before_nodes.contains_key(id) {
            continue;
        }
        incompat.push(NodeDelta {
            node_id: (*id).to_string(),
            component: after_node.component.clone(),
            kind: NodeDeltaKind::Added,
        });
    }

    // 3) Edge diff. Match by (source, target, kind).
    let topology = diff_edges(&before.edges, &after.edges);

    // 4) Combine into the top-level shape.
    match (compat.is_empty(), incompat.is_empty(), topology.is_empty()) {
        (true, true, true) => ReloadDelta::NoChange,
        (false, true, true) => ReloadDelta::Compatible(compat),
        (true, false, true) => ReloadDelta::Incompatible(incompat),
        (true, true, false) => ReloadDelta::TopologyOnly(topology),
        _ => ReloadDelta::Mixed {
            compat,
            incompat,
            topology,
        },
    }
}

/// Internal classification of a single definition's change.
#[derive(Debug, Clone)]
enum BodyClassification {
    Unchanged,
    /// **M15 Phase 3.** Body source differs but per-language
    /// hook-AST extraction confirms the hook structure is
    /// identical — safe to swap via `replace_definition` while
    /// preserving every `HookSlot`.
    BodyChangedCompatible {
        new_code: String,
        new_lang: ComponentLang,
    },
    /// Body source differs AND the hook structure differs (or the
    /// extraction failed and we fall back conservatively). Drain
    /// plus remount path.
    BodyChangedIncompatible {
        old_code: String,
        new_code: String,
        new_lang: ComponentLang,
    },
    LangChanged {
        old: ComponentLang,
        new: ComponentLang,
    },
    /// Definition was added in `after`. The runtime side handles
    /// this implicitly when nodes referencing the new definition
    /// mount; classification carries no extra payload.
    DefinitionAdded,
    DefinitionRemoved,
}

fn classify_definition_change(
    before: &IrDefinition,
    after: &IrDefinition,
    cache: &mut DiffCache,
) -> BodyClassification {
    if before.lang != after.lang {
        return BodyClassification::LangChanged {
            old: before.lang,
            new: after.lang,
        };
    }
    let before_key = format!("{}::before", before.component);
    let after_key = format!("{}::after", after.component);
    let before_h = cache.body_hash(&before_key, &before.code);
    let after_h = cache.body_hash(&after_key, &after.code);
    if before_h == after_h {
        return BodyClassification::Unchanged;
    }

    // **M15 Phase 3** — per-language hook-AST diffing. Bodies hash
    // differently; ask the AST extractor whether the hook structure
    // changed. Identical structures plus differing source = the
    // precision win (string-literal-only changes preserve
    // `useState`).
    //
    // Failure modes (parse error, dynamic dispatch, language we
    // can't statically verify) fall back to the conservative
    // `BodyChangedIncompatible` classification — same shape as
    // pre-Phase-3 behavior. Drain plus remount destroys state but
    // never silently corrupts it.
    let before_sig = cache.hook_signature(before_h, before.lang, &before.code);
    let after_sig = cache.hook_signature(after_h, after.lang, &after.code);
    let promote = match (&before_sig, &after_sig) {
        (Ok(b), Ok(a)) => b.compatible_with(a),
        _ => false,
    };
    if promote {
        BodyClassification::BodyChangedCompatible {
            new_code: after.code.clone(),
            new_lang: after.lang,
        }
    } else {
        BodyClassification::BodyChangedIncompatible {
            old_code: before.code.clone(),
            new_code: after.code.clone(),
            new_lang: after.lang,
        }
    }
}

fn node_kind_from_body_class(class: &BodyClassification) -> Option<NodeDeltaKind> {
    match class {
        BodyClassification::Unchanged => None,
        BodyClassification::BodyChangedCompatible { new_code, new_lang } => {
            Some(NodeDeltaKind::BodyChangedCompatible {
                component: String::new(), // filled in by caller via NodeDelta.component
                new_code: new_code.clone(),
                new_lang: *new_lang,
            })
        }
        BodyClassification::BodyChangedIncompatible {
            old_code,
            new_code,
            new_lang,
        } => Some(NodeDeltaKind::BodyChangedIncompatible {
            component: String::new(), // filled in by caller via NodeDelta.component
            old_code: old_code.clone(),
            new_code: new_code.clone(),
            new_lang: *new_lang,
        }),
        BodyClassification::LangChanged { old, new } => Some(NodeDeltaKind::LangChanged {
            old: *old,
            new: *new,
        }),
        BodyClassification::DefinitionAdded => {
            // A new definition with no prior code is Compatible-ish
            // for nodes that didn't exist before; for existing nodes
            // whose component is the new definition, treat as
            // Incompatible (their body changed from "not defined"
            // to "defined", semantically a remount).
            Some(NodeDeltaKind::PropsChanged)
        }
        BodyClassification::DefinitionRemoved => Some(NodeDeltaKind::Removed),
    }
}

fn attrs_differ_structurally(before: &AttrMap, after: &AttrMap) -> bool {
    // We inject `_depth`, `_parentPath`, `component` into IR attrs
    // — those reflect parent-path / component-id and are tracked
    // separately. Compare only user-supplied attrs.
    let strip = |m: &AttrMap| -> AttrMap {
        let mut out = AttrMap::new();
        for (k, v) in m {
            if k == "_depth" || k == "_parentPath" || k == "component" {
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
        out
    };
    strip(before) != strip(after)
}

fn diff_edges(before: &[IrEdge], after: &[IrEdge]) -> Vec<EdgeDelta> {
    // Use (String, String, u8) as the BTreeMap key since EdgeKind
    // doesn't derive Ord. We also keep the original EdgeKind in
    // the value tuple so downstream EdgeDelta variants reconstruct
    // the typed kind without re-parsing.
    type EdgeKey = (String, String, u8);
    let key =
        |e: &IrEdge| -> EdgeKey { (e.source.clone(), e.target.clone(), edge_kind_key(e.kind)) };
    let before_map: BTreeMap<EdgeKey, &IrEdge> = before.iter().map(|e| (key(e), e)).collect();
    let after_map: BTreeMap<EdgeKey, &IrEdge> = after.iter().map(|e| (key(e), e)).collect();

    let mut deltas: Vec<EdgeDelta> = Vec::new();
    for (k, before_edge) in &before_map {
        match after_map.get(k) {
            None => deltas.push(EdgeDelta::Removed {
                source: k.0.clone(),
                target: k.1.clone(),
                kind: before_edge.kind,
            }),
            Some(after_edge) => {
                if before_edge.attrs != after_edge.attrs {
                    deltas.push(EdgeDelta::AttrsChanged {
                        source: k.0.clone(),
                        target: k.1.clone(),
                        kind: after_edge.kind,
                        new_attrs: after_edge.attrs.clone(),
                    });
                }
            }
        }
    }
    for (k, after_edge) in &after_map {
        if before_map.contains_key(k) {
            continue;
        }
        deltas.push(EdgeDelta::Added {
            source: k.0.clone(),
            target: k.1.clone(),
            attrs: after_edge.attrs.clone(),
            kind: after_edge.kind,
        });
    }
    deltas
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_grx_parser::ir::parse_to_ir;

    fn parse(src: &str) -> GrxIr {
        parse_to_ir(src).expect("parse")
    }

    #[test]
    fn no_change_returns_nochange() {
        let src = r#"<Foo>const x = 1;</Foo><Graph id="g"><Foo id="a" /></Graph>"#;
        let a = parse(src);
        let b = parse(src);
        let mut cache = DiffCache::new();
        match classify(&a, &b, &mut cache) {
            ReloadDelta::NoChange => {}
            other => panic!("expected NoChange, got {other:?}"),
        }
    }

    #[test]
    fn body_change_with_no_hooks_promotes_to_compatible() {
        // **M15 Phase 3 (the precision win).** Bodies with no hook
        // calls have identical empty hook signatures across edits;
        // such changes promote from Incompatible to Compatible.
        // Same shape applies to bodies with same-shape hook
        // sequences differing only in string literals.
        let before = parse(r#"<Foo>const x = 1;</Foo><Graph id="g"><Foo id="a" /></Graph>"#);
        let after = parse(r#"<Foo>const x = 2;</Foo><Graph id="g"><Foo id="a" /></Graph>"#);
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::Compatible(deltas) => {
                assert_eq!(deltas.len(), 1);
                assert_eq!(deltas[0].node_id, "a");
                assert!(matches!(
                    deltas[0].kind,
                    NodeDeltaKind::BodyChangedCompatible { .. }
                ));
            }
            other => panic!("expected Compatible (Phase 3 promotion), got {other:?}"),
        }
    }

    #[test]
    fn body_change_adding_use_state_classifies_incompatible() {
        // Phase 3: a body change that ADDS a hook (changing the
        // hook signature length) must stay Incompatible — adding
        // `useState` shifts the slot table.
        let before = parse(r#"<Foo>const x = 1;</Foo><Graph id="g"><Foo id="a" /></Graph>"#);
        let after =
            parse(r#"<Foo>useState(0); const x = 1;</Foo><Graph id="g"><Foo id="a" /></Graph>"#);
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::Incompatible(deltas) => {
                assert_eq!(deltas.len(), 1);
                assert_eq!(deltas[0].node_id, "a");
                assert!(matches!(
                    deltas[0].kind,
                    NodeDeltaKind::BodyChangedIncompatible { .. }
                ));
            }
            other => panic!("expected Incompatible (added hook), got {other:?}"),
        }
    }

    #[test]
    fn body_change_with_string_literal_only_promotes_to_compatible() {
        // Phase 3 marquee: identical hook structure plus differing
        // string-literal changes → Compatible. This is the
        // precision-win acceptance gate (M15 plan line 1045).
        let before = parse(
            r#"<Foo>useState(0); useEffect(function() {}, [x]); host_observe('v1');</Foo><Graph id="g"><Foo id="a" /></Graph>"#,
        );
        let after = parse(
            r#"<Foo>useState(0); useEffect(function() {}, [x]); host_observe('v2');</Foo><Graph id="g"><Foo id="a" /></Graph>"#,
        );
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::Compatible(deltas) => {
                assert_eq!(deltas.len(), 1);
                assert!(matches!(
                    deltas[0].kind,
                    NodeDeltaKind::BodyChangedCompatible { .. }
                ));
            }
            other => panic!("expected Compatible (precision-win), got {other:?}"),
        }
    }

    #[test]
    fn added_edge_classifies_topology_only() {
        let before = parse(r#"<Graph id="g"><A id="a" /><B id="b" /></Graph>"#);
        let after = parse(
            r#"<Graph id="g"><A id="a" /><B id="b" /><edge source="a" target="b" /></Graph>"#,
        );
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::TopologyOnly(deltas) => {
                assert_eq!(deltas.len(), 1);
                assert!(matches!(deltas[0], EdgeDelta::Added { .. }));
            }
            other => panic!("expected TopologyOnly, got {other:?}"),
        }
    }

    #[test]
    fn removed_edge_classifies_topology_only() {
        let before = parse(
            r#"<Graph id="g"><A id="a" /><B id="b" /><edge source="a" target="b" /></Graph>"#,
        );
        let after = parse(r#"<Graph id="g"><A id="a" /><B id="b" /></Graph>"#);
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::TopologyOnly(deltas) => {
                assert_eq!(deltas.len(), 1);
                assert!(matches!(deltas[0], EdgeDelta::Removed { .. }));
            }
            other => panic!("expected TopologyOnly, got {other:?}"),
        }
    }

    #[test]
    fn added_node_classifies_incompatible() {
        let before = parse(r#"<Graph id="g"><A id="a" /></Graph>"#);
        let after = parse(r#"<Graph id="g"><A id="a" /><B id="b" /></Graph>"#);
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::Incompatible(deltas) => {
                assert!(deltas
                    .iter()
                    .any(|d| d.node_id == "b" && matches!(d.kind, NodeDeltaKind::Added)));
            }
            other => panic!("expected Incompatible, got {other:?}"),
        }
    }

    #[test]
    fn removed_node_classifies_incompatible() {
        let before = parse(r#"<Graph id="g"><A id="a" /><B id="b" /></Graph>"#);
        let after = parse(r#"<Graph id="g"><A id="a" /></Graph>"#);
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::Incompatible(deltas) => {
                assert!(deltas
                    .iter()
                    .any(|d| d.node_id == "b" && matches!(d.kind, NodeDeltaKind::Removed)));
            }
            other => panic!("expected Incompatible, got {other:?}"),
        }
    }

    #[test]
    fn component_rename_classifies_incompatible() {
        let before = parse(r#"<Graph id="g"><A id="x" /></Graph>"#);
        let after = parse(r#"<Graph id="g"><B id="x" /></Graph>"#);
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::Incompatible(deltas) => {
                assert_eq!(deltas[0].node_id, "x");
                assert!(matches!(
                    deltas[0].kind,
                    NodeDeltaKind::ComponentRenamed { .. }
                ));
            }
            other => panic!("expected Incompatible, got {other:?}"),
        }
    }

    #[test]
    fn lang_change_classifies_incompatible() {
        let before = parse(r#"<Foo lang="js">x</Foo><Graph id="g"><Foo id="f" /></Graph>"#);
        let after = parse(r#"<Foo lang="python">x</Foo><Graph id="g"><Foo id="f" /></Graph>"#);
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::Incompatible(deltas) => {
                assert!(deltas
                    .iter()
                    .any(|d| matches!(d.kind, NodeDeltaKind::LangChanged { .. })));
            }
            other => panic!("expected Incompatible, got {other:?}"),
        }
    }

    #[test]
    fn mixed_change_routes_each_part() {
        // **M15 Phase 3.** Body change on Foo (Compatible — same
        // empty hook signature; string-literal-only edit) plus edge
        // added (TopologyOnly). The classifier produces a Mixed
        // delta with both compat and topology buckets populated.
        let before = parse(r#"<Foo>x</Foo><Graph id="g"><Foo id="f" /><B id="b" /></Graph>"#);
        let after = parse(
            r#"<Foo>y</Foo><Graph id="g"><Foo id="f" /><B id="b" /><edge source="f" target="b" /></Graph>"#,
        );
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::Mixed {
                compat,
                incompat,
                topology,
            } => {
                assert!(
                    compat.iter().any(|d| d.node_id == "f"),
                    "expected Foo's body change in the compat bucket (Phase 3 promotion)"
                );
                assert!(incompat.is_empty(), "no incompat expected here");
                assert!(topology
                    .iter()
                    .any(|e| matches!(e, EdgeDelta::Added { .. })));
            }
            other => panic!("expected Mixed, got {other:?}"),
        }
    }

    #[test]
    fn mixed_change_with_incompat_body_routes_correctly() {
        // Phase 3: body change that adds a hook (Incompatible) plus
        // edge added (TopologyOnly).
        let before = parse(r#"<Foo>x</Foo><Graph id="g"><Foo id="f" /><B id="b" /></Graph>"#);
        let after = parse(
            r#"<Foo>useState(0); x</Foo><Graph id="g"><Foo id="f" /><B id="b" /><edge source="f" target="b" /></Graph>"#,
        );
        let mut cache = DiffCache::new();
        match classify(&before, &after, &mut cache) {
            ReloadDelta::Mixed {
                compat,
                incompat,
                topology,
            } => {
                assert!(compat.is_empty());
                assert!(incompat.iter().any(|d| d.node_id == "f"));
                assert!(topology
                    .iter()
                    .any(|e| matches!(e, EdgeDelta::Added { .. })));
            }
            other => panic!("expected Mixed, got {other:?}"),
        }
    }

    #[test]
    fn diff_cache_reuses_hash_across_reloads() {
        let src = r#"<Foo>const x = 1;</Foo><Graph id="g"><Foo id="a" /></Graph>"#;
        let a = parse(src);
        let b = parse(src);
        let mut cache = DiffCache::new();
        // First call populates cache.
        let _ = classify(&a, &b, &mut cache);
        // Second call should hit cached body hashes for Foo.
        let _ = classify(&a, &b, &mut cache);
        assert!(!cache.body_hashes.is_empty());
    }
}
