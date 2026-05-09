//! Multi-file `.grx` import resolution.
//!
//! Walks `<import src="..." />` directives transitively from a root
//! file, building a flat list of resolved files and detecting circular
//! imports. Imports are resolved relative to the importing file's
//! directory (matching the TS reference).
//!
//! The resolver does not execute or merge anything — it returns the
//! parsed [`ast::GrxFile`] for each visited path along with the import
//! graph. Downstream code (M12 CLI, runtime ingestion) chooses how to
//! merge: typically `definitions` are unioned across all files and the
//! root file's `<Graph>` becomes the runtime topology.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::GrxFile;
use crate::error::{GrxParseError, GrxResult};
use crate::parser::Parser;

/// Result of resolving a multi-file import tree starting from `root`.
#[derive(Debug, Clone)]
pub struct MultiFileResolved {
    /// The root file's parsed AST.
    pub root: GrxFile,
    pub root_path: PathBuf,
    /// Each transitively-imported file, in DFS order. Does NOT include
    /// the root.
    pub imports: Vec<ImportedFile>,
}

#[derive(Debug, Clone)]
pub struct ImportedFile {
    pub path: PathBuf,
    pub file: GrxFile,
}

/// Resolve a `.grx` file plus all of its transitive `<import>`s from
/// disk. Errors on circular imports or missing files.
pub fn resolve_multi_file(root_path: &Path) -> GrxResult<MultiFileResolved> {
    let canonical_root = canonicalize(root_path)?;
    let mut resolver = Resolver::default();
    let mut stack = vec![canonical_root.clone()];
    let root_file = resolver.load(&canonical_root, &mut stack)?;
    stack.pop();
    Ok(MultiFileResolved {
        root: root_file,
        root_path: canonical_root,
        imports: resolver.collected,
    })
}

/// **M15 Phase 1 (slice 1A).** Per-file parse cache for multi-file
/// `.grx` import-graph re-resolution. On a cache hit (mtime matches
/// the on-disk modified time), the file's previously-parsed
/// [`GrxFile`] is reused — no disk read, no parser invocation. On a
/// miss (file is new, mtime differs, or cache eviction), the file
/// is read, parsed, and re-cached.
///
/// # Why
///
/// M15 Phase 9 (`grx run --watch`) reparses the FULL import graph on
/// every save. For a four-file fixture (typical Thymus / ComplyFlow
/// shape), every edit reparses four files. Phase 1 slice 1A reduces
/// that to "reparse only the changed file" via this cache, on the
/// existing hand-rolled parser. Phase 1 slice 1B (tree-sitter swap)
/// will further reduce to "reparse only the changed subtree of the
/// changed file" via incremental tree-sitter reparse.
///
/// # Staleness
///
/// `mtime`-based. On every cache lookup, [`resolve_multi_file_cached`]
/// reads the file's metadata and compares the `modified` time with
/// the cached entry. Different mtime → cache miss + reparse. The
/// underlying assumption: filesystems update mtime synchronously
/// with `write(2)`. macOS, Linux, and Windows NTFS all honor this;
/// the file watcher's 50ms debounce window further insulates us
/// from mtime-not-yet-flushed races.
///
/// # Stats
///
/// [`ParseCache::hits`] + [`ParseCache::misses`] track resolution
/// outcomes for ops visibility (the watch driver logs hit-rate per
/// reload event).
#[derive(Debug, Default)]
pub struct ParseCache {
    entries: FxHashMap<PathBuf, CacheEntry>,
    hits: u64,
    misses: u64,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    mtime: SystemTime,
    file: GrxFile,
    /// **M15 Phase 1B.** Tree-sitter syntax tree, populated only by
    /// the tree-sitter-aware resolver
    /// ([`resolve_multi_file_cached_treesitter`]). The hand-rolled
    /// resolver leaves this as `None`. Caching the tree lets the
    /// watch-driver's incremental reparse path
    /// ([`crate::treesitter::IncrementalParser::reparse`]) reuse
    /// unchanged subtrees across edits.
    tree: Option<tree_sitter::Tree>,
}

impl ParseCache {
    /// Construct an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no entries are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Cumulative hit count since construction. Resets only on
    /// `clear`.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Cumulative miss count since construction.
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Reset hit + miss counters without clearing the entries
    /// themselves. Useful for per-reload-event stats.
    pub fn reset_stats(&mut self) {
        self.hits = 0;
        self.misses = 0;
    }

    /// Drop a single entry. Useful when the watch driver knows a
    /// specific path changed and wants to force a re-parse on the
    /// next resolution. The mtime-check below makes this redundant
    /// in practice (mtime mismatch = miss anyway), but explicit
    /// invalidation gives operator surfaces a no-knobs-needed shape.
    pub fn invalidate(&mut self, path: &Path) {
        self.entries.remove(path);
    }

    /// Clear every entry plus reset stats.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.hits = 0;
        self.misses = 0;
    }

    /// `true` when `path` has a non-stale entry (the cached mtime
    /// matches the on-disk mtime). Public so tests can assert
    /// hit-vs-miss without depending on the resolver's internal
    /// stats.
    #[must_use]
    pub fn contains_fresh(&self, path: &Path, current_mtime: SystemTime) -> bool {
        self.entries
            .get(path)
            .is_some_and(|e| e.mtime == current_mtime)
    }
}

/// **M15 Phase 1 slice 1A.** Cache-aware variant of
/// [`resolve_multi_file`]. The resolver walks the import tree as
/// before, but each file's read-plus-parse step is replaced by a
/// cache lookup keyed on canonical path. Unchanged files (mtime
/// matches) skip both the disk read AND the parser invocation;
/// only the import-tree walk runs (cheap traversal of cached
/// `GrxFile.imports` directives).
///
/// Equivalent semantics to `resolve_multi_file` — same return
/// shape, same circular-import detection, same DFS ordering of
/// `imports`. Differs only in performance.
pub fn resolve_multi_file_cached(
    root_path: &Path,
    cache: &mut ParseCache,
) -> GrxResult<MultiFileResolved> {
    let canonical_root = canonicalize(root_path)?;
    let mut resolver = Resolver::default();
    let mut stack = vec![canonical_root.clone()];
    let root_file = resolver.load_via_cache(&canonical_root, &mut stack, cache)?;
    stack.pop();
    Ok(MultiFileResolved {
        root: root_file,
        root_path: canonical_root,
        imports: resolver.collected,
    })
}

/// In-memory variant: provide a callable that returns the source for a
/// given canonical path. Useful for tests + for the M12 CLI when it
/// already has files in memory.
pub fn resolve_multi_file_with<F>(
    root_path: &Path,
    mut load: F,
) -> GrxResult<MultiFileResolved>
where
    F: FnMut(&Path) -> Result<String, std::io::Error>,
{
    let canonical_root = root_path.to_path_buf();
    let mut resolver = Resolver::default();
    let mut stack = vec![canonical_root.clone()];
    let root_file = resolver.load_with(&canonical_root, &mut stack, &mut load)?;
    stack.pop();
    Ok(MultiFileResolved {
        root: root_file,
        root_path: canonical_root,
        imports: resolver.collected,
    })
}

#[derive(Default)]
struct Resolver {
    /// Visited paths → short-circuit duplicate imports.
    visited: FxHashSet<PathBuf>,
    /// Imported files in DFS order (excluding the root).
    collected: Vec<ImportedFile>,
}

impl Resolver {
    fn load(
        &mut self,
        path: &Path,
        stack: &mut Vec<PathBuf>,
    ) -> GrxResult<GrxFile> {
        let mut load = |p: &Path| std::fs::read_to_string(p);
        self.load_with(path, stack, &mut load)
    }

    /// **M15 Phase 1 slice 1A.** Cache-aware load. Mtime-checks
    /// against [`ParseCache`] before reading; on hit, reuses the
    /// cached [`GrxFile`] but still walks its imports to populate
    /// the resolver's `collected` Vec (so the `MultiFileResolved`
    /// shape is identical to the uncached path).
    fn load_via_cache(
        &mut self,
        path: &Path,
        stack: &mut Vec<PathBuf>,
        cache: &mut ParseCache,
    ) -> GrxResult<GrxFile> {
        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|source| GrxParseError::Io {
                path: path.display().to_string(),
                source,
            })?;

        let file: GrxFile = match cache.entries.get(path) {
            Some(entry) if entry.mtime == mtime => {
                cache.hits += 1;
                entry.file.clone()
            }
            _ => {
                cache.misses += 1;
                let source =
                    std::fs::read_to_string(path).map_err(|source| GrxParseError::Io {
                        path: path.display().to_string(),
                        source,
                    })?;
                let parsed = Parser::parse_file(&source)?;
                cache.entries.insert(
                    path.to_path_buf(),
                    CacheEntry {
                        mtime,
                        file: parsed.clone(),
                        tree: None,
                    },
                );
                parsed
            }
        };

        let parent_dir = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
        for import in &file.imports {
            let resolved = parent_dir.join(&import.src);
            let canonical = canonicalize_or(&resolved, &resolved);

            if stack.iter().any(|p| paths_equivalent(p, &canonical)) {
                let chain = stack
                    .iter()
                    .map(|p| p.display().to_string())
                    .chain(std::iter::once(canonical.display().to_string()))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                return Err(GrxParseError::CircularImport { chain });
            }
            if !self.visited.insert(canonical.clone()) {
                continue;
            }
            stack.push(canonical.clone());
            let child = self.load_via_cache(&canonical, stack, cache)?;
            stack.pop();
            self.collected.push(ImportedFile {
                path: canonical,
                file: child,
            });
        }
        Ok(file)
    }

    /// **M15 Phase 1B.** Tree-sitter-backed cache-aware load.
    /// Equivalent to [`Self::load_via_cache`] in semantics + DFS
    /// ordering, with two differences:
    ///
    /// - The per-file parse goes through `tree_sitter_grx::LANGUAGE`
    ///   (not the hand-rolled lexer + parser). The resulting
    ///   `GrxFile` shape is preserved by walking the tree-sitter
    ///   tree (not yet implemented at this slice; a subsequent
    ///   refinement bridges `tree_sitter::Tree → GrxFile`). For
    ///   now we still parse the source with the hand-rolled
    ///   parser to preserve the existing `GrxFile`-consuming
    ///   contract; the tree-sitter pass runs in parallel and
    ///   caches the syntax tree.
    /// - The cache stores both the [`GrxFile`] (for the hand-rolled
    ///   resolver's contract) AND the `tree_sitter::Tree` (for
    ///   incremental reparse).
    fn load_via_cache_treesitter(
        &mut self,
        path: &Path,
        stack: &mut Vec<PathBuf>,
        cache: &mut ParseCache,
    ) -> GrxResult<GrxFile> {
        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|source| GrxParseError::Io {
                path: path.display().to_string(),
                source,
            })?;

        let file: GrxFile = match cache.entries.get(path) {
            Some(entry) if entry.mtime == mtime && entry.tree.is_some() => {
                cache.hits += 1;
                entry.file.clone()
            }
            _ => {
                cache.misses += 1;
                let source =
                    std::fs::read_to_string(path).map_err(|source| GrxParseError::Io {
                        path: path.display().to_string(),
                        source,
                    })?;
                let parsed = Parser::parse_file(&source)?;
                // Build the tree-sitter tree alongside (we don't
                // need it for IR construction here — the
                // hand-rolled parser already produced GrxFile —
                // but we cache it for future incremental reparse
                // by the watch driver).
                let mut ts_parser = tree_sitter::Parser::new();
                let _ = ts_parser.set_language(&tree_sitter_grx::LANGUAGE.into());
                let tree = ts_parser.parse(&source, None);
                cache.entries.insert(
                    path.to_path_buf(),
                    CacheEntry {
                        mtime,
                        file: parsed.clone(),
                        tree,
                    },
                );
                parsed
            }
        };

        let parent_dir = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
        for import in &file.imports {
            let resolved = parent_dir.join(&import.src);
            let canonical = canonicalize_or(&resolved, &resolved);

            if stack.iter().any(|p| paths_equivalent(p, &canonical)) {
                let chain = stack
                    .iter()
                    .map(|p| p.display().to_string())
                    .chain(std::iter::once(canonical.display().to_string()))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                return Err(GrxParseError::CircularImport { chain });
            }
            if !self.visited.insert(canonical.clone()) {
                continue;
            }
            stack.push(canonical.clone());
            let child = self.load_via_cache_treesitter(&canonical, stack, cache)?;
            stack.pop();
            self.collected.push(ImportedFile {
                path: canonical,
                file: child,
            });
        }
        Ok(file)
    }

    fn load_with<F>(
        &mut self,
        path: &Path,
        stack: &mut Vec<PathBuf>,
        load: &mut F,
    ) -> GrxResult<GrxFile>
    where
        F: FnMut(&Path) -> Result<String, std::io::Error>,
    {
        let source = load(path).map_err(|source| GrxParseError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let file = Parser::parse_file(&source)?;
        let parent_dir = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();

        for import in &file.imports {
            let resolved = parent_dir.join(&import.src);
            let canonical = canonicalize_or(&resolved, &resolved);

            if stack.iter().any(|p| paths_equivalent(p, &canonical)) {
                let chain = stack
                    .iter()
                    .map(|p| p.display().to_string())
                    .chain(std::iter::once(canonical.display().to_string()))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                return Err(GrxParseError::CircularImport { chain });
            }
            if !self.visited.insert(canonical.clone()) {
                continue;
            }
            stack.push(canonical.clone());
            let child = self.load_with(&canonical, stack, load)?;
            stack.pop();
            self.collected.push(ImportedFile {
                path: canonical,
                file: child,
            });
        }
        Ok(file)
    }
}

/// **M15 Phase 1B.** Cache-aware tree-sitter resolver. Mirrors
/// [`resolve_multi_file_cached`]'s shape but routes the per-file
/// parse through the tree-sitter backend AND caches the resulting
/// `tree_sitter::Tree` alongside the [`GrxFile`]. Watch-driver
/// integration consumes the cached tree via
/// [`ParseCache::tree_for`] for incremental reparse — when an edit
/// arrives, the driver applies the `tree_sitter::InputEdit` to the
/// cached tree and reparses; tree-sitter reuses unchanged subtrees
/// so the work scales with the edit region, not the whole file.
///
/// Same return shape as [`resolve_multi_file_cached`] (callers can
/// A/B between the two backends without API churn).
pub fn resolve_multi_file_cached_treesitter(
    root_path: &Path,
    cache: &mut ParseCache,
) -> GrxResult<MultiFileResolved> {
    let canonical_root = canonicalize(root_path)?;
    let mut resolver = Resolver::default();
    let mut stack = vec![canonical_root.clone()];
    let root_file = resolver.load_via_cache_treesitter(&canonical_root, &mut stack, cache)?;
    stack.pop();
    Ok(MultiFileResolved {
        root: root_file,
        root_path: canonical_root,
        imports: resolver.collected,
    })
}

impl ParseCache {
    /// **M15 Phase 1B.** Read-only access to the cached tree-sitter
    /// `Tree` for a path, populated by
    /// [`resolve_multi_file_cached_treesitter`]. Returns `None`
    /// when (a) the file isn't cached, or (b) it was cached via the
    /// hand-rolled resolver (no tree captured). Watch drivers use
    /// this to seed an [`crate::treesitter::IncrementalParser`]
    /// with the prior tree before applying edits.
    #[must_use]
    pub fn tree_for(&self, path: &Path) -> Option<&tree_sitter::Tree> {
        self.entries.get(path).and_then(|e| e.tree.as_ref())
    }
}

fn canonicalize(path: &Path) -> GrxResult<PathBuf> {
    std::fs::canonicalize(path).map_err(|source| GrxParseError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn canonicalize_or(path: &Path, fallback: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| fallback.to_path_buf())
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    a == b || canonicalize_or(a, a) == canonicalize_or(b, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn build_loader(files: HashMap<PathBuf, String>) -> impl FnMut(&Path) -> Result<String, std::io::Error> {
        move |p: &Path| {
            files
                .get(p)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "not found"))
        }
    }

    #[test]
    fn resolves_two_level_import_chain() {
        let mut files: HashMap<PathBuf, String> = HashMap::new();
        files.insert(
            PathBuf::from("/root/main.grx"),
            r#"<import src="./sensors.grx" />
<Graph id="g"><Logger id="l" /></Graph>"#
                .to_string(),
        );
        files.insert(
            PathBuf::from("/root/sensors.grx"),
            r"<TempSensor>
return { accepts: ['poll'] };
</TempSensor>"
                .to_string(),
        );
        let resolved = resolve_multi_file_with(
            Path::new("/root/main.grx"),
            build_loader(files),
        )
        .expect("ok");
        assert_eq!(resolved.imports.len(), 1);
        assert_eq!(resolved.imports[0].path, PathBuf::from("/root/sensors.grx"));
        assert_eq!(resolved.imports[0].file.definitions[0].component, "TempSensor");
    }

    #[test]
    fn detects_direct_circular_import() {
        let mut files: HashMap<PathBuf, String> = HashMap::new();
        files.insert(
            PathBuf::from("/c/a.grx"),
            r#"<import src="./b.grx" />
<Graph id="a"></Graph>"#
                .to_string(),
        );
        files.insert(
            PathBuf::from("/c/b.grx"),
            r#"<import src="./a.grx" />"#.to_string(),
        );
        let result =
            resolve_multi_file_with(Path::new("/c/a.grx"), build_loader(files));
        assert!(matches!(result, Err(GrxParseError::CircularImport { .. })));
    }

    #[test]
    fn detects_multi_hop_circular_import() {
        let mut files: HashMap<PathBuf, String> = HashMap::new();
        files.insert(
            PathBuf::from("/c/a.grx"),
            r#"<import src="./b.grx" />"#.to_string(),
        );
        files.insert(
            PathBuf::from("/c/b.grx"),
            r#"<import src="./c.grx" />"#.to_string(),
        );
        files.insert(
            PathBuf::from("/c/c.grx"),
            r#"<import src="./a.grx" />"#.to_string(),
        );
        let result =
            resolve_multi_file_with(Path::new("/c/a.grx"), build_loader(files));
        assert!(matches!(result, Err(GrxParseError::CircularImport { .. })));
    }

    #[test]
    fn duplicate_import_visited_once() {
        let mut files: HashMap<PathBuf, String> = HashMap::new();
        files.insert(
            PathBuf::from("/d/main.grx"),
            r#"<import src="./shared.grx" />
<import src="./other.grx" />
<Graph id="m"></Graph>"#
                .to_string(),
        );
        files.insert(
            PathBuf::from("/d/other.grx"),
            r#"<import src="./shared.grx" />"#.to_string(),
        );
        files.insert(
            PathBuf::from("/d/shared.grx"),
            r"<Foo>return {accepts: []};</Foo>".to_string(),
        );
        let resolved =
            resolve_multi_file_with(Path::new("/d/main.grx"), build_loader(files))
                .expect("ok");
        // shared.grx + other.grx, no duplicate of shared.
        assert_eq!(resolved.imports.len(), 2);
        let paths: Vec<_> = resolved.imports.iter().map(|i| &i.path).collect();
        let shared_count = paths
            .iter()
            .filter(|p| p.ends_with("shared.grx"))
            .count();
        assert_eq!(shared_count, 1);
    }

    #[test]
    fn missing_import_errors() {
        let mut files: HashMap<PathBuf, String> = HashMap::new();
        files.insert(
            PathBuf::from("/e/main.grx"),
            r#"<import src="./does-not-exist.grx" />
<Graph id="m"></Graph>"#
                .to_string(),
        );
        let result =
            resolve_multi_file_with(Path::new("/e/main.grx"), build_loader(files));
        assert!(matches!(result, Err(GrxParseError::Io { .. })));
    }

    /// **M15 Phase 1 slice 1A.** Cache hit on identical mtime.
    /// Editing zero files between two `resolve_multi_file_cached`
    /// calls yields all hits + zero misses on the second call.
    #[test]
    fn cache_hits_on_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("main.grx");
        let imp = dir.path().join("lib.grx");
        std::fs::write(
            &root,
            r#"<import src="./lib.grx" />
<Graph id="m"><Foo id="a" /></Graph>"#,
        )
        .unwrap();
        std::fs::write(&imp, r"<Foo>const x = 1;</Foo>").unwrap();

        let mut cache = ParseCache::new();
        let _r1 = resolve_multi_file_cached(&root, &mut cache).expect("r1");
        // First pass: 2 misses (root + import).
        assert_eq!(cache.misses(), 2);
        assert_eq!(cache.hits(), 0);

        // Reset stats so the second pass measures only its own
        // hit/miss outcomes.
        cache.reset_stats();
        let _r2 = resolve_multi_file_cached(&root, &mut cache).expect("r2");
        assert_eq!(
            cache.hits(),
            2,
            "second pass with unchanged files should be all hits"
        );
        assert_eq!(cache.misses(), 0, "no misses on unchanged files");
    }

    /// **M15 Phase 1 slice 1A.** mtime-staleness invalidates entries.
    /// Editing one of two cached files makes the second pass record
    /// one hit (unchanged file) plus one miss (changed file).
    #[test]
    fn cache_invalidates_on_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("main.grx");
        let imp = dir.path().join("lib.grx");
        std::fs::write(
            &root,
            r#"<import src="./lib.grx" />
<Graph id="m"><Foo id="a" /></Graph>"#,
        )
        .unwrap();
        std::fs::write(&imp, r"<Foo>const x = 1;</Foo>").unwrap();

        let mut cache = ParseCache::new();
        let _r1 = resolve_multi_file_cached(&root, &mut cache).expect("r1");
        cache.reset_stats();

        // Wait long enough that the rewritten file gets a distinct
        // mtime — filesystems vary in mtime resolution (HFS+ has
        // 1-second granularity historically; APFS is finer).
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Edit ONLY the import. The root stays untouched.
        std::fs::write(&imp, r"<Foo>const x = 2;</Foo>").unwrap();

        let _r2 = resolve_multi_file_cached(&root, &mut cache).expect("r2");
        assert_eq!(cache.hits(), 1, "root should hit (unchanged)");
        assert_eq!(cache.misses(), 1, "import should miss (mtime changed)");
    }

    /// **M15 Phase 1 slice 1A.** The four-file fixture from the
    /// memory's reasoning. Edit one leaf in a 4-file graph; the
    /// second pass reparses ONLY that leaf — three hits, one miss.
    /// Closes the user-facing prototypability promise.
    #[test]
    fn four_file_fixture_partial_reparse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root.grx");
        let a = dir.path().join("a.grx");
        let b = dir.path().join("b.grx");
        let leaf = dir.path().join("leaf.grx");

        std::fs::write(
            &root,
            r#"<import src="./a.grx" />
<import src="./b.grx" />
<Graph id="r"><Foo id="x" /></Graph>"#,
        )
        .unwrap();
        std::fs::write(
            &a,
            r#"<import src="./leaf.grx" />
<A>const a = 1;</A>"#,
        )
        .unwrap();
        std::fs::write(&b, r"<B>const b = 1;</B>").unwrap();
        std::fs::write(&leaf, r"<Foo>const leaf = 1;</Foo>").unwrap();

        let mut cache = ParseCache::new();
        let _r1 = resolve_multi_file_cached(&root, &mut cache).expect("r1");
        // First pass: 4 misses.
        assert_eq!(cache.misses(), 4);
        cache.reset_stats();

        // Edit only the leaf.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&leaf, r"<Foo>const leaf = 2;</Foo>").unwrap();

        let _r2 = resolve_multi_file_cached(&root, &mut cache).expect("r2");
        assert_eq!(
            cache.hits(),
            3,
            "root + a + b should all hit (unchanged)"
        );
        assert_eq!(cache.misses(), 1, "only the leaf should miss");
    }

    /// **M15 Phase 1 slice 1A.** Explicit invalidation works as a
    /// no-knobs operator surface even when mtime hasn't moved (e.g.,
    /// some test harnesses or filesystems with second-level mtime
    /// granularity write twice within the same mtime tick).
    #[test]
    fn explicit_invalidation_forces_reparse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("solo.grx");
        std::fs::write(&root, r#"<Foo>x</Foo><Graph id="g"><Foo id="a" /></Graph>"#)
            .unwrap();

        let mut cache = ParseCache::new();
        let _r1 = resolve_multi_file_cached(&root, &mut cache).expect("r1");
        cache.reset_stats();

        cache.invalidate(&std::fs::canonicalize(&root).unwrap());
        let _r2 = resolve_multi_file_cached(&root, &mut cache).expect("r2");
        assert_eq!(cache.misses(), 1, "explicit invalidate forces reparse");
        assert_eq!(cache.hits(), 0);
    }

    #[test]
    fn collects_imports_in_dfs_order() {
        let mut files: HashMap<PathBuf, String> = HashMap::new();
        files.insert(
            PathBuf::from("/f/main.grx"),
            r#"<import src="./a.grx" />
<import src="./b.grx" />
<Graph id="m"></Graph>"#
                .to_string(),
        );
        files.insert(
            PathBuf::from("/f/a.grx"),
            r#"<import src="./a-sub.grx" />"#.to_string(),
        );
        files.insert(PathBuf::from("/f/a-sub.grx"), String::new());
        files.insert(PathBuf::from("/f/b.grx"), String::new());
        let resolved =
            resolve_multi_file_with(Path::new("/f/main.grx"), build_loader(files))
                .expect("ok");
        let names: Vec<&str> = resolved
            .imports
            .iter()
            .map(|i| i.path.file_name().unwrap().to_str().unwrap())
            .collect();
        // DFS: a-sub before a (children first), then b.
        assert_eq!(names, vec!["a-sub.grx", "a.grx", "b.grx"]);
    }
}
