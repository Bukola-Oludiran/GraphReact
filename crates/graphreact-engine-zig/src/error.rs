//! Error types shared by every engine implementation.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("engine `{name}` is not available in this build")]
    EngineUnavailable { name: String },

    #[error("isolate handle invalid or already disposed")]
    InvalidIsolate,

    #[error("module handle invalid or already disposed")]
    InvalidModule,

    #[error("callback `{name}` is already registered on this isolate")]
    CallbackAlreadyRegistered { name: String },

    #[error("callback `{name}` not found on this isolate")]
    CallbackNotFound { name: String },

    #[error("script error in `{module}`: {message}")]
    ScriptError { module: String, message: String },

    #[error("host call rejected: {message}")]
    HostCallRejected { message: String },

    #[error("engine internal error: {0}")]
    Internal(String),

    /// **M16-Backend P9.** Per-body source debugger isn't supported
    /// for this engine on this platform. Concrete engines surface
    /// this when `enable_debugger` is called on a build target where
    /// the underlying debug protocol isn't reachable (e.g., JSC's
    /// `JSGlobalContextSetRemoteInspectionEnabled` is macOS-only;
    /// Linux returns this until M12.3b ships vendored WebKit).
    #[error("debugger unsupported for engine `{engine}` on this platform: {reason}")]
    DebuggerUnsupported { engine: String, reason: String },
}

pub type EngineResult<T> = Result<T, EngineError>;
