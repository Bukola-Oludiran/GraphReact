//! Multi-file resolver glue.
//!
//! Routes the LSP server through
//! [`graphreact_grx_parser::multi_file::resolve_multi_file_with`] so
//! unsaved-buffer text from the [`DocumentStore`] flows into the parser
//! AS IF it were on disk. Each `didChange` re-resolves the document's
//! root file plus any imports — the LSP outer-shell MVP doesn't yet
//! cache parses (Phase 1's `resolve_multi_file_cached` adds disk-mtime
//! invalidation; an unsaved-buffer-aware cache lives in M23.next).

use std::path::{Path, PathBuf};

use graphreact_grx_parser::ast::{GrxFile, GrxNode, NodeDecl, SubgraphDecl};
use graphreact_grx_parser::multi_file::{resolve_multi_file_with, MultiFileResolved};
use graphreact_grx_parser::{GrxParseError, GrxResult, SourceLoc};

use crate::documents::{path_to_uri_string, DocumentStore};

/// Resolve the multi-file shape of a `.grx` file using the document
/// store as the primary loader. Falls back to disk for any path not
/// open in an editor buffer.
pub async fn resolve_with_store(
    root: &Path,
    store: &DocumentStore,
) -> GrxResult<MultiFileResolved> {
    // Snapshot the in-memory buffers up front; the loader closure
    // is sync (FnMut) and we want to avoid awaiting inside it.
    let snapshot = snapshot_open_buffers(store).await;

    let loader = move |path: &Path| -> Result<String, std::io::Error> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let key = path_to_uri_string(&canonical)
            .or_else(|| canonical.to_str().map(|s| format!("file://{s}")));
        if let Some(uri) = key {
            if let Some(text) = snapshot
                .iter()
                .find(|(u, _)| u == &uri)
                .map(|(_, t)| t.clone())
            {
                return Ok(text);
            }
        }
        std::fs::read_to_string(path)
    };

    resolve_multi_file_with(root, loader)
}

async fn snapshot_open_buffers(store: &DocumentStore) -> Vec<(String, String)> {
    let uris = store.snapshot_uris().await;
    let mut out = Vec::with_capacity(uris.len());
    for uri_str in uris {
        let uri: tower_lsp_server::ls_types::Uri = match uri_str.parse() {
            Ok(u) => u,
            Err(_) => continue,
        };
        if let Some(doc) = store.get(&uri).await {
            out.push((uri_str, doc.text));
        }
    }
    out
}

/// Outcome of a `<Subsystem ref="…">` lookup: which file declares it
/// and the exact `<Subgraph>` source location.
#[derive(Debug, Clone)]
pub struct SubsystemTarget {
    pub file_path: PathBuf,
    pub subgraph_name: String,
    pub loc: SourceLoc,
}

/// Look up the `<Subgraph>` that a `<Subsystem ref="…">` resolves to,
/// walking the multi-file resolver's results. Supports both bare
/// in-file refs and aliased `<alias>:<name>` refs.
#[must_use]
pub fn resolve_subsystem_target(
    resolved: &MultiFileResolved,
    ref_value: &str,
) -> Option<SubsystemTarget> {
    // Bare ref: look in the root file's subgraphs first.
    let (alias, bare_name) = match ref_value.find(':') {
        Some(idx) => {
            let (alias, name) = ref_value.split_at(idx);
            (Some(alias.to_string()), name[1..].to_string())
        }
        None => (None, ref_value.to_string()),
    };

    // Bare refs resolve against the root file alone.
    if alias.is_none() {
        for sg in &resolved.root.subgraphs {
            if sg.name == bare_name {
                return Some(SubsystemTarget {
                    file_path: resolved.root_path.clone(),
                    subgraph_name: bare_name,
                    loc: sg.loc,
                });
            }
        }
        return None;
    }

    let alias = alias.unwrap();
    // Aliased refs walk the imports. We resolve the import directive
    // by matching `as=`, then look up the subgraph in the imported
    // file. The qualified-name machinery from
    // `MultiFileResolved::collect_root_subgraphs` is more powerful
    // (DFS-chain aliasing), but for the LSP MVP a single-hop alias
    // resolution suffices and matches the common authoring pattern.
    let mut import_path: Option<PathBuf> = None;
    let mut visited: Vec<PathBuf> = Vec::new();
    for import in &resolved.root.imports {
        if import.alias.as_deref() == Some(&alias) {
            let root_dir = resolved
                .root_path
                .parent()
                .unwrap_or_else(|| Path::new("."));
            let candidate = root_dir.join(&import.src);
            let canonical = candidate.canonicalize().ok().unwrap_or(candidate);
            visited.push(canonical.clone());
            import_path = Some(canonical);
            break;
        }
    }
    let import_path = import_path?;
    let imported_file = resolved
        .imports
        .iter()
        .find(|i| i.path == import_path)
        .map(|i| &i.file)?;
    locate_subgraph(imported_file, &import_path, &bare_name)
}

fn locate_subgraph(file: &GrxFile, file_path: &Path, name: &str) -> Option<SubsystemTarget> {
    for sg in &file.subgraphs {
        if sg.name == name {
            return Some(SubsystemTarget {
                file_path: file_path.to_path_buf(),
                subgraph_name: name.to_string(),
                loc: sg.loc,
            });
        }
    }
    None
}

/// Outcome of an `<import>` lookup: which file the alias addresses.
#[derive(Debug, Clone)]
pub struct ImportTarget {
    pub file_path: PathBuf,
    pub loc: SourceLoc,
}

/// Resolve an import alias to the imported file. Returns the
/// canonical path plus the `<import>` directive's own source location
/// (so the LSP can also "go to the import line" from a tag-name
/// click).
#[must_use]
pub fn resolve_import_alias(resolved: &MultiFileResolved, alias: &str) -> Option<ImportTarget> {
    let root_dir = resolved
        .root_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    for import in &resolved.root.imports {
        if import.alias.as_deref() == Some(alias) {
            let candidate = root_dir.join(&import.src);
            let canonical = candidate.canonicalize().ok().unwrap_or(candidate);
            return Some(ImportTarget {
                file_path: canonical,
                loc: import.loc,
            });
        }
    }
    None
}

/// Walk every `NodeDecl` in a file (root graph + every subgraph) so
/// the LSP can answer "what tag is at this position?" without going
/// through `build_ir`. Avoids losing the open-vocab tag names that the
/// IR coalesces away on splice resolution.
pub fn walk_decls<'a>(file: &'a GrxFile) -> Vec<&'a NodeDecl> {
    let mut out = Vec::new();
    if let Some(g) = &file.graph {
        collect_node_decls(&g.children, &mut out);
    }
    for sg in &file.subgraphs {
        collect_node_decls(&sg.children, &mut out);
    }
    out
}

fn collect_node_decls<'a>(nodes: &'a [GrxNode], out: &mut Vec<&'a NodeDecl>) {
    for n in nodes {
        if let GrxNode::Node(decl) = n {
            out.push(decl);
            collect_node_decls(&decl.children, out);
        }
    }
}

/// Collect every `<Subgraph>` declared anywhere in the multi-file
/// resolution. Used by completion to offer subsystem ref candidates.
#[must_use]
pub fn collect_all_subgraphs(resolved: &MultiFileResolved) -> Vec<(String, SubgraphDecl)> {
    let mut out: Vec<(String, SubgraphDecl)> = Vec::new();
    for sg in &resolved.root.subgraphs {
        out.push((sg.name.clone(), sg.clone()));
    }
    for imp in &resolved.imports {
        // Look up the alias prefix from the root's import directives.
        let alias = resolved
            .root
            .imports
            .iter()
            .find(|d| {
                resolved
                    .root_path
                    .parent()
                    .map(|p| p.join(&d.src))
                    .and_then(|p| p.canonicalize().ok())
                    .map(|p| p == imp.path)
                    .unwrap_or(false)
            })
            .and_then(|d| d.alias.clone());
        for sg in &imp.file.subgraphs {
            let qualified = if let Some(a) = &alias {
                format!("{a}:{}", sg.name)
            } else {
                sg.name.clone()
            };
            out.push((qualified, sg.clone()));
        }
    }
    out
}

/// Convenience: drop an `_ = err` for the unused-import-related
/// branches in the LSP server so it compiles without trailing
/// warnings when the error machinery isn't pulled in yet.
#[allow(dead_code)]
const fn _silence_grx_error_import(_: &GrxParseError) {}
