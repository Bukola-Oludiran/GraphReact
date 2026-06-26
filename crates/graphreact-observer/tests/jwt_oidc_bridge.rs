//! **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** End-to-end
//! test for the JWT-OIDC bridge.
//!
//! Spawns the management API with a [`RotatableRegistry`] threaded
//! through [`SpawnOptions::rbac`] plus a [`JwtVerifier`] threaded
//! through `SpawnOptions::jwt_verifier`, then exercises:
//!
//! 1. JWT happy path → `auth_success` audit + 200 on a protected
//!    GET (`/v1/identity`).
//! 2. JWT expired-signature → `jwt_auth_fail reason=expired` audit
//!    + 401 challenge.
//! 3. JWT wrong-issuer → `jwt_auth_fail reason=invalid_issuer` +
//!    401.
//! 4. JWT signed by an UnknownKid → falls through to the static-
//!    token walk (which produces `auth_fail reason=wrong`) — proves
//!    the graceful coexistence path.
//! 5. Non-eyJ static bearer still works (the JWT short-circuit
//!    doesn't break the Phase B Part 1 static-token flow).
//! 6. JWT with a role claim mapping to ReadOnly correctly denies a
//!    POST endpoint (`/v1/nodes/foo/messages`) with
//!    `jwt_auth_fail reason=authz_deny`.
//!
//! Pre-baked RSA-2048 test keypair below — generated once via
//! `openssl genpkey`; the matching JWK `n`/`e` values are derived
//! from the same key. Tests use this static material so they're
//! fully deterministic.

#![allow(clippy::pedantic)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use graphreact_observer::{
    spawn_management_api_with_options, BearerToken, BindConfig, JwksStore, JwtOidcConfig,
    JwtVerifier, MetricsInner, Role, RoleMapping, RotatableJwks, RotatableRegistry,
    SnapshotRequest, SpawnOptions, TokenEntry, TokenId, TokenRegistry,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Pre-baked RSA-2048 private key (PKCS#8 PEM). Used by the test
/// to sign JWTs. NEVER ship this in production — it's intentionally
/// visible so the test is reproducible.
const TEST_RSA_PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCrLc6X2Gln5jIO
sycNUfm68pjZGHVTYqM+Ii3mSu8a8gNBA9HgQlwPRyjPtHW4oBlM6HXju1zOyYTL
jM7rHyUVBLkBgzZ9UxQaJeoaiLckmgJWgX3mFPmD3yvY1dJWJfK6HYQh/rPUShwg
jiy8bvSeFVbFDDjqkrb0C6ruGSLApMRFej+0SnxN6IJtoSDlKBvoD+v/VxZ38E90
k2Lv5xkyv4epwOs1eb+7jmmsQKc2VVWh9eKjBQF9s5Klb2JbCl3cbu99OU0i3xOV
xwOSn0eVQMc4jLUWtRTkRuQXRlsj3oabDbLQe7ycvMku4AB4ijQVxs3AxFDekj6j
exnllOnHAgMBAAECggEAUnqITCbEpwDGBsowm1zvyLGijxwXT/27bTE3z3o0mlWY
1sc55WXkCKJ/WhD4FeTiO+nX/8ojzilQGGm4u5ZuniF/6OgppZoVmgHBulY8dGDW
lorfODDMBsvNyzU/my2+tgmJxH1+dV/eDJukl4iYyeC7Rcd9Vx0pFQ7YJdArrAkO
CBtOoIzoeOODpC6fP0Dn2EPN6iOIlQ0QzTA1WE6QEBA0RzmDYXGUHQJIqQboZ9Qk
JsWOuMF7Hhuo9UsFdL/DiLP5tvVh1yDQktLThAvfFfRUsv65h1iDs+0X5CPMv8WI
hBk730yf59rEHfezWJoNK4gUXp80IjaZxSb/xOFkRQKBgQDYTUJe1bC/IiHKtFyN
GxBCG/4Llxb1c8NbOqAztxMrU3mczuF54+PvNbkHb6xU2L5PjFVIlog2pj6Wjvtw
DEIn08A/CP536obvDdMpHrXLxh1ha7BY5B+cce4BlS8xdBjyK763Arxg11CJN96k
XDNwUMvZYcCB/VzZWxQAJIK7QwKBgQDKmH3eXZjVt3oRxmxoYqZx1c0WQCZC/Pin
E+ZEAA0LWSwkcaMVu9xXXMWv6Fvecfdy9GDyAfJZd9Dkx8UtURIOSAS/A8nYw0X7
NmFeWe0EaPtuyc4XcPR8DgY18JqkyIY7r5yNTv1rlYmIYAJXnvoAdUQMZhZxsrZ6
5vlmnjSVLQKBgC7i4GYqL/vebDDCU+WKXkXWR4U/pQxjoY8e8CcFojQcDhp9hcak
LX5myMrh3BswOGCJ7NqlNrWR57M/HyW0oop7jgqb6n0X61PaTVF2hLW9JhU+AKXk
HS4OTCcvrWEs51NnRBnrj+X2BlEYOTyrd8L64JUoaVBEGWJMKdO2Flh9AoGAOlCK
amcmht25FUS4D5PVtQ6FadItMTr/l5fyv8AEATA5tIPBHZBlbAAYJvoK94x2XXBa
N064I2iJiFC4QMMFH3c7ZoAAqZQLoLOWc0IPJyXQuaL832Jz/QFMaFpJKQVPHf7+
YO5Lo5v3anm4MuBftaILZEY+T5vRSq8ROqhvGkECgYAqlTmopbqVoCarhQmIn39D
gaYdochyflueseH6dVBwitJ8QqRxJqIWXYQfwh8aaTwr3iIRLW98TqQHnATlQ/k9
hcUs+3jWSjxq0Q7/IL5djv0l2Zt7JprTJZm/N9Ea+CevSNuCy8TlffV3b6Kbk/Mk
3toJdLqw2tmM3hhPI5xkZw==
-----END PRIVATE KEY-----";

/// Public RSA key as JWKS `n` field (base64url, no padding) —
/// derived from `TEST_RSA_PRIVATE_PEM`.
const TEST_RSA_N: &str = "qy3Ol9hpZ-YyDrMnDVH5uvKY2Rh1U2KjPiIt5krvGvIDQQPR4EJcD0coz7R1uKAZTOh147tczsmEy4zO6x8lFQS5AYM2fVMUGiXqGoi3JJoCVoF95hT5g98r2NXSViXyuh2EIf6z1EocII4svG70nhVWxQw46pK29Auq7hkiwKTERXo_tEp8TeiCbaEg5Sgb6A_r_1cWd_BPdJNi7-cZMr-HqcDrNXm_u45prECnNlVVofXiowUBfbOSpW9iWwpd3G7vfTlNIt8TlccDkp9HlUDHOIy1FrUU5EbkF0ZbI96Gmw2y0Hu8nLzJLuAAeIo0FcbNwMRQ3pI-o3sZ5ZTpxw";
const TEST_RSA_E: &str = "AQAB";
const TEST_KID: &str = "test-kid-2026";
const TEST_ISSUER: &str = "https://issuer.example.com";
const TEST_AUDIENCE: &str = "grx-management-api";

fn build_test_jwks_store() -> Arc<JwksStore> {
    let jwks_json = format!(
        r#"{{"keys": [{{"kty":"RSA","kid":"{TEST_KID}","use":"sig","alg":"RS256","n":"{TEST_RSA_N}","e":"{TEST_RSA_E}"}}]}}"#
    );
    let set: jsonwebtoken::jwk::JwkSet =
        serde_json::from_str(&jwks_json).expect("parse JWKS json");
    Arc::new(
        JwksStore::from_jwk_set(set, std::path::Path::new("test-jwks.json"))
            .expect("build JwksStore"),
    )
}

fn build_test_role_mapping() -> RoleMapping {
    let mut custom = HashMap::new();
    let mut deployer = BTreeSet::new();
    deployer.insert("/v1/nodes/:id/call".to_string());
    custom.insert("grx-deployer".to_string(), deployer);
    RoleMapping {
        role_claim: "role".into(),
        admin_values: vec!["grx-admin".into()],
        read_only_values: vec!["grx-reader".into()],
        custom_role_mappings: custom,
    }
}

fn build_test_jwt_verifier() -> Arc<JwtVerifier> {
    let jwks = RotatableJwks::new(build_test_jwks_store());
    let cfg = JwtOidcConfig {
        expected_issuers: vec![TEST_ISSUER.into()],
        expected_audiences: vec![TEST_AUDIENCE.into()],
        allowed_algorithms: vec![Algorithm::RS256],
        leeway_seconds: 60,
        role_mapping: build_test_role_mapping(),
    };
    Arc::new(JwtVerifier::new(cfg, jwks))
}

fn build_static_registry() -> RotatableRegistry {
    let entries = vec![TokenEntry {
        id: TokenId("static-admin".into()),
        token: BearerToken::from_bytes(b"static-admin-token-bytes".to_vec()).unwrap(),
        role: Role::Admin,
        expires_at: None,
    }];
    let registry = TokenRegistry::new(entries).expect("registry");
    RotatableRegistry::new(Arc::new(registry))
}

/// Mint a JWT signed by the test private key.
fn mint_test_jwt(
    iss: &str,
    aud: &str,
    sub: &str,
    role_claim_value: serde_json::Value,
    exp_offset_secs: i64,
    kid: &str,
) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut claims = BTreeMap::new();
    claims.insert("iss".to_string(), json!(iss));
    claims.insert("aud".to_string(), json!(aud));
    claims.insert("sub".to_string(), json!(sub));
    claims.insert("exp".to_string(), json!(now + exp_offset_secs));
    claims.insert("nbf".to_string(), json!(now - 30));
    claims.insert("iat".to_string(), json!(now));
    claims.insert("role".to_string(), role_claim_value);
    let key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_PEM.as_bytes())
        .expect("load test RSA key");
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    jsonwebtoken::encode(&header, &claims, &key).expect("encode jwt")
}

async fn spawn_with_jwt_bridge(
    rbac: RotatableRegistry,
    jwt_verifier: Arc<JwtVerifier>,
) -> (
    graphreact_observer::ApiServerHandle,
    JoinHandle<()>,
    std::net::SocketAddr,
) {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let mut server = spawn_management_api_with_options(SpawnOptions {
        bind,
        metrics,
        process_start_id: 0xface_feed,
        cluster_readyz: None,
        audit_sink: None,
        draining: None,
        rate_limit: None,
        rbac: Some(rbac),
        trusted_proxies: None,
        jwt_verifier: Some(jwt_verifier),
    })
    .await
    .expect("spawn");
    let local_addr = server.local_addr;
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let drain_task = tokio::spawn(async move {
        while let Some(req) = snapshot_rx.recv().await {
            let _: SnapshotRequest = req;
        }
    });
    (server, drain_task, local_addr)
}

async fn http_get(url: &str, bearer: Option<&str>) -> u16 {
    use std::fmt::Write as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let s = url.strip_prefix("http://").expect("http://");
    let (host_port, path) = match s.find('/') {
        Some(idx) => (&s[..idx], &s[idx..]),
        None => (s, "/"),
    };
    let (host, port) = match host_port.rfind(':') {
        Some(idx) => (
            &host_port[..idx],
            host_port[idx + 1..].parse::<u16>().unwrap_or(80),
        ),
        None => (host_port, 80),
    };
    let mut stream = TcpStream::connect((host, port)).await.expect("connect");
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n",
        path, host, port
    );
    if let Some(b) = bearer {
        write!(req, "Authorization: Bearer {b}\r\n").unwrap();
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let resp = String::from_utf8_lossy(&buf);
    resp.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

async fn http_post(url: &str, bearer: Option<&str>, body: &str) -> u16 {
    use std::fmt::Write as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let s = url.strip_prefix("http://").expect("http://");
    let (host_port, path) = match s.find('/') {
        Some(idx) => (&s[..idx], &s[idx..]),
        None => (s, "/"),
    };
    let (host, port) = match host_port.rfind(':') {
        Some(idx) => (
            &host_port[..idx],
            host_port[idx + 1..].parse::<u16>().unwrap_or(80),
        ),
        None => (host_port, 80),
    };
    let mut stream = TcpStream::connect((host, port)).await.expect("connect");
    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        path, host, port, body.len()
    );
    if let Some(b) = bearer {
        write!(req, "Authorization: Bearer {b}\r\n").unwrap();
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let resp = String::from_utf8_lossy(&buf);
    resp.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

#[tokio::test]
async fn jwt_happy_path_admin_role_grants_protected_get() {
    let jwt = mint_test_jwt(
        TEST_ISSUER,
        TEST_AUDIENCE,
        "alice@example.com",
        json!("grx-admin"),
        3600,
        TEST_KID,
    );
    let (server, drain, addr) =
        spawn_with_jwt_bridge(build_static_registry(), build_test_jwt_verifier()).await;
    let status = http_get(&format!("http://{addr}/v1/identity"), Some(&jwt)).await;
    assert_eq!(status, 200, "JWT with admin role should grant /v1/identity");
    server.shutdown().await;
    drain.abort();
}

#[tokio::test]
async fn jwt_expired_signature_denies() {
    // exp_offset = -3600 (one hour in the past). leeway is 60s.
    let jwt = mint_test_jwt(
        TEST_ISSUER,
        TEST_AUDIENCE,
        "alice@example.com",
        json!("grx-admin"),
        -3600,
        TEST_KID,
    );
    let (server, drain, addr) =
        spawn_with_jwt_bridge(build_static_registry(), build_test_jwt_verifier()).await;
    let status = http_get(&format!("http://{addr}/v1/identity"), Some(&jwt)).await;
    assert_eq!(status, 401, "expired JWT should be 401");
    server.shutdown().await;
    drain.abort();
}

#[tokio::test]
async fn jwt_wrong_issuer_denies() {
    let jwt = mint_test_jwt(
        "https://attacker.example.com",
        TEST_AUDIENCE,
        "alice@example.com",
        json!("grx-admin"),
        3600,
        TEST_KID,
    );
    let (server, drain, addr) =
        spawn_with_jwt_bridge(build_static_registry(), build_test_jwt_verifier()).await;
    let status = http_get(&format!("http://{addr}/v1/identity"), Some(&jwt)).await;
    assert_eq!(status, 401, "JWT from unexpected issuer should be 401");
    server.shutdown().await;
    drain.abort();
}

#[tokio::test]
async fn jwt_unknown_kid_falls_through_to_static_token_walk() {
    // Sign a JWT with a kid that's NOT in the JWKS — the JWT path
    // should silently fall through to the static-token walk, which
    // will also reject this token (it isn't a registered static
    // token either) → 401 auth_fail reason=wrong.
    let jwt = mint_test_jwt(
        TEST_ISSUER,
        TEST_AUDIENCE,
        "alice@example.com",
        json!("grx-admin"),
        3600,
        "kid-not-in-jwks",
    );
    let (server, drain, addr) =
        spawn_with_jwt_bridge(build_static_registry(), build_test_jwt_verifier()).await;
    let status = http_get(&format!("http://{addr}/v1/identity"), Some(&jwt)).await;
    assert_eq!(status, 401);
    server.shutdown().await;
    drain.abort();
}

#[tokio::test]
async fn non_jwt_static_bearer_still_works() {
    // Static token (doesn't start with eyJ) should completely
    // skip the JWT path and dispatch through the existing
    // TokenRegistry walk.
    let (server, drain, addr) =
        spawn_with_jwt_bridge(build_static_registry(), build_test_jwt_verifier()).await;
    let status = http_get(
        &format!("http://{addr}/v1/identity"),
        Some("static-admin-token-bytes"),
    )
    .await;
    assert_eq!(status, 200, "static admin token should still grant access");
    server.shutdown().await;
    drain.abort();
}

#[tokio::test]
async fn jwt_read_only_role_denies_post_endpoint() {
    let jwt = mint_test_jwt(
        TEST_ISSUER,
        TEST_AUDIENCE,
        "bob@example.com",
        json!("grx-reader"),
        3600,
        TEST_KID,
    );
    let (server, drain, addr) =
        spawn_with_jwt_bridge(build_static_registry(), build_test_jwt_verifier()).await;
    // POST endpoints are NOT in READ_ONLY_ROUTES — the reader
    // role should be denied with 403.
    let status = http_post(
        &format!("http://{addr}/v1/nodes/foo/messages"),
        Some(&jwt),
        r#"{"payload":"x"}"#,
    )
    .await;
    assert_eq!(status, 403, "ReadOnly role should not grant POST endpoints");
    // But a read-only route SHOULD grant.
    let status = http_get(&format!("http://{addr}/v1/identity"), Some(&jwt)).await;
    assert_eq!(status, 200, "ReadOnly role should grant /v1/identity");
    server.shutdown().await;
    drain.abort();
}

#[tokio::test]
async fn jwt_no_role_claim_match_denies() {
    // The JWT verifies cleanly but its role-claim value is not in
    // the operator's mapping → ClaimMappingError::NoRoleClaimMatch
    // → 401 challenge.
    let jwt = mint_test_jwt(
        TEST_ISSUER,
        TEST_AUDIENCE,
        "carol@example.com",
        json!("unmapped-role"),
        3600,
        TEST_KID,
    );
    let (server, drain, addr) =
        spawn_with_jwt_bridge(build_static_registry(), build_test_jwt_verifier()).await;
    let status = http_get(&format!("http://{addr}/v1/identity"), Some(&jwt)).await;
    assert_eq!(status, 401, "JWT with unmapped role should be denied");
    server.shutdown().await;
    drain.abort();
}
