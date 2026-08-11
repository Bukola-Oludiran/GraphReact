//! **M16-Backend P2 verification gate.** When a token is configured
//! on `BindConfig`, `/metrics` requires `Authorization: Bearer <token>`
//! and returns 401 + `WWW-Authenticate` for missing/wrong tokens.
//! `BearerToken::matches` uses `subtle::ConstantTimeEq` so the
//! prefix-length timing leak the README of `subtle` warns about
//! is closed.

use std::sync::Arc;

use graphreact_observer::{spawn_http_server, BearerToken, BindConfig, MetricsInner};

#[tokio::test]
async fn missing_authorization_returns_401_with_challenge() {
    let metrics = Arc::new(MetricsInner::new());
    let token = BearerToken::from_bytes(b"correct-horse-battery-staple".to_vec()).unwrap();
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: Some(token),
        tls: None,
    };
    let handle = spawn_http_server(bind, metrics).await.expect("spawn");

    let (status, headers, _body) =
        http_request(&format!("http://{}/metrics", handle.local_addr), None).await;
    assert_eq!(status, 401);
    let auth_header = headers
        .lines()
        .find_map(|l| l.strip_prefix("WWW-Authenticate: "))
        .or_else(|| {
            headers
                .lines()
                .find_map(|l| l.strip_prefix("www-authenticate: "))
        })
        .expect("missing WWW-Authenticate header");
    assert!(
        auth_header.starts_with("Bearer "),
        "expected Bearer challenge, got {auth_header}"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn wrong_token_returns_401() {
    let metrics = Arc::new(MetricsInner::new());
    let token = BearerToken::from_bytes(b"the-real-token".to_vec()).unwrap();
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: Some(token),
        tls: None,
    };
    let handle = spawn_http_server(bind, metrics).await.expect("spawn");

    let (status, _headers, _body) = http_request(
        &format!("http://{}/metrics", handle.local_addr),
        Some("Bearer not-the-real-token"),
    )
    .await;
    assert_eq!(status, 401);

    handle.shutdown().await;
}

#[tokio::test]
async fn correct_token_returns_200() {
    let metrics = Arc::new(MetricsInner::new());
    let token = BearerToken::from_bytes(b"shibboleth-2027".to_vec()).unwrap();
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: Some(token),
        tls: None,
    };
    let handle = spawn_http_server(bind, metrics).await.expect("spawn");

    let (status, _headers, body) = http_request(
        &format!("http://{}/metrics", handle.local_addr),
        Some("Bearer shibboleth-2027"),
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        body.contains("# TYPE grx_node_mount_total counter"),
        "expected Prometheus exposition body; got {body}"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn healthz_does_not_require_auth_even_when_token_configured() {
    // Liveness probe MUST work without ever shipping the token to
    // every load-balancer in the deployment.
    let metrics = Arc::new(MetricsInner::new());
    let token = BearerToken::from_bytes(b"live-only-secret".to_vec()).unwrap();
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: Some(token),
        tls: None,
    };
    let handle = spawn_http_server(bind, metrics).await.expect("spawn");

    let (status, _headers, body) =
        http_request(&format!("http://{}/healthz", handle.local_addr), None).await;
    assert_eq!(status, 200);
    assert_eq!(body, "ok");

    handle.shutdown().await;
}

/// Bare HTTP/1.1 GET with optional Authorization header. Returns
/// (status, raw header block, body).
async fn http_request(url: &str, authorization: Option<&str>) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let after_scheme = url.strip_prefix("http://").expect("http:// scheme");
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(hp, p)| (hp, format!("/{p}")))
        .unwrap_or((after_scheme, "/".into()));

    let mut stream = TcpStream::connect(host_port).await.expect("tcp connect");
    let auth_line = match authorization {
        Some(a) => format!("Authorization: {a}\r\n"),
        None => String::new(),
    };
    let req =
        format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n{auth_line}\r\n");
    stream.write_all(req.as_bytes()).await.expect("send req");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let response = String::from_utf8(response).expect("utf-8");
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP/1.1 framing missing CRLF CRLF");
    let status_line = head.lines().next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("u16 status");
    (status, head.to_string(), body.to_string())
}
