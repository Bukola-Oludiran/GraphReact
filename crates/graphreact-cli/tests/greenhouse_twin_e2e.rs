use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::Duration;

fn call(
    addr: SocketAddr,
    request_type: &str,
    reply_type: &str,
    payload: serde_json::Value,
) -> (u16, serde_json::Value) {
    try_call(addr, request_type, reply_type, payload).expect("call observer")
}

fn try_call(
    addr: SocketAddr,
    request_type: &str,
    reply_type: &str,
    payload: serde_json::Value,
) -> std::io::Result<(u16, serde_json::Value)> {
    let body = serde_json::json!({
        "type": request_type,
        "reply_type": reply_type,
        "payload": payload,
        "timeout_ms": 2_000,
    })
    .to_string();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    write!(stream, "POST /v1/nodes/twin-gateway/call HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let value = serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({"raw": body}));
    Ok((status, value))
}

fn call_when_ready(
    addr: SocketAddr,
    request_type: &str,
    reply_type: &str,
    payload: serde_json::Value,
) -> (u16, serde_json::Value) {
    for _ in 0..50 {
        if let Ok(response) = try_call(addr, request_type, reply_type, payload.clone()) {
            if response.0 != 503 {
                return response;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    call(addr, request_type, reply_type, payload)
}

#[test]
fn stuck_irrigation_is_contained_through_real_go_runtime_and_ingress() {
    let binary = assert_cmd::cargo::cargo_bin!("grx");
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cache = tempfile::tempdir().unwrap();
    let observer_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let observer_bind = format!("127.0.0.1:{observer_port}");
    let mut child = Command::new(binary)
        .current_dir(&workspace)
        .env("GOCACHE", cache.path())
        .args([
            "cluster",
            "daemon",
            "--dev-loopback",
            "--path",
            "examples/greenhouse-twin/greenhouse.grx",
            "--runtime",
            "go",
            "--observer-bind",
            &observer_bind,
            "--duration-secs",
            "30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn greenhouse daemon");

    let stdout = child.stdout.take().unwrap();
    let mut stdout = BufReader::new(stdout);
    let mut observer = None;
    loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let line = line.trim_end();
        if let Some(value) = line.strip_prefix("grx-observer-ready ") {
            observer = Some(value.parse::<SocketAddr>().unwrap());
            break;
        }
    }
    let _stdout_drain = std::thread::spawn(move || {
        for line in stdout.lines() {
            let _ = line;
        }
    });
    let addr = observer.unwrap_or_else(|| {
        let _ = child.kill();
        panic!("daemon never reported observer readiness")
    });
    std::thread::sleep(Duration::from_millis(500));
    if let Some(status) = child.try_wait().expect("daemon status") {
        panic!("daemon exited after readiness ({status})");
    }

    let (status, _) = call_when_ready(
        addr,
        "set_scenario",
        "scenario_set",
        serde_json::json!({"scenario": "stuck_irrigation"}),
    );
    assert_eq!(status, 200);
    for _ in 0..5 {
        let (status, _) = call(addr, "step", "step_complete", serde_json::Value::Null);
        assert_eq!(status, 200);
    }
    let (status, snapshot) = call(
        addr,
        "get_snapshot",
        "twin_snapshot",
        serde_json::Value::Null,
    );
    assert_eq!(status, 200);
    let payload = &snapshot["payload"];
    assert_eq!(payload["outcome"], "contained");
    assert_eq!(payload["nodes"]["irrigation-valve"]["open"], false);
    assert_eq!(payload["nodes"]["irrigation-valve"]["locked_out"], true);
    assert_eq!(payload["nodes"]["safety-interlock"]["status"], "tripped");
    assert_eq!(payload["nodes"]["alarm"]["active"], true);
    assert_eq!(payload["risk"]["affected_asset"], "crop-zone-a");

    let (status, _) = call(addr, "undeclared", "nope", serde_json::Value::Null);
    assert_eq!(status, 403);
    let (status, _) = call(addr, "reset", "reset_complete", serde_json::Value::Null);
    assert_eq!(status, 200);

    let _ = child.kill();
    let _ = child.wait();
}
