//! Engine-creation options.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IsolateOptions {
    /// Memory limit in bytes; engine may apply approximations. `None`
    /// means "engine default".
    pub memory_limit: Option<usize>,
    /// How aggressively the engine drains its microtask queue.
    pub microtask_policy: MicrotaskPolicy,
    /// Whether to load a startup snapshot/bytecode-cache if the engine
    /// supports one. JSC supports bytecode cache; V8 supports startup
    /// snapshots; Boa / QuickJS do not.
    pub snapshot: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MicrotaskPolicy {
    /// Drain microtasks at every scheduler tick boundary (M12 default).
    #[default]
    DrainEachTick,
    /// Drain only when explicitly called.
    Manual,
}
