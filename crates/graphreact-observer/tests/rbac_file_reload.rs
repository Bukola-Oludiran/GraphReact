//! **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Integration
//! tests for [`graphreact_observer::load_registry_from_file`] —
//! the NDJSON-per-line token registry file parser that the SIGHUP
//! reload-tokens path drives.

use graphreact_observer::{
    load_registry_from_file, RegistryLoadError, Role, RotatableRegistry, TokenRegistry,
};

struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("{tag}-{pid}-{n}"));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write_0600(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

#[test]
fn happy_path_loads_three_roles() {
    let tmp = TempDir::new("grx-rbac-load");
    let path = tmp.path().join("tokens.ndjson");
    write_0600(
        &path,
        r#"{"id":"alice","token":"AAAAAAAA","role":"Admin"}
{"id":"reader","token":"BBBBBBBB","role":"ReadOnly"}
{"id":"ci","token":"CCCCCCCC","role":{"Custom":["/v1/metrics"]}}
"#,
    );
    let reg = load_registry_from_file(&path).expect("load");
    assert_eq!(reg.len(), 3);
    let counts = reg.role_counts();
    assert_eq!(counts.admin, 1);
    assert_eq!(counts.read_only, 1);
    assert_eq!(counts.custom, 1);

    let admin = reg.match_token(b"AAAAAAAA").expect("admin match");
    assert_eq!(admin.id.0, "alice");
    assert_eq!(admin.role, Role::Admin);

    let custom = reg.match_token(b"CCCCCCCC").expect("custom match");
    assert!(reg.check_route(custom, "/v1/metrics"));
    assert!(!reg.check_route(custom, "/v1/identity"));
}

#[test]
fn defensive_parse_skips_malformed_lines_when_others_valid() {
    let tmp = TempDir::new("grx-rbac-skip");
    let path = tmp.path().join("tokens.ndjson");
    write_0600(
        &path,
        // line 0 is malformed (truncated JSON);
        // line 1 is a comment;
        // line 2 is valid.
        r#"{"id":"bad",
# this is a comment line, skip it
{"id":"good","token":"DDDDDDDD","role":"Admin"}
"#,
    );
    let reg = load_registry_from_file(&path).expect("load");
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.match_token(b"DDDDDDDD").unwrap().id.0, "good");
}

#[test]
fn empty_file_returns_parse_error() {
    let tmp = TempDir::new("grx-rbac-empty");
    let path = tmp.path().join("tokens.ndjson");
    write_0600(&path, "\n\n");
    let err = load_registry_from_file(&path).unwrap_err();
    assert!(matches!(err, RegistryLoadError::Parse { .. }));
    assert_eq!(err.kind_str(), "parse");
}

#[cfg(unix)]
#[test]
fn refuses_world_readable_file() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new("grx-rbac-mode");
    let path = tmp.path().join("tokens.ndjson");
    std::fs::write(
        &path,
        r#"{"id":"a","token":"AAAAAAAA","role":"Admin"}
"#,
    )
    .expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod 644");
    let err = load_registry_from_file(&path).unwrap_err();
    assert!(matches!(err, RegistryLoadError::Mode { .. }));
    assert_eq!(err.kind_str(), "mode");
}

#[test]
fn rotatable_registry_round_trip_via_load() {
    // Simulate the SIGHUP flow: build a RotatableRegistry from an
    // initial file, then "re-read" a fresh file + rotate. The
    // current() snapshot updates atomically.
    let tmp = TempDir::new("grx-rbac-rot");
    let path1 = tmp.path().join("v1.ndjson");
    let path2 = tmp.path().join("v2.ndjson");
    write_0600(
        &path1,
        r#"{"id":"a","token":"AAAAAAAA","role":"Admin"}
"#,
    );
    write_0600(
        &path2,
        r#"{"id":"a","token":"AAAAAAAA","role":"Admin"}
{"id":"b","token":"BBBBBBBB","role":"ReadOnly"}
"#,
    );
    let reg1 = load_registry_from_file(&path1).expect("v1");
    assert_eq!(reg1.len(), 1);
    let rot = RotatableRegistry::new(std::sync::Arc::new(reg1));
    assert_eq!(rot.current().len(), 1);
    let reg2 = load_registry_from_file(&path2).expect("v2");
    rot.rotate(std::sync::Arc::new(reg2));
    assert_eq!(rot.current().len(), 2);
}

#[test]
fn invalid_token_bytes_skipped_but_others_kept() {
    let tmp = TempDir::new("grx-rbac-badtok");
    let path = tmp.path().join("tokens.ndjson");
    write_0600(
        &path,
        // Empty token bytes are rejected by BearerToken::from_bytes
        // — line skipped, second line kept.
        r#"{"id":"bad","token":"","role":"Admin"}
{"id":"good","token":"EEEEEEEE","role":"Admin"}
"#,
    );
    let reg = load_registry_from_file(&path).expect("load");
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.match_token(b"EEEEEEEE").unwrap().id.0, "good");
    let _ = TokenRegistry::new; // suppress unused-import warning
}
