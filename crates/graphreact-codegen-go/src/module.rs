//! Deterministic sidecar Go-module builds for `.grx` Go bodies.
//!
//! A module is owned by the application, never GraphReact. We discover
//! the nearest ancestor `go.mod`, validate every Go-bearing source file
//! resolves to the same root, and invoke the Go toolchain in readonly
//! module mode. Credentials are supplied by the CLI as short-lived
//! values and are excluded from fingerprints and evidence artifacts.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{BuildArtifact, CodegenError};

const CACHE_RECORD: &str = "grx-go-build.json";
const INVENTORY: &str = "grx-go-dependencies.json";
const SBOM: &str = "grx-go.cdx.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoModule {
    pub root: PathBuf,
    pub go_mod: PathBuf,
    pub go_sum: Option<PathBuf>,
    pub auth_path: Option<PathBuf>,
    pub auth: Option<GoAuthConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoAuthConfig {
    pub version: u32,
    #[serde(default)]
    pub private_patterns: Vec<String>,
    #[serde(default)]
    pub https: Vec<HttpsAuth>,
    pub proxy: Option<ProxyAuth>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpsAuth {
    pub host: String,
    pub username: String,
    pub password_secret: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyAuth {
    pub url: String,
    pub username_secret: String,
    pub password_secret: String,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedGoAuth {
    pub https_passwords: BTreeMap<String, String>,
    pub proxy_username: Option<String>,
    pub proxy_password: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GoBuildOptions {
    pub module: Option<GoModule>,
    pub offline: bool,
    pub module_cache: Option<PathBuf>,
    pub auth: Option<ResolvedGoAuth>,
}

pub struct PreparedBuild {
    pub fingerprint_hex: String,
    env: BTreeMap<OsString, OsString>,
    secret_values: Vec<String>,
    _credentials: Option<PreparedCredentials>,
}

struct PreparedCredentials {
    _dir: tempfile::TempDir,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheRecord {
    fingerprint: String,
    library_sha256: String,
    sbom: String,
    inventory: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DependencyInventory {
    schema_version: u32,
    go_version: String,
    module_root: Option<String>,
    artifact_sha256: String,
    modules: Vec<GoListModule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct GoListModule {
    path: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    sum: String,
    #[serde(default)]
    main: bool,
    #[serde(default)]
    replace: Option<Box<GoListModule>>,
}

pub fn discover_module(
    entry: &Path,
    go_sources: &[PathBuf],
) -> Result<Option<GoModule>, CodegenError> {
    let entry = fs::canonicalize(entry).map_err(|source| CodegenError::Io {
        path: entry.to_path_buf(),
        source,
    })?;
    let entry_root = nearest_module(entry.parent().unwrap_or(Path::new(".")));
    let Some(root) = entry_root else {
        for source in go_sources {
            if let Some(other) = nearest_module(source.parent().unwrap_or(Path::new("."))) {
                return Err(module_error(format!(
                    "Go source {} belongs to module {}, but entry {} has no module",
                    source.display(),
                    other.display(),
                    entry.display()
                )));
            }
        }
        return Ok(None);
    };

    for source in go_sources {
        let canonical = fs::canonicalize(source).map_err(|error| {
            module_error(format!(
                "cannot resolve Go-bearing source {}: {error}",
                source.display()
            ))
        })?;
        let source_root = nearest_module(canonical.parent().unwrap_or(Path::new(".")));
        if source_root.as_deref() != Some(root.as_path()) {
            return Err(module_error(format!(
                "Go-bearing source {} resolves to module {}, expected {}",
                canonical.display(),
                source_root
                    .as_deref()
                    .map_or_else(|| "<none>".to_string(), |p| p.display().to_string()),
                root.display()
            )));
        }
    }

    let vendor = root.join("vendor");
    if vendor.is_dir() {
        return Err(module_error(format!(
            "{} contains vendor/; GraphReact requires unambiguous readonly module mode",
            root.display()
        )));
    }
    let go_mod = root.join("go.mod");
    let go_sum_path = root.join("go.sum");
    let go_sum = go_sum_path.exists().then_some(go_sum_path);
    let go_mod_text = fs::read_to_string(&go_mod).map_err(|source| CodegenError::Io {
        path: go_mod.clone(),
        source,
    })?;
    if contains_require(&go_mod_text) && go_sum.is_none() && !all_requires_replaced(&go_mod_text) {
        return Err(module_error(format!(
            "{} declares external requirements but {} is missing",
            go_mod.display(),
            root.join("go.sum").display()
        )));
    }

    let auth_path = root.join("go.auth.json");
    let (auth_path, auth) = if auth_path.exists() {
        validate_auth_permissions(&auth_path)?;
        let bytes = fs::read(&auth_path).map_err(|source| CodegenError::Io {
            path: auth_path.clone(),
            source,
        })?;
        let auth: GoAuthConfig = serde_json::from_slice(&bytes)
            .map_err(|error| module_error(format!("invalid {}: {error}", auth_path.display())))?;
        validate_auth(&auth)?;
        (Some(auth_path), Some(auth))
    } else {
        (None, None)
    };
    Ok(Some(GoModule {
        root,
        go_mod,
        go_sum,
        auth_path,
        auth,
    }))
}

fn nearest_module(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("go.mod").is_file())
        .and_then(|p| fs::canonicalize(p).ok())
}

fn contains_require(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .any(|line| line == "require (" || line.starts_with("require "))
}

fn all_requires_replaced(text: &str) -> bool {
    let mut required = BTreeSet::new();
    let mut local_replacements = BTreeSet::new();
    let mut in_require = false;
    let mut in_replace = false;
    for raw_line in text.lines() {
        let line = raw_line.split("//").next().unwrap_or("").trim();
        match line {
            "require (" => {
                in_require = true;
                continue;
            }
            "replace (" => {
                in_replace = true;
                continue;
            }
            ")" => {
                in_require = false;
                in_replace = false;
                continue;
            }
            _ => {}
        }
        if in_require {
            if let Some(module) = line.split_whitespace().next() {
                if !module.is_empty() {
                    required.insert(module);
                }
            }
        } else if let Some(rest) = line.strip_prefix("require ") {
            if let Some(module) = rest.split_whitespace().next() {
                required.insert(module);
            }
        }

        let replacement = if in_replace {
            line.split_once("=>")
        } else {
            line.strip_prefix("replace ")
                .and_then(|line| line.split_once("=>"))
        };
        if let Some((source, target)) = replacement {
            let target = target.split_whitespace().next().unwrap_or("");
            if Path::new(target).is_absolute() || target.starts_with('.') {
                if let Some(module) = source.split_whitespace().next() {
                    local_replacements.insert(module);
                }
            }
        }
    }
    !required.is_empty() && required.is_subset(&local_replacements)
}

fn validate_auth(auth: &GoAuthConfig) -> Result<(), CodegenError> {
    if auth.version != 1 {
        return Err(module_error(format!(
            "go.auth.json version must be 1, got {}",
            auth.version
        )));
    }
    let mut patterns = BTreeSet::new();
    for pattern in &auth.private_patterns {
        if pattern.is_empty() || pattern.chars().any(char::is_whitespace) || pattern.contains("..")
        {
            return Err(module_error(format!(
                "invalid private module pattern `{pattern}`"
            )));
        }
        if !patterns.insert(pattern) {
            return Err(module_error(format!(
                "duplicate private module pattern `{pattern}`"
            )));
        }
    }
    let mut hosts = BTreeSet::new();
    for item in &auth.https {
        if !valid_host(&item.host)
            || item.username.is_empty()
            || !valid_secret_name(&item.password_secret)
        {
            return Err(module_error(format!(
                "invalid HTTPS auth entry for host `{}`",
                item.host
            )));
        }
        if !hosts.insert(item.host.as_str()) {
            return Err(module_error(format!(
                "duplicate HTTPS auth host `{}`",
                item.host
            )));
        }
    }
    if let Some(proxy) = &auth.proxy {
        if !valid_https_url(&proxy.url)
            || !valid_secret_name(&proxy.username_secret)
            || !valid_secret_name(&proxy.password_secret)
        {
            return Err(module_error(
                "proxy URL must be credential-free HTTPS and secret identifiers must be valid",
            ));
        }
    }
    Ok(())
}

fn valid_https_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://") else {
        return false;
    };
    if rest.is_empty() || rest.contains(['@', '?', '#']) || rest.chars().any(char::is_whitespace) {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or("");
    let (host, port) = authority
        .rsplit_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    valid_host(host)
        && port.is_none_or(|port| {
            !port.is_empty()
                && port.chars().all(|character| character.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port != 0)
        })
}

fn valid_host(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['/', ':', '@'])
        && value.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

fn valid_secret_name(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(unix)]
fn validate_auth_permissions(path: &Path) -> Result<(), CodegenError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .map_err(|source| CodegenError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .permissions()
        .mode();
    if mode & 0o022 != 0 {
        return Err(module_error(format!(
            "{} must not be group/world writable (mode {:o})",
            path.display(),
            mode & 0o777
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_auth_permissions(_path: &Path) -> Result<(), CodegenError> {
    Ok(())
}

pub fn required_secret_names(module: &GoModule) -> Vec<String> {
    let Some(auth) = &module.auth else {
        return Vec::new();
    };
    let mut names = BTreeSet::new();
    names.extend(auth.https.iter().map(|entry| entry.password_secret.clone()));
    if let Some(proxy) = &auth.proxy {
        names.insert(proxy.username_secret.clone());
        names.insert(proxy.password_secret.clone());
    }
    names.into_iter().collect()
}

pub fn prepare_build(
    go_bin: &str,
    src_dir: &Path,
    out_dir: &Path,
    options: &GoBuildOptions,
) -> Result<PreparedBuild, CodegenError> {
    let mut env = sanitized_environment(out_dir, options);
    let credentials = prepare_credentials(options, &mut env)?;
    let version = Command::new(go_bin)
        .arg("version")
        .output()
        .map_err(|source| CodegenError::Io {
            path: PathBuf::from(go_bin),
            source,
        })?;
    if !version.status.success() {
        return Err(CodegenError::ToolchainMissing);
    }
    let mut hash = Sha256::new();
    for name in ["grx_prelude.go", "dispatch.go"] {
        hash_file(&mut hash, &src_dir.join(name))?;
    }
    let mut bodies: Vec<_> = fs::read_dir(src_dir)
        .map_err(|source| CodegenError::Io {
            path: src_dir.to_path_buf(),
            source,
        })?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(OsStr::to_str).is_some_and(|n| {
                n.starts_with("body_")
                    && Path::new(n)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("go"))
            })
        })
        .collect();
    bodies.sort();
    for path in bodies {
        hash_file(&mut hash, &path)?;
    }
    if let Some(module) = &options.module {
        hash_file(&mut hash, &module.go_mod)?;
        let go_mod_text =
            fs::read_to_string(&module.go_mod).map_err(|source| CodegenError::Io {
                path: module.go_mod.clone(),
                source,
            })?;
        for replacement in local_replace_paths(&go_mod_text) {
            let replacement = if replacement.is_absolute() {
                replacement
            } else {
                module.root.join(replacement)
            };
            hash_local_replacement(&mut hash, &replacement)?;
        }
        if let Some(sum) = &module.go_sum {
            hash_file(&mut hash, sum)?;
        }
        if let Some(auth_path) = &module.auth_path {
            hash_file(&mut hash, auth_path)?;
        }
    } else {
        hash_file(&mut hash, &src_dir.join("go.mod"))?;
    }
    hash.update(&version.stdout);
    hash.update(std::env::consts::OS.as_bytes());
    hash.update(std::env::consts::ARCH.as_bytes());
    hash.update(b"GOWORK=off\0-mod=readonly\0-buildmode=c-shared");
    for name in ["GOOS", "GOARCH", "GOAMD64", "GOARM", "CGO_ENABLED"] {
        hash.update(name.as_bytes());
        hash.update(b"=");
        if let Some(value) = env.get(OsStr::new(name)) {
            hash.update(value.as_encoded_bytes());
        }
        hash.update(b"\0");
    }
    hash.update(if options.offline { "offline" } else { "online" }.as_bytes());
    let fingerprint_hex = format!("{:x}", hash.finalize());
    let secret_values = options.auth.as_ref().map_or_else(Vec::new, |auth| {
        auth.https_passwords
            .values()
            .cloned()
            .chain(auth.proxy_username.iter().cloned())
            .chain(auth.proxy_password.iter().cloned())
            .collect()
    });
    Ok(PreparedBuild {
        fingerprint_hex,
        env,
        secret_values,
        _credentials: credentials,
    })
}

fn hash_file(hash: &mut Sha256, path: &Path) -> Result<(), CodegenError> {
    let bytes = fs::read(path).map_err(|source| CodegenError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    hash.update(path.file_name().unwrap_or_default().as_encoded_bytes());
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    Ok(())
}

fn local_replace_paths(go_mod: &str) -> Vec<PathBuf> {
    let mut paths: Vec<_> = go_mod
        .lines()
        .filter_map(|line| line.split_once("=>").map(|(_, replacement)| replacement))
        .filter_map(|replacement| {
            let candidate = replacement
                .split_whitespace()
                .next()?
                .trim_matches(['"', '\'']);
            let path = PathBuf::from(candidate);
            (path.is_absolute() || candidate.starts_with('.')).then_some(path)
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn hash_local_replacement(hash: &mut Sha256, root: &Path) -> Result<(), CodegenError> {
    let root = fs::canonicalize(root).map_err(|source| CodegenError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let mut pending = vec![root.clone()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries: Vec<_> = fs::read_dir(&directory)
            .map_err(|source| CodegenError::Io {
                path: directory.clone(),
                source,
            })?
            .collect::<Result<_, _>>()
            .map_err(|source| CodegenError::Io {
                path: directory.clone(),
                source,
            })?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            if entry.file_name() == OsStr::new(".git") {
                continue;
            }
            let file_type = entry.file_type().map_err(|source| CodegenError::Io {
                path: entry.path(),
                source,
            })?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
            } else if file_type.is_symlink() {
                let target = fs::read_link(entry.path()).map_err(|source| CodegenError::Io {
                    path: entry.path(),
                    source,
                })?;
                hash.update(
                    entry
                        .path()
                        .strip_prefix(&root)
                        .unwrap_or(entry.path().as_path())
                        .as_os_str()
                        .as_encoded_bytes(),
                );
                hash.update(target.as_os_str().as_encoded_bytes());
            }
        }
    }
    files.sort();
    for file in files {
        let relative = file.strip_prefix(&root).unwrap_or(&file);
        let bytes = fs::read(&file).map_err(|source| CodegenError::Io {
            path: file.clone(),
            source,
        })?;
        hash.update(relative.as_os_str().as_encoded_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    Ok(())
}

fn sanitized_environment(out_dir: &Path, options: &GoBuildOptions) -> BTreeMap<OsString, OsString> {
    const ALLOWED: &[&str] = &[
        "PATH",
        "TMPDIR",
        "TMP",
        "TEMP",
        "SystemRoot",
        "ComSpec",
        "PATHEXT",
        "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET",
        "DEVELOPER_DIR",
        "CC",
        "CXX",
        "CGO_ENABLED",
        "AR",
        "PKG_CONFIG_PATH",
        "PKG_CONFIG_LIBDIR",
        "PKG_CONFIG_SYSROOT_DIR",
        "GOOS",
        "GOARCH",
        "GOAMD64",
        "GOARM",
        "GOROOT",
    ];
    let mut env = BTreeMap::new();
    for name in ALLOWED {
        if let Some(value) = std::env::var_os(name) {
            env.insert((*name).into(), value);
        }
    }
    env.insert("GOWORK".into(), "off".into());
    env.insert(
        "GOCACHE".into(),
        absolute_path(&out_dir.join(".go-build-cache")).into_os_string(),
    );
    let module_cache = options
        .module_cache
        .clone()
        .or_else(|| std::env::var_os("GOMODCACHE").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("go/pkg/mod")));
    if let Some(cache) = module_cache {
        env.insert("GOMODCACHE".into(), absolute_path(&cache).into_os_string());
    }
    if options.offline {
        env.insert("GOPROXY".into(), "off".into());
        env.insert("GOSUMDB".into(), "off".into());
    } else {
        env.insert("GOPROXY".into(), "https://proxy.golang.org,direct".into());
    }
    env
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn prepare_credentials(
    options: &GoBuildOptions,
    env: &mut BTreeMap<OsString, OsString>,
) -> Result<Option<PreparedCredentials>, CodegenError> {
    let Some(module) = &options.module else {
        return Ok(None);
    };
    let Some(policy) = &module.auth else {
        return Ok(None);
    };
    if options.offline {
        return Ok(None);
    }
    let auth = options
        .auth
        .as_ref()
        .ok_or_else(|| module_error("go.auth.json requires resolved GraphReact build secrets"))?;
    let dir = tempfile::Builder::new()
        .prefix("grx-go-auth-")
        .tempdir()
        .map_err(|source| CodegenError::Io {
            path: std::env::temp_dir(),
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).map_err(|source| {
            CodegenError::Io {
                path: dir.path().to_path_buf(),
                source,
            }
        })?;
    }
    let mut git = String::new();
    for item in &policy.https {
        let password = auth.https_passwords.get(&item.host).ok_or_else(|| {
            module_error(format!(
                "missing resolved secret for HTTPS host {}",
                item.host
            ))
        })?;
        let authenticated = format!(
            "https://{}:{}@{}/",
            percent_encode(&item.username),
            percent_encode(password),
            item.host
        );
        let _ = write!(
            git,
            "[url \"{authenticated}\"]\n\tinsteadOf = https://{}/\n",
            item.host
        );
    }
    let git_path = dir.path().join("gitconfig");
    write_private(&git_path, git.as_bytes())?;
    env.insert("HOME".into(), dir.path().as_os_str().to_owned());
    env.insert("GIT_CONFIG_GLOBAL".into(), git_path.into_os_string());
    env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    if !policy.private_patterns.is_empty() {
        let patterns = policy.private_patterns.join(",");
        env.insert("GOPRIVATE".into(), patterns.clone().into());
        env.insert("GONOSUMDB".into(), patterns.into());
    }
    if let Some(proxy) = &policy.proxy {
        let username = auth
            .proxy_username
            .as_deref()
            .ok_or_else(|| module_error("missing resolved proxy username"))?;
        let password = auth
            .proxy_password
            .as_deref()
            .ok_or_else(|| module_error("missing resolved proxy password"))?;
        let rest = proxy
            .url
            .strip_prefix("https://")
            .expect("validated HTTPS proxy");
        let url = format!(
            "https://{}:{}@{}",
            percent_encode(username),
            percent_encode(password),
            rest
        );
        env.insert(
            "GOPROXY".into(),
            format!("{url},https://proxy.golang.org,direct").into(),
        );
        env.insert("GONOPROXY".into(), "none".into());
    }
    Ok(Some(PreparedCredentials { _dir: dir }))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), CodegenError> {
    fs::write(path, bytes).map_err(|source| CodegenError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            CodegenError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

pub fn go_command(go_bin: &str, cwd: &Path, prepared: &PreparedBuild) -> Command {
    let mut command = Command::new(go_bin);
    command.current_dir(cwd).env_clear().envs(&prepared.env);
    command
}

#[must_use]
pub fn redact_output(prepared: &PreparedBuild, bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    for secret in &prepared.secret_values {
        if !secret.is_empty() {
            text = text.replace(secret, "[REDACTED]");
            text = text.replace(&percent_encode(secret), "[REDACTED]");
        }
    }
    text
}

pub fn run_go_verify(
    go_bin: &str,
    cwd: &Path,
    prepared: &PreparedBuild,
) -> Result<(), CodegenError> {
    let output = go_command(go_bin, cwd, prepared)
        .args(["mod", "verify"])
        .output()
        .map_err(|source| CodegenError::Io {
            path: cwd.to_path_buf(),
            source,
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(CodegenError::GoBuildFailed {
        status: output.status.code().unwrap_or(-1),
        stdout: redact_output(prepared, &output.stdout),
        stderr: redact_output(prepared, &output.stderr),
    })
}

pub fn try_cached_artifact(
    library_path: &Path,
    out_dir: &Path,
    prepared: &PreparedBuild,
) -> Result<Option<BuildArtifact>, CodegenError> {
    let record_path = out_dir.join(CACHE_RECORD);
    if !record_path.exists() || !library_path.exists() {
        return Ok(None);
    }
    let record: CacheRecord = match fs::read(&record_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(record) => record,
        None => return Ok(None),
    };
    if record.fingerprint != prepared.fingerprint_hex {
        return Ok(None);
    }
    let actual = format!(
        "{:x}",
        Sha256::digest(fs::read(library_path).map_err(|source| CodegenError::Io {
            path: library_path.to_path_buf(),
            source
        })?)
    );
    if actual != record.library_sha256 {
        return Ok(None);
    }
    let sbom_path = out_dir.join(record.sbom);
    let dependency_inventory_path = out_dir.join(record.inventory);
    if !sbom_path.is_file() || !dependency_inventory_path.is_file() {
        return Ok(None);
    }
    let header = super::derive_header_path(library_path);
    Ok(Some(BuildArtifact {
        library_path: library_path.to_path_buf(),
        header_path: header.exists().then_some(header),
        library_sha256_hex: actual,
        build_fingerprint_hex: prepared.fingerprint_hex.clone(),
        sbom_path,
        dependency_inventory_path,
    }))
}

pub fn write_cache_record(out_dir: &Path, artifact: &BuildArtifact) -> Result<(), CodegenError> {
    let record = CacheRecord {
        fingerprint: artifact.build_fingerprint_hex.clone(),
        library_sha256: artifact.library_sha256_hex.clone(),
        sbom: artifact
            .sbom_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        inventory: artifact
            .dependency_inventory_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    };
    write_json(&out_dir.join(CACHE_RECORD), &record)
}

pub fn write_supply_chain_evidence(
    go_bin: &str,
    cwd: &Path,
    out_dir: &Path,
    prepared: &PreparedBuild,
    artifact_sha256: &str,
) -> Result<(PathBuf, PathBuf), CodegenError> {
    let output = go_command(go_bin, cwd, prepared)
        .args(["list", "-mod=readonly", "-m", "-json", "all"])
        .output()
        .map_err(|source| CodegenError::Io {
            path: cwd.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(CodegenError::GoBuildFailed {
            status: output.status.code().unwrap_or(-1),
            stdout: redact_output(prepared, &output.stdout),
            stderr: redact_output(prepared, &output.stderr),
        });
    }
    let mut modules: Vec<GoListModule> = serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter::<GoListModule>()
        .collect::<Result<_, _>>()
        .map_err(|error| module_error(format!("cannot decode `go list -m -json all`: {error}")))?;
    modules.sort_by(|a, b| a.path.cmp(&b.path).then(a.version.cmp(&b.version)));
    let version = go_command(go_bin, cwd, prepared)
        .arg("version")
        .output()
        .map_err(|source| CodegenError::Io {
            path: PathBuf::from(go_bin),
            source,
        })?;
    let go_version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    let inventory = DependencyInventory {
        schema_version: 1,
        go_version: go_version.clone(),
        module_root: modules.iter().find(|m| m.main).map(|m| m.path.clone()),
        artifact_sha256: artifact_sha256.to_string(),
        modules: modules.clone(),
    };
    let inventory_path = out_dir.join(INVENTORY);
    write_json(&inventory_path, &inventory)?;
    let components: Vec<_> = modules
        .iter()
        .map(|module| {
            let version = if module.version.is_empty() {
                "0.0.0"
            } else {
                &module.version
            };
            let mut component = serde_json::json!({
                "type": "library", "name": module.path, "version": version,
                "purl": format!("pkg:golang/{}@{}", module.path, version)
            });
            if !module.sum.is_empty() {
                component["hashes"] = serde_json::json!([{
                    "alg":"SHA-256",
                    "content":module.sum.trim_start_matches("h1:")
                }]);
            }
            if let Some(replacement) = &module.replace {
                let replacement_value = if replacement.version.is_empty() {
                    replacement.path.clone()
                } else {
                    format!("{}@{}", replacement.path, replacement.version)
                };
                component["properties"] = serde_json::json!([{
                    "name": "graphreact:goReplacement",
                    "value": replacement_value
                }]);
            }
            component
        })
        .collect();
    let sbom = serde_json::json!({
        "bomFormat": "CycloneDX", "specVersion": "1.4", "version": 1,
        "metadata": {
            "tools": [{"vendor":"The Go Authors","name":"go","version":go_version}],
            "component": {
                "type":"application",
                "name":"grx-go-body",
                "version":prepared.fingerprint_hex,
                "hashes":[{"alg":"SHA-256","content":artifact_sha256}]
            }
        },
        "components": components
    });
    let sbom_path = out_dir.join(SBOM);
    write_json(&sbom_path, &sbom)?;
    Ok((inventory_path, sbom_path))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CodegenError> {
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|error| module_error(error.to_string()))?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|source| CodegenError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn module_error(message: impl Into<String>) -> CodegenError {
    CodegenError::Module(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_validation_rejects_duplicates_and_non_https_proxy() {
        let duplicate = GoAuthConfig {
            version: 1,
            private_patterns: vec!["example.com/x/*".into(), "example.com/x/*".into()],
            https: Vec::new(),
            proxy: None,
        };
        assert!(validate_auth(&duplicate).is_err());
        let proxy = GoAuthConfig {
            version: 1,
            private_patterns: Vec::new(),
            https: Vec::new(),
            proxy: Some(ProxyAuth {
                url: "http://proxy.test".into(),
                username_secret: "USER".into(),
                password_secret: "TOKEN".into(),
            }),
        };
        assert!(validate_auth(&proxy).is_err());
    }

    #[test]
    fn checksum_exception_requires_every_dependency_to_have_a_local_replace() {
        let fully_local = r"
            require (
                example.test/one v0.0.0
                example.test/two v0.0.0
            )
            replace (
                example.test/one => ./one
                example.test/two => ../two
            )
        ";
        assert!(all_requires_replaced(fully_local));
        assert!(!all_requires_replaced(
            "require example.test/one v1.0.0\nreplace example.test/other => ./other"
        ));
    }

    #[test]
    fn percent_encoding_keeps_credentials_out_of_url_syntax() {
        assert_eq!(percent_encode("a:b@c/x"), "a%3Ab%40c%2Fx");
    }

    #[test]
    fn sanitized_environment_excludes_graphreact_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let env = sanitized_environment(dir.path(), &GoBuildOptions::default());
        assert!(!env
            .keys()
            .any(|key| key.to_string_lossy().starts_with("GRX_SECRET_")));
        assert_eq!(env.get(OsStr::new("GOWORK")), Some(&OsString::from("off")));
    }

    #[test]
    fn discovers_nearest_module_and_rejects_a_second_root() {
        let first = tempfile::tempdir().unwrap();
        fs::write(
            first.path().join("go.mod"),
            "module example.test/first\n\ngo 1.22\n",
        )
        .unwrap();
        let nested = first.path().join("systems/nested");
        fs::create_dir_all(&nested).unwrap();
        let entry = nested.join("main.grx");
        fs::write(&entry, "<Graph id=\"x\"></Graph>").unwrap();
        let discovered = discover_module(&entry, std::slice::from_ref(&entry))
            .unwrap()
            .unwrap();
        assert_eq!(discovered.root, fs::canonicalize(first.path()).unwrap());

        let second = tempfile::tempdir().unwrap();
        fs::write(
            second.path().join("go.mod"),
            "module example.test/second\n\ngo 1.22\n",
        )
        .unwrap();
        let other = second.path().join("body.grx");
        fs::write(&other, "<Graph id=\"y\"></Graph>").unwrap();
        assert!(discover_module(&entry, &[other]).is_err());
    }

    #[test]
    fn redacts_plain_and_url_encoded_credentials() {
        let prepared = PreparedBuild {
            fingerprint_hex: "x".into(),
            env: BTreeMap::new(),
            secret_values: vec!["tok:en@value".into()],
            _credentials: None,
        };
        let output = redact_output(&prepared, b"tok:en@value tok%3Aen%40value");
        assert_eq!(output, "[REDACTED] [REDACTED]");
    }

    #[test]
    fn credential_rotation_does_not_change_build_fingerprint() {
        let probe = Command::new("go").arg("version").output();
        if !matches!(&probe, Ok(out) if out.status.success()) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("generated");
        let output = root.path().join("output");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(source.join("grx_prelude.go"), "package main\n").unwrap();
        fs::write(source.join("dispatch.go"), "package main\n").unwrap();
        fs::write(source.join("body_test.go"), "package main\n").unwrap();
        let go_mod = root.path().join("go.mod");
        let auth_path = root.path().join("go.auth.json");
        fs::write(&go_mod, "module example.test/app\n\ngo 1.22\n").unwrap();
        fs::write(
            &auth_path,
            r#"{"version":1,"privatePatterns":["example.test/*"],"https":[{"host":"example.test","username":"token","passwordSecret":"TOKEN"}]}"#,
        )
        .unwrap();
        let module = GoModule {
            root: root.path().to_path_buf(),
            go_mod,
            go_sum: None,
            auth_path: Some(auth_path),
            auth: Some(GoAuthConfig {
                version: 1,
                private_patterns: vec!["example.test/*".into()],
                https: vec![HttpsAuth {
                    host: "example.test".into(),
                    username: "token".into(),
                    password_secret: "TOKEN".into(),
                }],
                proxy: None,
            }),
        };
        let options = |secret: &str| GoBuildOptions {
            module: Some(module.clone()),
            auth: Some(ResolvedGoAuth {
                https_passwords: BTreeMap::from([("example.test".into(), secret.into())]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let first = prepare_build("go", &source, &output, &options("first-secret")).unwrap();
        let second = prepare_build("go", &source, &output, &options("rotated-secret")).unwrap();
        assert_eq!(first.fingerprint_hex, second.fingerprint_hex);
    }
}
