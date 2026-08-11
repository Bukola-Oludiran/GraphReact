//! **M16-Backend Phase 8.** `--debug=<engine>:<port>` flag parser.
//!
//! Format `engine:port` keeps the same parser open to JSC (`jsc:9229`)
//! when P9 lands. P8 ships `cpython:<port>` only; the JSC variant
//! returns [`DebugFlagError::JscNotYetSupported`] until P9.
//!
//! Plan-locked default port mappings (mirrors VS Code / Chrome
//! DevTools defaults):
//! - `cpython` — 5678 (debugpy's default `--listen` port).
//! - `jsc` — 9229 (Chrome DevTools' inspector port; same as Node).
//!
//! Operators override by passing the explicit port: `--debug=cpython:6000`.

use std::str::FromStr;

use thiserror::Error;

/// Per-body debug protocol selector — pairs with a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugEngine {
    /// CPython debugpy attach via `import debugpy; debugpy.listen(...)`.
    /// VS Code / PyCharm / `pdb`'s `attach` mode all speak this protocol.
    Cpython,
    /// JSC remote inspection via `JSGlobalContextSetRemoteInspectionEnabled`
    /// (P9). P8 returns [`DebugFlagError::JscNotYetSupported`] when
    /// this variant is requested.
    Jsc,
    /// Go Delve attach — Delve speaks DAP (Debug Adapter Protocol) on
    /// TCP `:2345` by default. VS Code Go extension + IntelliJ Go
    /// plugin both speak this protocol. Returns
    /// [`DebugFlagError::GoNotYetSupported`] until M17 phase 1
    /// closes; the parser accepts the variant so the flag surface is
    /// stable.
    Go,
}

impl DebugEngine {
    /// Plan-locked default port for the engine. Operators override
    /// by passing an explicit port (`--debug=cpython:6000`).
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            DebugEngine::Cpython => 5678,
            DebugEngine::Jsc => 9229,
            DebugEngine::Go => 2345,
        }
    }
}

impl std::fmt::Display for DebugEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DebugEngine::Cpython => write!(f, "cpython"),
            DebugEngine::Jsc => write!(f, "jsc"),
            DebugEngine::Go => write!(f, "go"),
        }
    }
}

/// Decoded `--debug=<engine>:<port>` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebugFlag {
    pub engine: DebugEngine,
    pub port: u16,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DebugFlagError {
    #[error("expected `<engine>:<port>` (got `{0}`)")]
    BadFormat(String),
    #[error("unknown debug engine `{0}`; expected `cpython`, `jsc`, or `go`")]
    UnknownEngine(String),
    #[error("port `{0}`: {1}")]
    BadPort(String, String),
    #[error("debug engine `jsc` not yet wired (P8 ships cpython only; JSC CDP lands in P9)")]
    JscNotYetSupported,
    #[error(
        "debug engine `go` is parser-accepted but not yet wired (Delve DAP attach lands with M17 phase 1)"
    )]
    GoNotYetSupported,
}

impl FromStr for DebugFlag {
    type Err = DebugFlagError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (engine_str, port_str) = s
            .split_once(':')
            .ok_or_else(|| DebugFlagError::BadFormat(s.to_string()))?;
        let engine = match engine_str {
            "cpython" => DebugEngine::Cpython,
            "jsc" => DebugEngine::Jsc,
            "go" => DebugEngine::Go,
            other => return Err(DebugFlagError::UnknownEngine(other.to_string())),
        };
        let port: u16 = port_str.parse().map_err(|e: std::num::ParseIntError| {
            DebugFlagError::BadPort(port_str.to_string(), e.to_string())
        })?;
        Ok(DebugFlag { engine, port })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpython_port() {
        let f: DebugFlag = "cpython:5678".parse().unwrap();
        assert_eq!(f.engine, DebugEngine::Cpython);
        assert_eq!(f.port, 5678);
    }

    #[test]
    fn parses_jsc_port_but_engine_is_jsc() {
        // Parser accepts both; surface-level engine validation lives
        // in the dispatch arm.
        let f: DebugFlag = "jsc:9229".parse().unwrap();
        assert_eq!(f.engine, DebugEngine::Jsc);
    }

    #[test]
    fn rejects_missing_colon() {
        let err = "cpython5678".parse::<DebugFlag>().unwrap_err();
        assert!(matches!(err, DebugFlagError::BadFormat(_)));
    }

    #[test]
    fn rejects_unknown_engine() {
        let err = "v8:9229".parse::<DebugFlag>().unwrap_err();
        assert!(matches!(err, DebugFlagError::UnknownEngine(_)));
    }

    #[test]
    fn rejects_bad_port() {
        let err = "cpython:abc".parse::<DebugFlag>().unwrap_err();
        assert!(matches!(err, DebugFlagError::BadPort(_, _)));
    }

    #[test]
    fn rejects_overflow_port() {
        let err = "cpython:99999".parse::<DebugFlag>().unwrap_err();
        assert!(matches!(err, DebugFlagError::BadPort(_, _)));
    }

    #[test]
    fn default_ports_match_canonical() {
        assert_eq!(DebugEngine::Cpython.default_port(), 5678);
        assert_eq!(DebugEngine::Jsc.default_port(), 9229);
        assert_eq!(DebugEngine::Go.default_port(), 2345);
    }

    #[test]
    fn display_round_trips_engine_name() {
        assert_eq!(format!("{}", DebugEngine::Cpython), "cpython");
        assert_eq!(format!("{}", DebugEngine::Jsc), "jsc");
        assert_eq!(format!("{}", DebugEngine::Go), "go");
    }

    #[test]
    fn parses_go_port() {
        let f: DebugFlag = "go:2345".parse().unwrap();
        assert_eq!(f.engine, DebugEngine::Go);
        assert_eq!(f.port, 2345);
    }
}
