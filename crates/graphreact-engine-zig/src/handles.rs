//! Opaque handles handed out by the engine.
//!
//! Handles are `u64` indices into engine-private tables. The engine
//! validates them on every call; using a stale handle returns
//! [`EngineError::InvalidIsolate`] / [`InvalidModule`] rather than
//! UB.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IsolateHandle(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModuleHandle(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FunctionHandle(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallbackId(pub u64);

impl IsolateHandle {
    pub const INVALID: Self = Self(0);
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl ModuleHandle {
    pub const INVALID: Self = Self(0);
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl FunctionHandle {
    pub const INVALID: Self = Self(0);
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}
