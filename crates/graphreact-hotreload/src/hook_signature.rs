//! **M15 Phase 3** — per-language hook-AST diffing.
//!
//! Extracts a [`HookSignature`] from a component body's source —
//! the ordered list of `(hook_kind, dep_arity)` tuples that the body
//! invokes at the top level of its render function. Two bodies with
//! identical signatures can be hot-swapped via
//! `Scheduler::replace_definition` while preserving every
//! `HookSlot` (i.e., `useState` values, committed effect deps, memo
//! caches survive the swap).
//!
//! # Why
//!
//! Phase 4's classifier ships in body-hash mode — any byte change in
//! a body source classifies as `BodyChangedIncompatible` (drain plus
//! remount, destroying `useState`). That makes editing a string
//! literal in a manifest force a full unmount-then-remount, which
//! defeats the hot-reload prototypability mandate. Phase 3 fixes this
//! by parsing both source versions with a per-language AST library,
//! extracting the hook-call sequence, and comparing structures —
//! identical hook structure plus differing source = Compatible
//! (preserve state).
//!
//! # Conservatism
//!
//! Hot-reload is a substrate-level safety boundary. Wrong
//! classifications either:
//!
//! - Promote an incompatible change to Compatible → corrupted state +
//!   undefined behavior.
//! - Demote a compatible change to Incompatible → unnecessary state
//!   loss (the user's `useState` value gets reset).
//!
//! We err on the side of demote-rather-than-promote. Anything we
//! can't statically verify (dynamic dispatch through aliased
//! references, deps arrays computed at runtime, hook calls inside
//! conditional branches) classifies as
//! [`HookSignatureError::DynamicDispatch`] and the caller falls back
//! to the conservative shape.
//!
//! # Architecture
//!
//! - JS bodies → `swc_ecma_parser` produces a `Script`/`Module` AST
//!   → `swc_ecma_visit::Visit` walks the tree → call expressions
//!   matching one of 22 hook names emit `HookCall` entries.
//! - Python bodies → `rustpython_parser::parse_program` produces a
//!   module AST → an explicit walker emits `HookCall` entries.
//!
//! Both bodies are real source — JS bodies are loose statements that
//! the runtime wraps in an IIFE before eval; Python bodies are
//! module-level `def body(ctx):` declarations whose function body
//! contains the hook calls. The JS extractor wraps the input in
//! `(function(){ ... })` before parsing so loose `return` statements
//! parse cleanly; the Python extractor walks the `body` function's
//! statement list directly.

use graphreact_grx_parser::ast::ComponentLang;
use thiserror::Error;

/// Ordered fingerprint of a component body's hook-call sequence.
/// Compatible-with comparison is the load-bearing operation —
/// `replace_definition` is only safe across signature-preserving
/// edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSignature {
    pub hooks: Vec<HookCall>,
}

/// One entry in a [`HookSignature`]. `dep_arity` is the literal
/// length of the deps array if the hook accepts one and the array
/// is statically inspectable; `None` otherwise (hook has no deps,
/// or the deps argument was dynamic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookCall {
    pub kind: HookKind,
    pub dep_arity: Option<usize>,
}

/// All 22 hooks the runtime exposes — JS plus Python share the same
/// kinds (Python uses `snake_case` aliases — `use_state` vs
/// `useState`; we normalize both to the same enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookKind {
    UseState,
    UseEffect,
    UseEffectAsync,
    UseMessage,
    UseMemo,
    UseCallback,
    UseReducer,
    UseNeighbors,
    UseTopology,
    UseAcceptEdge,
    UseEdgeAcceptHandler,
    UseGraphQuery,
    UseSpawnChild,
    UseUnmountSelf,
    UseCompose,
    UseSource,
    UseMonitor,
    UseDemonitor,
    UseLink,
    UseUnlink,
    UseGroupJoin,
    UseGroupLeave,
}

impl HookKind {
    /// Map a JS-style `useFooBar` identifier to a `HookKind`.
    fn from_js_name(name: &str) -> Option<Self> {
        Some(match name {
            "useState" => Self::UseState,
            "useEffect" => Self::UseEffect,
            "useEffectAsync" => Self::UseEffectAsync,
            "useMessage" => Self::UseMessage,
            "useMemo" => Self::UseMemo,
            "useCallback" => Self::UseCallback,
            "useReducer" => Self::UseReducer,
            "useNeighbors" => Self::UseNeighbors,
            "useTopology" => Self::UseTopology,
            "useAcceptEdge" => Self::UseAcceptEdge,
            "useEdgeAcceptHandler" => Self::UseEdgeAcceptHandler,
            "useGraphQuery" => Self::UseGraphQuery,
            "useSpawnChild" => Self::UseSpawnChild,
            "useUnmountSelf" => Self::UseUnmountSelf,
            "useCompose" => Self::UseCompose,
            "useSource" => Self::UseSource,
            "useMonitor" => Self::UseMonitor,
            "useDemonitor" => Self::UseDemonitor,
            "useLink" => Self::UseLink,
            "useUnlink" => Self::UseUnlink,
            "useGroupJoin" => Self::UseGroupJoin,
            "useGroupLeave" => Self::UseGroupLeave,
            _ => return None,
        })
    }

    /// Map a Python-style `use_foo_bar` identifier to a `HookKind`.
    /// Python's snake_case convention is the canonical Python author
    /// surface — `pyruntime` registers callbacks under both
    /// `host_state` plus `use_state`-style stubs.
    fn from_python_name(name: &str) -> Option<Self> {
        Some(match name {
            "use_state" => Self::UseState,
            "use_effect" => Self::UseEffect,
            "use_effect_async" => Self::UseEffectAsync,
            "use_message" => Self::UseMessage,
            "use_memo" => Self::UseMemo,
            "use_callback" => Self::UseCallback,
            "use_reducer" => Self::UseReducer,
            "use_neighbors" => Self::UseNeighbors,
            "use_topology" => Self::UseTopology,
            "use_accept_edge" => Self::UseAcceptEdge,
            "use_edge_accept_handler" => Self::UseEdgeAcceptHandler,
            "use_graph_query" => Self::UseGraphQuery,
            "use_spawn_child" => Self::UseSpawnChild,
            "use_unmount_self" => Self::UseUnmountSelf,
            "use_compose" => Self::UseCompose,
            "use_source" => Self::UseSource,
            "use_monitor" => Self::UseMonitor,
            "use_demonitor" => Self::UseDemonitor,
            "use_link" => Self::UseLink,
            "use_unlink" => Self::UseUnlink,
            "use_group_join" => Self::UseGroupJoin,
            "use_group_leave" => Self::UseGroupLeave,
            _ => return None,
        })
    }

    /// Index of the deps argument for hooks that accept one.
    /// Used to extract `dep_arity` when the deps argument is a
    /// statically-knowable array literal.
    fn deps_arg_index(self) -> Option<usize> {
        match self {
            // (fn, deps)
            Self::UseEffect | Self::UseEffectAsync | Self::UseMemo | Self::UseCallback => Some(1),
            // No deps argument — these hooks either take none or
            // their argument shape is something other than a deps
            // array (e.g., `useState(initial)`, `useMessage(type, fn)`).
            _ => None,
        }
    }
}

/// Errors that prevent a clean signature extraction. Callers
/// (typically the diff classifier) treat both variants as "defer
/// the reload, wait for the next event" rather than guessing —
/// per the M15 plan's footgun-mitigation rule for transient
/// mid-edit state.
#[derive(Debug, Clone, Error)]
pub enum HookSignatureError {
    /// The body source didn't parse. Half-saved file, transient
    /// typo, or a syntax we don't support yet.
    #[error("body source failed to parse: {0}")]
    ParseFailed(String),
    /// We saw what looks like a hook call through dynamic dispatch
    /// (alias, member access, `eval`, etc.). Can't statically
    /// verify the call shape — defer the reload rather than
    /// guess.
    #[error("dynamic dispatch detected: {0}")]
    DynamicDispatch(String),
    /// The body uses a language we don't have a parser for.
    #[error("unsupported component lang: {0:?}")]
    UnsupportedLang(ComponentLang),
}

impl HookSignature {
    /// Two signatures are compatible iff they're the same length and
    /// every position has the same `HookKind` plus the same
    /// `dep_arity`. This is the load-bearing equality for
    /// hot-reload — if it returns `true`, swapping the body via
    /// `Scheduler::replace_definition` preserves every `HookSlot`
    /// in the existing `NodeInstance`.
    #[must_use]
    pub fn compatible_with(&self, other: &Self) -> bool {
        if self.hooks.len() != other.hooks.len() {
            return false;
        }
        self.hooks
            .iter()
            .zip(other.hooks.iter())
            .all(|(a, b)| a == b)
    }
}

/// Extract a [`HookSignature`] from a body source. Dispatches by
/// language. Returns the typed error variants so callers can
/// distinguish "transient mid-edit state" (defer) from "we don't
/// support this language yet" (fall through to body-hash mode).
pub fn extract(source: &str, lang: ComponentLang) -> Result<HookSignature, HookSignatureError> {
    match lang {
        ComponentLang::Js => extract_js(source),
        ComponentLang::Python => extract_python(source),
        // M17 Go bodies are pre-compiled via `graphreact-codegen-go`
        // + `go build -buildmode=c-shared`; the daemon never sees the
        // raw `.go` source as a string the way it sees `.js` / `.py`.
        // The hot-reload hook-signature extractor falls through to
        // `UnsupportedLang` for `Go`, which routes the caller to
        // body-hash mode per M15's footgun-mitigation rule.
        // Real Go hot-reload arrives no earlier than M17 phase 3g.
        ComponentLang::Go => Err(HookSignatureError::UnsupportedLang(ComponentLang::Go)),
    }
}

// ---------------------------------------------------------------------
// JS extractor — swc-backed
// ---------------------------------------------------------------------

/// Parse `source` as a JS body, walk the AST, emit hook calls in
/// source order. The body is wrapped in a function expression
/// before parsing so top-level `return` statements (which the
/// runtime allows because it itself wraps the body in an IIFE on
/// eval) parse cleanly.
pub fn extract_js(source: &str) -> Result<HookSignature, HookSignatureError> {
    use swc_common::{sync::Lrc, FileName, SourceMap};
    use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};
    use swc_ecma_visit::Visit;

    // Wrap so loose `return` statements + statement-level expressions
    // parse as the function body that the runtime would evaluate.
    let wrapped = format!("(function(__rawCtx) {{ {source} }})");

    let cm: Lrc<SourceMap> = Lrc::new(SourceMap::default());
    let fm = cm.new_source_file(Lrc::new(FileName::Custom("body.js".into())), wrapped);
    let lexer = Lexer::new(
        Syntax::Es(Default::default()),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let module = parser
        .parse_script()
        .map_err(|e| HookSignatureError::ParseFailed(format!("{e:?}")))?;

    let mut visitor = JsHookVisitor::new();
    visitor.visit_script(&module);
    if let Some(reason) = visitor.dynamic_reason {
        return Err(HookSignatureError::DynamicDispatch(reason));
    }
    Ok(HookSignature {
        hooks: visitor.hooks,
    })
}

/// AST visitor — captures hook calls in source order. Uses
/// `swc_ecma_visit::Visit` so we get default-traversal for free
/// (visit children unless we override).
struct JsHookVisitor {
    hooks: Vec<HookCall>,
    dynamic_reason: Option<String>,
}

impl JsHookVisitor {
    fn new() -> Self {
        Self {
            hooks: Vec::new(),
            dynamic_reason: None,
        }
    }
}

impl swc_ecma_visit::Visit for JsHookVisitor {
    fn visit_call_expr(&mut self, call: &swc_ecma_ast::CallExpr) {
        // Only direct identifier callees count as hook calls. A
        // member-expression callee (`obj.useState(...)`) or any other
        // shape is either: (a) not a hook call at all (just a
        // method on some user object), or (b) dynamic dispatch we
        // refuse to verify. Both fall through without flagging
        // dynamic-reason — they can't be misidentified as hooks
        // because they don't match the hook-name pattern.
        let swc_ecma_ast::Callee::Expr(callee_expr) = &call.callee else {
            // Super / Import callees aren't hooks.
            swc_ecma_visit::VisitWith::visit_children_with(call, self);
            return;
        };
        let swc_ecma_ast::Expr::Ident(ident) = &**callee_expr else {
            // Could be a member expression like `React.useState(...)`
            // or a parenthesised expression. If the identifier path
            // looks hook-shaped, flag dynamic dispatch defensively.
            if expr_looks_hook_shaped(callee_expr) {
                self.dynamic_reason = Some(
                    "non-identifier callee with hook-shaped name; can't statically verify".into(),
                );
            }
            swc_ecma_visit::VisitWith::visit_children_with(call, self);
            return;
        };
        let name = ident.sym.as_ref();
        let Some(kind) = HookKind::from_js_name(name) else {
            swc_ecma_visit::VisitWith::visit_children_with(call, self);
            return;
        };

        // Hook call detected. Extract dep_arity if applicable.
        let dep_arity = match kind.deps_arg_index() {
            Some(idx) => extract_js_array_arity(call.args.get(idx)),
            None => None,
        };
        self.hooks.push(HookCall { kind, dep_arity });
        swc_ecma_visit::VisitWith::visit_children_with(call, self);
    }
}

/// Best-effort check: does this expression LOOK like it's referring
/// to a hook by name even though it's not a bare identifier? E.g.,
/// `React.useState`, `(useState)`, or an aliased ref. Used to flag
/// dynamic dispatch defensively without misclassifying genuinely
/// non-hook calls.
fn expr_looks_hook_shaped(expr: &swc_ecma_ast::Expr) -> bool {
    match expr {
        swc_ecma_ast::Expr::Member(member) => {
            if let swc_ecma_ast::MemberProp::Ident(prop) = &member.prop {
                HookKind::from_js_name(prop.sym.as_ref()).is_some()
            } else {
                false
            }
        }
        swc_ecma_ast::Expr::Paren(paren) => expr_looks_hook_shaped(&paren.expr),
        _ => false,
    }
}

/// If `arg` is `Some(ArrayLit { elems })` returns the length;
/// otherwise `None`. A non-array argument (variable, function call,
/// undefined) returns `None` — we treat dynamic deps as opaque
/// arity, which makes `compatible_with` reject swaps that change
/// from literal to dynamic OR vice versa.
fn extract_js_array_arity(arg: Option<&swc_ecma_ast::ExprOrSpread>) -> Option<usize> {
    let arg = arg?;
    if arg.spread.is_some() {
        return None;
    }
    let swc_ecma_ast::Expr::Array(arr) = &*arg.expr else {
        return None;
    };
    Some(arr.elems.len())
}

// ---------------------------------------------------------------------
// Python extractor — rustpython-parser-backed
// ---------------------------------------------------------------------

/// Parse `source` as a Python module, find the `body(ctx)` (or
/// `init(ctx)` legacy) function definition, walk its statement
/// list, emit hook calls in source order. The Python author
/// convention is module-level `def body(ctx): ...`; the runtime
/// resolves this function via `gr_cpython_function_resolve("body")`
/// and calls it on every render.
pub fn extract_python(source: &str) -> Result<HookSignature, HookSignatureError> {
    use rustpython_parser::Parse;
    let module = rustpython_parser::ast::Suite::parse(source, "<body>")
        .map_err(|e| HookSignatureError::ParseFailed(format!("{e:?}")))?;

    // Find the `body` (preferred) or `init` (legacy) function def.
    let target = module.iter().find_map(|stmt| match stmt {
        rustpython_parser::ast::Stmt::FunctionDef(f) if f.name.as_str() == "body" => Some(&f.body),
        rustpython_parser::ast::Stmt::AsyncFunctionDef(f) if f.name.as_str() == "body" => {
            Some(&f.body)
        }
        _ => None,
    });
    let body_stmts = match target {
        Some(stmts) => stmts,
        None => {
            // Fall back to `init`.
            let fallback = module.iter().find_map(|stmt| match stmt {
                rustpython_parser::ast::Stmt::FunctionDef(f) if f.name.as_str() == "init" => {
                    Some(&f.body)
                }
                rustpython_parser::ast::Stmt::AsyncFunctionDef(f) if f.name.as_str() == "init" => {
                    Some(&f.body)
                }
                _ => None,
            });
            match fallback {
                Some(stmts) => stmts,
                None => {
                    return Err(HookSignatureError::ParseFailed(
                        "Python body source must define `def body(ctx):` or `def init(ctx):`"
                            .into(),
                    ))
                }
            }
        }
    };

    let mut visitor = PyHookVisitor::new();
    for stmt in body_stmts {
        visitor.visit_stmt(stmt);
    }
    if let Some(reason) = visitor.dynamic_reason {
        return Err(HookSignatureError::DynamicDispatch(reason));
    }
    Ok(HookSignature {
        hooks: visitor.hooks,
    })
}

/// Explicit walker — rustpython-parser doesn't expose a Visit
/// trait analogous to swc's, so we recurse manually. We only need
/// to descend through statement and expression nodes to find call
/// expressions; that's a finite + tractable subset.
struct PyHookVisitor {
    hooks: Vec<HookCall>,
    dynamic_reason: Option<String>,
}

impl PyHookVisitor {
    fn new() -> Self {
        Self {
            hooks: Vec::new(),
            dynamic_reason: None,
        }
    }

    fn visit_stmt(&mut self, stmt: &rustpython_parser::ast::Stmt) {
        use rustpython_parser::ast::Stmt;
        match stmt {
            Stmt::Expr(e) => self.visit_expr(&e.value),
            Stmt::Assign(a) => {
                for target in &a.targets {
                    self.visit_expr(target);
                }
                self.visit_expr(&a.value);
            }
            Stmt::AugAssign(a) => {
                self.visit_expr(&a.target);
                self.visit_expr(&a.value);
            }
            Stmt::AnnAssign(a) => {
                self.visit_expr(&a.target);
                if let Some(v) = &a.value {
                    self.visit_expr(v);
                }
            }
            Stmt::Return(r) => {
                if let Some(v) = &r.value {
                    self.visit_expr(v);
                }
            }
            Stmt::If(s) => {
                self.visit_expr(&s.test);
                for st in &s.body {
                    self.visit_stmt(st);
                }
                for st in &s.orelse {
                    self.visit_stmt(st);
                }
            }
            Stmt::While(s) => {
                self.visit_expr(&s.test);
                for st in &s.body {
                    self.visit_stmt(st);
                }
                for st in &s.orelse {
                    self.visit_stmt(st);
                }
            }
            Stmt::For(s) => {
                self.visit_expr(&s.target);
                self.visit_expr(&s.iter);
                for st in &s.body {
                    self.visit_stmt(st);
                }
                for st in &s.orelse {
                    self.visit_stmt(st);
                }
            }
            Stmt::With(s) => {
                for item in &s.items {
                    self.visit_expr(&item.context_expr);
                }
                for st in &s.body {
                    self.visit_stmt(st);
                }
            }
            Stmt::Try(s) => {
                for st in &s.body {
                    self.visit_stmt(st);
                }
                for handler in &s.handlers {
                    let rustpython_parser::ast::ExceptHandler::ExceptHandler(h) = handler;
                    for st in &h.body {
                        self.visit_stmt(st);
                    }
                }
                for st in &s.orelse {
                    self.visit_stmt(st);
                }
                for st in &s.finalbody {
                    self.visit_stmt(st);
                }
            }
            // Everything else (FunctionDef / AsyncFunctionDef /
            // ClassDef / Pass / Break / Continue / Raise / Import /
            // Global / Nonlocal / Delete / AsyncFor / AsyncWith /
            // Match) — skip. Hooks called inside nested function or
            // class bodies don't contribute top-level hook calls
            // (React's "no conditional hook calls" rule); the rest
            // are control-flow or imports with no hook-call
            // surface.
            _ => {}
        }
    }

    fn visit_expr(&mut self, expr: &rustpython_parser::ast::Expr) {
        use rustpython_parser::ast::Expr;
        match expr {
            Expr::Call(c) => {
                // Hook detection.
                if let Expr::Name(n) = &*c.func {
                    let name = n.id.as_str();
                    if let Some(kind) = HookKind::from_python_name(name) {
                        let dep_arity = match kind.deps_arg_index() {
                            Some(idx) => extract_py_list_arity(c.args.get(idx)),
                            None => None,
                        };
                        self.hooks.push(HookCall { kind, dep_arity });
                    }
                } else if let Expr::Attribute(attr) = &*c.func {
                    // Member call — flag if the attribute name is hook-shaped.
                    if HookKind::from_python_name(attr.attr.as_str()).is_some() {
                        self.dynamic_reason = Some(
                            "Python member-expression callee with hook-shaped name; \
                             can't statically verify"
                                .into(),
                        );
                    }
                }
                // Recurse into args.
                for arg in &c.args {
                    self.visit_expr(arg);
                }
                for kw in &c.keywords {
                    self.visit_expr(&kw.value);
                }
            }
            Expr::BinOp(b) => {
                self.visit_expr(&b.left);
                self.visit_expr(&b.right);
            }
            Expr::UnaryOp(u) => self.visit_expr(&u.operand),
            Expr::IfExp(i) => {
                self.visit_expr(&i.test);
                self.visit_expr(&i.body);
                self.visit_expr(&i.orelse);
            }
            Expr::Tuple(t) => {
                for e in &t.elts {
                    self.visit_expr(e);
                }
            }
            Expr::List(l) => {
                for e in &l.elts {
                    self.visit_expr(e);
                }
            }
            Expr::Dict(d) => {
                for k in d.keys.iter().flatten() {
                    self.visit_expr(k);
                }
                for v in &d.values {
                    self.visit_expr(v);
                }
            }
            _ => {}
        }
    }
}

/// If `arg` is a list literal, return its length. Other argument
/// shapes (variable refs, comprehensions, function calls) return
/// `None` — opaque arity.
fn extract_py_list_arity(arg: Option<&rustpython_parser::ast::Expr>) -> Option<usize> {
    let arg = arg?;
    let rustpython_parser::ast::Expr::List(list) = arg else {
        return None;
    };
    Some(list.elts.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- JS extractor ----

    #[test]
    fn js_extracts_use_state_with_no_deps_arg() {
        let src = "const [count, setCount] = useState(0); return { accepts: [], emits: [] };";
        let sig = extract_js(src).expect("extract");
        assert_eq!(sig.hooks.len(), 1);
        assert_eq!(sig.hooks[0].kind, HookKind::UseState);
        assert_eq!(sig.hooks[0].dep_arity, None);
    }

    #[test]
    fn js_extracts_use_effect_with_deps_arity() {
        let src = "useState(0); useEffect(function() {}, [a, b, c]); return null;";
        let sig = extract_js(src).expect("extract");
        assert_eq!(sig.hooks.len(), 2);
        assert_eq!(sig.hooks[0].kind, HookKind::UseState);
        assert_eq!(sig.hooks[1].kind, HookKind::UseEffect);
        assert_eq!(sig.hooks[1].dep_arity, Some(3));
    }

    #[test]
    fn js_string_literal_change_preserves_signature() {
        // The precision win — body source differs by a string
        // literal only; the hook signatures must be byte-identical.
        let v1 = "useState(0); useEffect(function() { host_observe('v1'); }, [count]); return { accepts: ['v1'], emits: [] };";
        let v2 = "useState(0); useEffect(function() { host_observe('v2'); }, [count]); return { accepts: ['v2'], emits: [] };";
        let sig1 = extract_js(v1).expect("v1");
        let sig2 = extract_js(v2).expect("v2");
        assert!(
            sig1.compatible_with(&sig2),
            "string-literal-only change must preserve compatibility: \nv1={sig1:?}\nv2={sig2:?}",
        );
    }

    #[test]
    fn js_added_use_state_breaks_compatibility() {
        let v1 = "useState(0); return null;";
        let v2 = "useState(0); useState(1); return null;";
        let sig1 = extract_js(v1).expect("v1");
        let sig2 = extract_js(v2).expect("v2");
        assert!(!sig1.compatible_with(&sig2), "added useState breaks");
    }

    #[test]
    fn js_changed_dep_arity_breaks_compatibility() {
        let v1 = "useEffect(function() {}, []); return null;";
        let v2 = "useEffect(function() {}, [x]); return null;";
        let sig1 = extract_js(v1).expect("v1");
        let sig2 = extract_js(v2).expect("v2");
        assert!(!sig1.compatible_with(&sig2), "deps arity change breaks");
    }

    #[test]
    fn js_dynamic_dep_arity_makes_signatures_distinct_from_literal() {
        let v1 = "useEffect(function() {}, []); return null;";
        let v2 = "useEffect(function() {}, computeDeps()); return null;";
        let sig1 = extract_js(v1).expect("v1");
        let sig2 = extract_js(v2).expect("v2");
        // dep_arity = Some(0) vs None — incompatible.
        assert!(!sig1.compatible_with(&sig2));
    }

    #[test]
    fn js_dynamic_dispatch_via_member_call_returns_dynamic_dispatch_error() {
        // `React.useState(0)` looks hook-shaped through a member
        // expression — flag as DynamicDispatch.
        let src = "React.useState(0); return null;";
        let err = extract_js(src).expect_err("must flag dynamic");
        assert!(matches!(err, HookSignatureError::DynamicDispatch(_)));
    }

    #[test]
    fn js_parse_failure_returns_parse_failed() {
        let src = "const [count, setCount] = useState(0"; // unbalanced paren
        let err = extract_js(src).expect_err("must error");
        assert!(matches!(err, HookSignatureError::ParseFailed(_)));
    }

    #[test]
    fn js_non_hook_call_with_hook_shaped_name_inside_method_does_not_count() {
        // Calling `.useState(...)` on some random user object
        // shouldn't be classified as a hook call.
        let src = "obj.useState(0); return null;";
        let err = extract_js(src).expect_err("must flag dynamic");
        // We flag dynamic dispatch defensively rather than counting
        // this as zero hooks.
        assert!(matches!(err, HookSignatureError::DynamicDispatch(_)));
    }

    // ---- Python extractor ----

    #[test]
    fn py_extracts_use_state_with_no_deps_arg() {
        let src = "def body(ctx):\n    count, set_count = use_state(0)\n    return {'accepts': [], 'emits': []}\n";
        let sig = extract_python(src).expect("extract");
        assert_eq!(sig.hooks.len(), 1);
        assert_eq!(sig.hooks[0].kind, HookKind::UseState);
        assert_eq!(sig.hooks[0].dep_arity, None);
    }

    #[test]
    fn py_extracts_use_effect_with_deps_arity() {
        let src = "def body(ctx):\n    use_state(0)\n    use_effect(lambda: None, [a, b])\n    return None\n";
        let sig = extract_python(src).expect("extract");
        assert_eq!(sig.hooks.len(), 2);
        assert_eq!(sig.hooks[1].kind, HookKind::UseEffect);
        assert_eq!(sig.hooks[1].dep_arity, Some(2));
    }

    #[test]
    fn py_string_literal_change_preserves_signature() {
        let v1 = "def body(ctx):\n    use_state(0)\n    return {'accepts': ['v1']}\n";
        let v2 = "def body(ctx):\n    use_state(0)\n    return {'accepts': ['v2']}\n";
        let sig1 = extract_python(v1).expect("v1");
        let sig2 = extract_python(v2).expect("v2");
        assert!(sig1.compatible_with(&sig2));
    }

    #[test]
    fn py_falls_back_to_init_for_legacy_fixtures() {
        let src = "def init(ctx):\n    use_state(0)\n    return {}\n";
        let sig = extract_python(src).expect("extract");
        assert_eq!(sig.hooks.len(), 1);
    }

    #[test]
    fn py_parse_failure_returns_parse_failed() {
        let src = "def body(ctx):\n    use_state(0";
        let err = extract_python(src).expect_err("must error");
        assert!(matches!(err, HookSignatureError::ParseFailed(_)));
    }

    #[test]
    fn py_missing_body_or_init_returns_parse_failed() {
        let src = "def helper():\n    return 1\n";
        let err = extract_python(src).expect_err("must error");
        assert!(matches!(err, HookSignatureError::ParseFailed(_)));
    }

    #[test]
    fn py_member_call_with_hook_shaped_name_flags_dynamic() {
        let src = "def body(ctx):\n    self.use_state(0)\n    return None\n";
        let err = extract_python(src).expect_err("must flag");
        assert!(matches!(err, HookSignatureError::DynamicDispatch(_)));
    }

    // ---- Cross-language equality ----

    #[test]
    fn signatures_with_same_hooks_compare_equal_across_languages() {
        // Lang-mismatch is a separate concern (the diff classifier
        // promotes lang change to LangChanged → Incompatible
        // unconditionally). Within a single language, identical
        // hook orderings produce identical signatures.
        let js =
            extract_js("useState(0); useEffect(function() {}, [x]); return null;").expect("js");
        let py = extract_python(
            "def body(ctx):\n    use_state(0)\n    use_effect(lambda: None, [x])\n    return None\n",
        )
        .expect("py");
        assert!(js.compatible_with(&py));
    }
}
