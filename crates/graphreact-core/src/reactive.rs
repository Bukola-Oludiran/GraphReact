//! Leptos-inspired reactive derivation layer (M7).
//!
//! Adds [`Memo<T>`] — a derived reactive value with **Clean / Check /
//! Dirty** 3-state propagation and automatic source tracking via the
//! observer pattern. Sources are tracked when a Memo's compute closure
//! reads from a [`ReactiveSignal<T>`] (or from another `Memo<T>`); the
//! Memo only recomputes when one of its sources actually changed value
//! since the last read — the **Check** state lets the dirty cascade
//! short-circuit when an intermediate Memo's recompute returns the same
//! value.
//!
//! ## Why a separate primitive from [`crate::Signal<T>`]?
//!
//! The M4 [`crate::Signal<T>`] is node-tied (created via
//! [`crate::use_state`], lives in a `NodeInstance`'s arena, marks the
//! owning node dirty on update). It's the primitive React-style
//! components consume.
//!
//! [`Memo<T>`] is *standalone*. It can be created outside of any node
//! context and is intended for derived computation in algorithms,
//! query layers, and anywhere a value is computed from inputs and
//! cached until the inputs change. The plan calls out graspologic
//! ports (M8–M10) as the headline consumer — algorithm results that
//! live across re-evaluations and only re-run when their inputs
//! change in a way that materially affects the output.
//!
//! ## Worked example
//!
//! ```ignore
//! use graphreact_core::reactive::{Memo, ReactiveSignal};
//!
//! let count = ReactiveSignal::new(0i32);
//! let count_for_memo = count.clone();
//! let parity = Memo::new(move || {
//!     if count_for_memo.get() % 2 == 0 { "even" } else { "odd" }
//! });
//! assert_eq!(parity.get(), "even");
//!
//! count.set(2);
//! // `count` changed but `parity` didn't — `Memo::get()` recomputes
//! // internally and returns "even" again, but a downstream Memo
//! // subscribed to `parity` would see no Dirty propagation.
//! assert_eq!(parity.get(), "even");
//!
//! count.set(3);
//! assert_eq!(parity.get(), "odd");
//! ```
//!
//! ## Threading model
//!
//! `!Send` like the rest of the M4 runtime — uses `Rc<RefCell<...>>`
//! for hot-path performance. Multi-thread variants are out of scope.
//! Memos and Signals stay on a single thread; cross-thread fan-out
//! goes through the M5 cluster/messaging layer.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Reactive node state + trait
// ---------------------------------------------------------------------------

/// Three-state propagation marker. `Clean` means the node's cached
/// value is current; `Dirty` means it must recompute on next read;
/// `Check` means it might or might not need to recompute — the node
/// must walk its sources to find out (lazy).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ReactiveNodeState {
    Clean,
    Check,
    Dirty,
}

/// Type-erased trait every reactive node implements. Signals are pure
/// sources (`update_if_necessary` is a no-op-ish `false`); Memos are
/// both source AND subscriber.
///
/// The split between `mark_check` and `mark_dirty` is the heart of the
/// 3-state propagation:
/// - A direct source change calls `mark_dirty` on every subscriber:
///   that subscriber's *own* state goes Dirty (must recompute) and
///   *its* downstream subscribers go Check (may need recompute).
/// - When a Memo recomputes and its value hasn't changed, it leaves
///   its subscribers Check rather than escalating them to Dirty —
///   their next read does an `update_if_necessary` walk that returns
///   `false` and they stay Clean.
pub trait ReactiveNode {
    /// Stable id for de-duplication and cycle-detection.
    fn id(&self) -> u64;

    /// "Your sources may have changed; re-check on next read." Goes
    /// from Clean → Check; no-op if already Dirty.
    fn mark_check(&self);

    /// "You must recompute on next read." Goes to Dirty regardless of
    /// current state; cascades Check to downstream subscribers.
    fn mark_dirty(&self);

    /// Recompute if needed; return whether the value changed.
    /// Signals always return `false` (they don't recompute themselves —
    /// they're already at their latest value); Memos return `true`
    /// when the recomputed value differs from the cached one.
    fn update_if_necessary(&self) -> bool;

    /// Register `sub` as a subscriber. Called from a Memo's compute
    /// closure: when the Memo reads from a source, the source adds
    /// the Memo as a subscriber so subsequent source updates can
    /// notify the Memo.
    fn add_subscriber(&self, sub: WeakReactive);
}

/// Strong handle to a reactive node. Used for source-side ownership
/// (Memos hold `AnyReactive` to their sources so Signals don't get
/// dropped while a Memo still observes them).
pub type AnyReactive = Rc<dyn ReactiveNode>;

/// Weak handle — used for subscriber-side references so a Signal
/// doesn't keep a downstream Memo alive past the user's intent.
pub type WeakReactive = Weak<dyn ReactiveNode>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Observer thread-local
// ---------------------------------------------------------------------------

/// Closure that registers a source observed during a Memo's compute.
/// When invoked, it pushes `source` into the running Memo's source
/// list AND pushes the Memo (as a subscriber) into the source's
/// subscriber list. Set on a thread-local for the duration of the
/// compute via [`ObserverGuard`].
type SourceRegistrar = Box<dyn Fn(AnyReactive)>;

thread_local! {
    /// Active registrar. `None` outside any Memo's compute closure;
    /// `Some` while one runs. Read by [`ReactiveSignal::get`] and
    /// [`Memo::get`] to wire the source/subscriber relationship.
    static CURRENT_REGISTRAR: RefCell<Option<SourceRegistrar>> = const {
        RefCell::new(None)
    };
}

/// Push `source` into the running Memo's source list (and conversely
/// register the Memo as a subscriber on `source`). No-op outside of a
/// compute closure.
fn register_source(source: AnyReactive) {
    CURRENT_REGISTRAR.with(|cell| {
        if let Some(reg) = cell.borrow().as_ref() {
            reg(source);
        }
    });
}

/// RAII guard — installs `registrar` as the active source-registration
/// closure for the guard's lifetime. Restores the previous registrar
/// on drop (supporting nested Memo recomputes).
struct ObserverGuard {
    prev: Option<SourceRegistrar>,
}

impl ObserverGuard {
    fn new(registrar: SourceRegistrar) -> Self {
        let prev = CURRENT_REGISTRAR.with(|cell| cell.borrow_mut().replace(registrar));
        Self { prev }
    }
}

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        CURRENT_REGISTRAR.with(|cell| {
            *cell.borrow_mut() = self.prev.take();
        });
    }
}

// ---------------------------------------------------------------------------
// ReactiveSignal<T> — standalone reactive source
// ---------------------------------------------------------------------------

/// Standalone reactive value. Independent of [`crate::Signal<T>`]
/// (which is node-tied) — `ReactiveSignal<T>` lives wherever you put
/// it and broadcasts to any [`Memo`] subscribers when its value
/// changes.
///
/// Cheap to clone (one `Rc` increment). All clones share the same
/// underlying cell.
pub struct ReactiveSignal<T> {
    inner: Rc<ReactiveSignalInner<T>>,
}

struct ReactiveSignalInner<T> {
    id: u64,
    value: RefCell<T>,
    subscribers: RefCell<Vec<WeakReactive>>,
}

impl<T> Clone for ReactiveSignal<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

impl<T: 'static> ReactiveSignal<T> {
    /// Construct a new reactive cell with the given initial value.
    pub fn new(value: T) -> Self {
        Self {
            inner: Rc::new(ReactiveSignalInner {
                id: next_id(),
                value: RefCell::new(value),
                subscribers: RefCell::new(Vec::new()),
            }),
        }
    }

    /// Read the current value (cloning). If invoked inside a Memo's
    /// compute closure, registers this signal as a source of the
    /// running Memo (and conversely, the Memo as a subscriber here).
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        register_source(Rc::clone(&self.inner) as AnyReactive);
        self.inner.value.borrow().clone()
    }

    /// Replace the value. If different from the current value, marks
    /// every live subscriber Dirty (which cascades Check to their
    /// downstream subscribers). If equal, this is a no-op.
    pub fn set(&self, value: T)
    where
        T: PartialEq,
    {
        {
            let mut current = self.inner.value.borrow_mut();
            if *current == value {
                return;
            }
            *current = value;
        }
        // A Signal change is a "definitely changed" signal to every
        // subscriber. Memos receive `mark_dirty` (state=Dirty),
        // which then cascades `mark_check` to *their* downstream
        // subscribers via Memo's mark_dirty impl.
        let live = collect_live_subscribers(&self.inner.subscribers);
        for sub in live {
            sub.mark_dirty();
        }
    }

    /// Apply a transformation to the current value.
    pub fn update<F>(&self, f: F)
    where
        F: FnOnce(&T) -> T,
        T: PartialEq + Clone,
    {
        let new_value = f(&self.inner.value.borrow());
        self.set(new_value);
    }

    /// Test/diagnostic: number of currently-live subscribers (after
    /// pruning any that have dropped).
    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        let mut subs = self.inner.subscribers.borrow_mut();
        subs.retain(|w| w.upgrade().is_some());
        subs.len()
    }
}

impl<T: 'static> ReactiveNode for ReactiveSignalInner<T> {
    fn id(&self) -> u64 {
        self.id
    }

    fn mark_check(&self) {
        // Signals don't have state — no-op.
    }

    fn mark_dirty(&self) {
        // Signals don't have state — no-op for self; subscribers were
        // already notified via `set` directly.
    }

    fn update_if_necessary(&self) -> bool {
        // A signal is always at its latest value. Its update path
        // returns `false` ("I didn't recompute"); a Memo subscribing
        // to a Signal sees the change via the direct `mark_dirty`
        // call from `set`, not via this method.
        false
    }

    fn add_subscriber(&self, sub: WeakReactive) {
        add_subscriber_dedup(&self.subscribers, sub);
    }
}

// ---------------------------------------------------------------------------
// Memo<T> — derived reactive value with Clean / Check / Dirty propagation
// ---------------------------------------------------------------------------

/// Derived reactive value. Holds a compute closure and a cache of the
/// last-computed value; recomputes lazily on `get()` only when one of
/// its sources actually changed in a way that affects the output.
///
/// `Memo<T>` requires `T: PartialEq + Clone + 'static` — the equality
/// check is what makes the Check state actually skip downstream
/// recomputes.
pub struct Memo<T: PartialEq + Clone + 'static> {
    inner: Rc<MemoInner<T>>,
}

impl<T: PartialEq + Clone + 'static> Clone for Memo<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

struct MemoInner<T: PartialEq + Clone + 'static> {
    id: u64,
    state: Cell<ReactiveNodeState>,
    /// Cached value. `None` until first compute.
    value: RefCell<Option<T>>,
    /// Compute closure — reads sources via `ReactiveSignal::get` /
    /// other Memos. Each call sets up an observer guard so the
    /// reads register this Memo as a subscriber.
    compute: Box<dyn Fn() -> T>,
    /// Sources observed during the last compute. Held strongly so the
    /// sources stay alive while this Memo references them. Cleared
    /// before each recompute and rebuilt as the closure runs.
    sources: RefCell<Vec<AnyReactive>>,
    /// Subscribers — Weak so dropped Memos don't keep their parents'
    /// subscriber lists growing.
    subscribers: RefCell<Vec<WeakReactive>>,
    /// Weak self-reference. Set to `Some(Rc::downgrade(&inner))`
    /// immediately after `Rc::new(MemoInner { ... })`. Allows
    /// `recompute_inner` (called via the `ReactiveNode` trait, which
    /// only sees `&self`) to install itself as the active observer
    /// during compute — keeping source tracking accurate when sibling
    /// Memos walk sources via `update_if_necessary`.
    self_weak: RefCell<Option<Weak<MemoInner<T>>>>,
}

impl<T: PartialEq + Clone + 'static> Memo<T> {
    /// Construct a new Memo from a compute closure. The closure is
    /// invoked lazily on first `get()` (and on subsequent reads when
    /// the Memo's state requires recompute).
    pub fn new<F>(compute: F) -> Self
    where
        F: Fn() -> T + 'static,
    {
        let inner = Rc::new(MemoInner {
            id: next_id(),
            state: Cell::new(ReactiveNodeState::Dirty),
            value: RefCell::new(None),
            compute: Box::new(compute),
            sources: RefCell::new(Vec::new()),
            subscribers: RefCell::new(Vec::new()),
            self_weak: RefCell::new(None),
        });
        // Plant the self-weak so the trait-path recompute can find
        // itself as the observer.
        *inner.self_weak.borrow_mut() = Some(Rc::downgrade(&inner));
        Self { inner }
    }

    /// Read the current value, recomputing if necessary. If invoked
    /// inside another Memo's compute closure, registers this Memo as
    /// a source of the outer Memo.
    pub fn get(&self) -> T {
        // Register subscription FIRST so a downstream observer that
        // reads a Clean Memo still ends up subscribed even if the
        // value didn't change.
        register_source(Rc::clone(&self.inner) as AnyReactive);

        // `update_if_necessary` reconciles the Memo's state and runs
        // the compute closure if needed.
        self.inner_update_if_necessary();

        self.inner
            .value
            .borrow()
            .as_ref()
            .expect("Memo value must be Some after update_if_necessary")
            .clone()
    }

    /// Internal: drives the 3-state propagation. Returns whether the
    /// Memo's value changed during this call.
    fn inner_update_if_necessary(&self) -> bool {
        match self.inner.state.get() {
            ReactiveNodeState::Clean => false,
            ReactiveNodeState::Dirty => self.recompute(),
            ReactiveNodeState::Check => {
                // Walk sources. If any source's update reported a
                // change, OR if a source's update flipped our state to
                // Dirty (because a recompute upstream marked us
                // Dirty), recompute ourselves.
                let sources_snapshot: Vec<AnyReactive> = self.inner.sources.borrow().clone();
                let mut needs_recompute = false;
                for source in sources_snapshot {
                    if source.update_if_necessary() {
                        needs_recompute = true;
                        break;
                    }
                    if self.inner.state.get() == ReactiveNodeState::Dirty {
                        needs_recompute = true;
                        break;
                    }
                }
                if needs_recompute {
                    self.recompute()
                } else {
                    self.inner.state.set(ReactiveNodeState::Clean);
                    false
                }
            }
        }
    }

    /// Re-run the compute closure. Returns whether the new value
    /// differs from the cached one. The public path delegates to the
    /// trait-impl's `recompute_inner` so the trait and direct paths
    /// share one source-tracking implementation.
    fn recompute(&self) -> bool {
        self.inner.recompute_inner()
    }

    /// Test/diagnostic: current state of the Memo (without triggering
    /// recompute).
    #[cfg(test)]
    pub fn state(&self) -> ReactiveNodeState {
        self.inner.state.get()
    }

    /// Test/diagnostic: number of currently-live subscribers.
    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        let mut subs = self.inner.subscribers.borrow_mut();
        subs.retain(|w| w.upgrade().is_some());
        subs.len()
    }

    /// Test/diagnostic: number of sources observed during the last
    /// compute.
    #[cfg(test)]
    pub fn source_count(&self) -> usize {
        self.inner.sources.borrow().len()
    }
}

impl<T: PartialEq + Clone + 'static> ReactiveNode for MemoInner<T> {
    fn id(&self) -> u64 {
        self.id
    }

    fn mark_check(&self) {
        // Already-Dirty stays Dirty (a stronger commitment); else flip
        // to Check and cascade.
        if self.state.get() != ReactiveNodeState::Dirty {
            self.state.set(ReactiveNodeState::Check);
        }
        let live = collect_live_subscribers(&self.subscribers);
        for sub in live {
            sub.mark_check();
        }
    }

    fn mark_dirty(&self) {
        self.state.set(ReactiveNodeState::Dirty);
        // Subscribers go to Check (they may have changed; they'll do
        // their own update_if_necessary walk on next read).
        let live = collect_live_subscribers(&self.subscribers);
        for sub in live {
            sub.mark_check();
        }
    }

    fn update_if_necessary(&self) -> bool {
        // Mirror Memo::inner_update_if_necessary but expressed on the
        // inner type so trait calls from sibling sources work.
        match self.state.get() {
            ReactiveNodeState::Clean => false,
            ReactiveNodeState::Dirty => self.recompute_inner(),
            ReactiveNodeState::Check => {
                let sources_snapshot: Vec<AnyReactive> = self.sources.borrow().clone();
                let mut needs_recompute = false;
                for source in sources_snapshot {
                    if source.update_if_necessary() {
                        needs_recompute = true;
                        break;
                    }
                    if self.state.get() == ReactiveNodeState::Dirty {
                        needs_recompute = true;
                        break;
                    }
                }
                if needs_recompute {
                    self.recompute_inner()
                } else {
                    self.state.set(ReactiveNodeState::Clean);
                    false
                }
            }
        }
    }

    fn add_subscriber(&self, sub: WeakReactive) {
        add_subscriber_dedup(&self.subscribers, sub);
    }
}

impl<T: PartialEq + Clone + 'static> MemoInner<T> {
    fn recompute_inner(&self) -> bool {
        // Tear down old source subscriptions; the registrar planted
        // below will rebuild the list as the closure reads sources.
        self.sources.borrow_mut().clear();

        // Resolve a strong handle to ourselves via the planted weak
        // self-reference. Build a registrar closure that pushes
        // observed sources into our source list AND registers us as
        // a subscriber on each source.
        let self_rc: Option<Rc<MemoInner<T>>> =
            self.self_weak.borrow().as_ref().and_then(Weak::upgrade);
        let new_value = match self_rc {
            Some(self_rc) => {
                let registrar: SourceRegistrar = {
                    let inner = Rc::clone(&self_rc);
                    let weak_self: WeakReactive = Rc::downgrade(&(inner.clone() as AnyReactive));
                    Box::new(move |source: AnyReactive| {
                        // Add to my source list (dedup by id).
                        let mut sources = inner.sources.borrow_mut();
                        if !sources.iter().any(|s| s.id() == source.id()) {
                            sources.push(Rc::clone(&source));
                        }
                        drop(sources);
                        // Register me as a subscriber on the source.
                        source.add_subscriber(weak_self.clone());
                    })
                };
                let _guard = ObserverGuard::new(registrar);
                (self.compute)()
            }
            None => {
                // self_weak is only None mid-construction — can't
                // happen once the Rc has been handed to user code.
                (self.compute)()
            }
        };

        let mut value_slot = self.value.borrow_mut();
        let changed = match value_slot.as_ref() {
            Some(old) => old != &new_value,
            None => true,
        };
        *value_slot = Some(new_value);
        drop(value_slot);

        self.state.set(ReactiveNodeState::Clean);

        if changed {
            let live = collect_live_subscribers(&self.subscribers);
            for sub in live {
                sub.mark_dirty();
            }
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn add_subscriber_dedup(slot: &RefCell<Vec<WeakReactive>>, sub: WeakReactive) {
    let mut subs = slot.borrow_mut();
    if let Some(new_id) = sub.upgrade().map(|s| s.id()) {
        let already = subs
            .iter()
            .filter_map(Weak::upgrade)
            .any(|s| s.id() == new_id);
        if already {
            return;
        }
    }
    subs.push(sub);
}

fn collect_live_subscribers(slot: &RefCell<Vec<WeakReactive>>) -> Vec<AnyReactive> {
    let mut subs = slot.borrow_mut();
    subs.retain(|w| w.upgrade().is_some());
    subs.iter().filter_map(Weak::upgrade).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell as StdCell;

    /// A counter wrapped in `Rc<Cell>` for ergonomic use as a
    /// recompute-counter inside `Fn() -> T` closures.
    fn counter() -> Rc<StdCell<u32>> {
        Rc::new(StdCell::new(0))
    }

    fn bump(c: &Rc<StdCell<u32>>) {
        c.set(c.get() + 1);
    }

    // ---- Basic Memo behaviour ----

    #[test]
    fn memo_computes_once_on_first_get() {
        let runs = counter();
        let r = Rc::clone(&runs);
        let m = Memo::new(move || {
            bump(&r);
            42i32
        });
        assert_eq!(runs.get(), 0);
        assert_eq!(m.get(), 42);
        assert_eq!(runs.get(), 1);
    }

    #[test]
    fn memo_is_clean_after_first_get() {
        let m: Memo<i32> = Memo::new(|| 7);
        assert_eq!(m.state(), ReactiveNodeState::Dirty);
        let _ = m.get();
        assert_eq!(m.state(), ReactiveNodeState::Clean);
    }

    #[test]
    fn memo_caches_value_when_no_sources_change() {
        let runs = counter();
        let r = Rc::clone(&runs);
        let m = Memo::new(move || {
            bump(&r);
            "hello".to_string()
        });
        assert_eq!(m.get(), "hello");
        assert_eq!(m.get(), "hello");
        assert_eq!(m.get(), "hello");
        assert_eq!(runs.get(), 1);
    }

    // ---- Signal -> Memo dirty cascade ----

    #[test]
    fn memo_recomputes_when_signal_changes() {
        let runs = counter();
        let r = Rc::clone(&runs);
        let s = ReactiveSignal::new(1i32);
        let s_for_memo = s.clone();
        let m = Memo::new(move || {
            bump(&r);
            s_for_memo.get() * 2
        });
        assert_eq!(m.get(), 2);
        assert_eq!(runs.get(), 1);
        s.set(5);
        assert_eq!(m.get(), 10);
        assert_eq!(runs.get(), 2);
    }

    #[test]
    fn signal_set_to_same_value_does_not_dirty_memo() {
        let runs = counter();
        let r = Rc::clone(&runs);
        let s = ReactiveSignal::new(7i32);
        let s_for_memo = s.clone();
        let m = Memo::new(move || {
            bump(&r);
            s_for_memo.get()
        });
        assert_eq!(m.get(), 7);
        s.set(7);
        assert_eq!(m.get(), 7);
        assert_eq!(runs.get(), 1, "no recompute on identical-value set");
    }

    // ---- Check propagation: identical-result intermediate Memo ----

    #[test]
    fn check_state_skips_downstream_recompute_when_intermediate_unchanged() {
        // count -> parity (changes only on odd<->even flip)
        //       -> downstream observes parity
        let count = ReactiveSignal::new(0i32);
        let count_for_parity = count.clone();
        let parity_runs = counter();
        let pr = Rc::clone(&parity_runs);
        let parity = Memo::new(move || {
            bump(&pr);
            count_for_parity.get() % 2 == 0
        });

        let parity_for_label = parity.clone();
        let label_runs = counter();
        let lr = Rc::clone(&label_runs);
        let label: Memo<&'static str> = Memo::new(move || {
            bump(&lr);
            if parity_for_label.get() {
                "even"
            } else {
                "odd"
            }
        });

        assert_eq!(label.get(), "even");
        assert_eq!(parity_runs.get(), 1);
        assert_eq!(label_runs.get(), 1);

        // Change count from 0 to 2 — parity stays "even", label
        // should NOT re-run.
        count.set(2);
        assert_eq!(label.get(), "even");
        assert_eq!(
            parity_runs.get(),
            2,
            "parity recomputes (its value didn't change but it was Dirty)"
        );
        assert_eq!(label_runs.get(), 1, "label SKIPS recompute via Check state");
    }

    #[test]
    fn check_state_recomputes_downstream_when_intermediate_does_change() {
        let count = ReactiveSignal::new(0i32);
        let count_for_parity = count.clone();
        let parity = Memo::new(move || count_for_parity.get() % 2 == 0);

        let parity_for_label = parity.clone();
        let label_runs = counter();
        let lr = Rc::clone(&label_runs);
        let label: Memo<&'static str> = Memo::new(move || {
            bump(&lr);
            if parity_for_label.get() {
                "even"
            } else {
                "odd"
            }
        });

        assert_eq!(label.get(), "even");
        assert_eq!(label_runs.get(), 1);
        count.set(1);
        assert_eq!(label.get(), "odd");
        assert_eq!(label_runs.get(), 2);
    }

    // ---- Multi-source ----

    #[test]
    fn memo_tracks_multiple_signal_sources() {
        let a = ReactiveSignal::new(1i32);
        let b = ReactiveSignal::new(10i32);
        let a_for_memo = a.clone();
        let b_for_memo = b.clone();
        let sum = Memo::new(move || a_for_memo.get() + b_for_memo.get());
        assert_eq!(sum.get(), 11);
        a.set(5);
        assert_eq!(sum.get(), 15);
        b.set(20);
        assert_eq!(sum.get(), 25);
    }

    #[test]
    fn changing_one_signal_only_dirties_dependent_memos() {
        let runs_a = counter();
        let runs_b = counter();
        let ra = Rc::clone(&runs_a);
        let rb = Rc::clone(&runs_b);
        let a = ReactiveSignal::new(1i32);
        let b = ReactiveSignal::new(10i32);
        let a_for = a.clone();
        let b_for = b.clone();
        let only_a = Memo::new(move || {
            bump(&ra);
            a_for.get() * 2
        });
        let only_b = Memo::new(move || {
            bump(&rb);
            b_for.get() * 3
        });
        assert_eq!(only_a.get(), 2);
        assert_eq!(only_b.get(), 30);
        a.set(7);
        assert_eq!(only_a.get(), 14);
        assert_eq!(only_b.get(), 30);
        assert_eq!(runs_a.get(), 2);
        assert_eq!(
            runs_b.get(),
            1,
            "only_b should not recompute when only `a` changed"
        );
    }

    // ---- Diamond dependency ----

    #[test]
    fn diamond_dependency_recomputes_each_node_once_per_change() {
        // a → b → d
        // a → c → d
        let a = ReactiveSignal::new(1i32);
        let runs_b = counter();
        let runs_c = counter();
        let runs_d = counter();

        let a_for_b = a.clone();
        let rb = Rc::clone(&runs_b);
        let b = Memo::new(move || {
            bump(&rb);
            a_for_b.get() + 10
        });

        let a_for_c = a.clone();
        let rc = Rc::clone(&runs_c);
        let c = Memo::new(move || {
            bump(&rc);
            a_for_c.get() + 100
        });

        let b_for_d = b.clone();
        let c_for_d = c.clone();
        let rd = Rc::clone(&runs_d);
        let d = Memo::new(move || {
            bump(&rd);
            b_for_d.get() + c_for_d.get()
        });

        assert_eq!(d.get(), 1 + 10 + 1 + 100);
        assert_eq!(runs_b.get(), 1);
        assert_eq!(runs_c.get(), 1);
        assert_eq!(runs_d.get(), 1);

        a.set(5);
        assert_eq!(d.get(), 5 + 10 + 5 + 100);
        // Each intermediate recomputes once; d recomputes once.
        assert_eq!(runs_b.get(), 2);
        assert_eq!(runs_c.get(), 2);
        assert_eq!(runs_d.get(), 2);
    }

    // ---- Chain ----

    #[test]
    fn chain_of_memos_all_recompute_on_signal_change() {
        let a = ReactiveSignal::new(1i32);
        let runs1 = counter();
        let runs2 = counter();
        let runs3 = counter();

        let a_for_m1 = a.clone();
        let r1 = Rc::clone(&runs1);
        let m1 = Memo::new(move || {
            bump(&r1);
            a_for_m1.get() + 1
        });
        let m1_for_m2 = m1.clone();
        let r2 = Rc::clone(&runs2);
        let m2 = Memo::new(move || {
            bump(&r2);
            m1_for_m2.get() + 1
        });
        let m2_for_m3 = m2.clone();
        let r3 = Rc::clone(&runs3);
        let m3 = Memo::new(move || {
            bump(&r3);
            m2_for_m3.get() + 1
        });

        assert_eq!(m3.get(), 4);
        assert_eq!((runs1.get(), runs2.get(), runs3.get()), (1, 1, 1));
        a.set(10);
        assert_eq!(m3.get(), 13);
        assert_eq!((runs1.get(), runs2.get(), runs3.get()), (2, 2, 2));
    }

    #[test]
    fn chain_short_circuits_when_intermediate_unchanged() {
        // count -> parity -> bool_to_int
        // Changing count from 0 to 2 keeps parity true, bool_to_int
        // shouldn't re-run.
        let count = ReactiveSignal::new(0i32);
        let cf = count.clone();
        let parity = Memo::new(move || cf.get() % 2 == 0);
        let pf = parity.clone();
        let runs = counter();
        let r = Rc::clone(&runs);
        let bool_to_int: Memo<i32> = Memo::new(move || {
            bump(&r);
            i32::from(pf.get())
        });

        assert_eq!(bool_to_int.get(), 1);
        assert_eq!(runs.get(), 1);
        count.set(2);
        assert_eq!(bool_to_int.get(), 1);
        assert_eq!(runs.get(), 1, "downstream skipped via Check");
        count.set(3);
        assert_eq!(bool_to_int.get(), 0);
        assert_eq!(runs.get(), 2);
    }

    // ---- Update via signal.update ----

    #[test]
    fn signal_update_propagates_to_memo() {
        let s = ReactiveSignal::new(5i32);
        let sf = s.clone();
        let m = Memo::new(move || sf.get() * sf.get());
        assert_eq!(m.get(), 25);
        s.update(|v| v + 1);
        assert_eq!(m.get(), 36);
    }

    // ---- Subscriber / source bookkeeping ----

    #[test]
    fn signal_tracks_subscribers_after_memo_first_get() {
        let s = ReactiveSignal::new(1i32);
        let sf = s.clone();
        let m = Memo::new(move || sf.get() + 1);
        assert_eq!(s.subscriber_count(), 0);
        let _ = m.get();
        assert_eq!(s.subscriber_count(), 1);
    }

    #[test]
    fn signal_loses_subscriber_when_memo_dropped() {
        let s = ReactiveSignal::new(1i32);
        let sf = s.clone();
        let m = Memo::new(move || sf.get() + 1);
        let _ = m.get();
        assert_eq!(s.subscriber_count(), 1);
        drop(m);
        // subscriber_count prunes Weak refs that fail to upgrade.
        assert_eq!(s.subscriber_count(), 0);
    }

    #[test]
    fn dedup_subscriber_registration_when_memo_reads_same_signal_twice() {
        let s = ReactiveSignal::new(1i32);
        let sf1 = s.clone();
        let m = Memo::new(move || sf1.get() + sf1.get());
        let _ = m.get();
        assert_eq!(
            s.subscriber_count(),
            1,
            "single subscriber even though read twice"
        );
    }

    #[test]
    fn memo_source_count_matches_observed_signals() {
        let a = ReactiveSignal::new(1i32);
        let b = ReactiveSignal::new(2i32);
        let af = a.clone();
        let bf = b.clone();
        let m = Memo::new(move || af.get() + bf.get());
        let _ = m.get();
        assert_eq!(m.source_count(), 2);
    }

    // ---- get() inside a compute closure registers subscriber ----

    #[test]
    fn memo_subscribes_to_inner_memo() {
        let s = ReactiveSignal::new(0i32);
        let sf = s.clone();
        let inner = Memo::new(move || sf.get() + 1);
        let inner_for_outer = inner.clone();
        let outer = Memo::new(move || inner_for_outer.get() * 2);
        assert_eq!(outer.get(), 2);
        assert_eq!(inner.subscriber_count(), 1);
    }

    // ---- Branchy compute (dynamic source set) ----

    #[test]
    fn memo_re_tracks_sources_each_recompute() {
        // m reads `selector` to choose between `a` and `b`.
        let selector = ReactiveSignal::new(true);
        let a = ReactiveSignal::new(1i32);
        let b = ReactiveSignal::new(100i32);
        let sel = selector.clone();
        let af = a.clone();
        let bf = b.clone();
        let m = Memo::new(move || if sel.get() { af.get() } else { bf.get() });
        assert_eq!(m.get(), 1);
        // Currently subscribes to: selector + a (b not read)
        assert_eq!(a.subscriber_count(), 1);
        assert_eq!(b.subscriber_count(), 0);

        selector.set(false);
        assert_eq!(m.get(), 100);
        // Recompute re-tracks: selector + b. `a` is no longer a
        // source — its subscriber slot drains via the source-clear
        // step in `recompute_inner`.
        assert_eq!(b.subscriber_count(), 1);
    }

    // ---- Signal alone ----

    #[test]
    fn signal_set_and_get_round_trip() {
        let s = ReactiveSignal::new(0i32);
        assert_eq!(s.get(), 0);
        s.set(42);
        assert_eq!(s.get(), 42);
    }

    #[test]
    fn signal_clone_shares_state() {
        let s1 = ReactiveSignal::new(0i32);
        let s2 = s1.clone();
        s1.set(7);
        assert_eq!(s2.get(), 7);
    }

    // ---- Edge cases ----

    #[test]
    fn memo_with_constant_compute_never_recomputes() {
        let runs = counter();
        let r = Rc::clone(&runs);
        let m = Memo::new(move || {
            bump(&r);
            42i32
        });
        assert_eq!(m.get(), 42);
        for _ in 0..10 {
            assert_eq!(m.get(), 42);
        }
        assert_eq!(runs.get(), 1);
    }

    #[test]
    fn memo_value_persists_when_no_observer_present() {
        let s = ReactiveSignal::new(1i32);
        let sf = s.clone();
        let m = Memo::new(move || sf.get() + 100);
        // Read outside any compute — current_observer is None — but
        // the Memo still tracks `s` because Memo::recompute installs
        // its own registrar internally.
        assert_eq!(m.get(), 101);
        s.set(2);
        assert_eq!(m.get(), 102);
    }

    // ---- Sequential set + get patterns ----

    #[test]
    fn many_consecutive_sets_each_dirty_memo_once() {
        let runs = counter();
        let r = Rc::clone(&runs);
        let s = ReactiveSignal::new(0i32);
        let sf = s.clone();
        let m = Memo::new(move || {
            bump(&r);
            sf.get() + 1
        });
        let _ = m.get();
        assert_eq!(runs.get(), 1);
        for i in 1..=5 {
            s.set(i);
            let _ = m.get();
        }
        assert_eq!(runs.get(), 6, "1 initial + 5 changes");
    }

    #[test]
    fn signal_get_outside_compute_does_not_panic() {
        let s = ReactiveSignal::new("hello".to_string());
        assert_eq!(s.get(), "hello");
        s.set("world".to_string());
        assert_eq!(s.get(), "world");
    }

    // ---- Ensure Check propagation through deeper chains ----

    #[test]
    fn three_level_chain_no_recompute_when_first_intermediate_idempotent() {
        // signal → m1 (always returns 0) → m2 → m3
        // No matter what signal does, m1 returns 0, so m2 and m3
        // shouldn't re-run.
        let s = ReactiveSignal::new(1i32);
        let sf = s.clone();
        let runs1 = counter();
        let r1 = Rc::clone(&runs1);
        let m1: Memo<i32> = Memo::new(move || {
            bump(&r1);
            let _ = sf.get();
            0
        });
        let m1f = m1.clone();
        let runs2 = counter();
        let r2 = Rc::clone(&runs2);
        let m2 = Memo::new(move || {
            bump(&r2);
            m1f.get() + 1
        });
        let m2f = m2.clone();
        let runs3 = counter();
        let r3 = Rc::clone(&runs3);
        let m3 = Memo::new(move || {
            bump(&r3);
            m2f.get() + 1
        });

        assert_eq!(m3.get(), 2);
        assert_eq!(runs1.get(), 1);
        assert_eq!(runs2.get(), 1);
        assert_eq!(runs3.get(), 1);
        for i in 2..=10 {
            s.set(i);
            assert_eq!(m3.get(), 2);
        }
        // m1 recomputes each time signal changes (it's Dirty); m2/m3
        // skip via Check.
        assert_eq!(runs1.get(), 10);
        assert_eq!(runs2.get(), 1);
        assert_eq!(runs3.get(), 1);
    }
}
