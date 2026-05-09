//! Bit flags and runtime defaults.
//!
//! Mirrors `graphreact/src/core/runtime/constants.ts`.

/// Bit flags packed into [`crate::runtime::node_instance::NodeInstance::bits`].
pub mod flags {
    /// Node has been queued for re-evaluation.
    pub const DIRTY: u32 = 1 << 0;
    /// Node is currently inside its component function.
    pub const PROCESSING: u32 = 1 << 1;
    /// First evaluation has completed.
    pub const MOUNTED: u32 = 1 << 2;
    /// Node has been removed from the graph.
    pub const UNMOUNTED: u32 = 1 << 3;
}

/// Default TTL for new messages.
pub const DEFAULT_TTL: u32 = 10;

/// Maximum scheduler iterations per `process_batch` to break runaway cycles.
pub const MAX_PROCESS_DEPTH: usize = 100;
