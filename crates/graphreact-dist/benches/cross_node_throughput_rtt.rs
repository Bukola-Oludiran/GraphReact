//! M14.8.x-followon (Phase D) — cross-node throughput at 1ms RTT.
//!
//! Sister bench to `cross_node_throughput.rs` (loopback). Wraps the
//! `tokio::io::duplex` pair in a `DelayedDuplex` shim that injects
//! per-frame transit delay. Plugs into `Cluster::accept_stream` /
//! `Cluster::connect_with_stream` (added in Phase A) — production
//! `Cluster::start` / `Cluster::connect` keep their TCP-only path
//! untouched.
//!
//! ## RTT model
//!
//! Each direction's `poll_flush` sleeps `ONE_WAY_DELAY` (default
//! 500µs) once per frame before the inner duplex flushes. Two
//! one-way trips = ~1ms round-trip. Reads are immediate (the byte
//! buffer is already in the duplex inner once flush returns).
//!
//! Per-frame delay caps single-stream throughput at
//! `1 / ONE_WAY_DELAY ≈ 2k frames/sec`. The M14 plan's `≥10k @ 1ms
//! RTT` threshold is achievable on real TCP because TCP pipelines
//! bytes asynchronously; the dist crate's framed-codec writer is
//! serial-per-peer today, so this bench reports the
//! architecture-imposed ceiling. **Documented as the substrate's
//! current shape; lifting it is sub-followon work** (multi-frame
//! pipelining at the writer task or per-direction parallel
//! send-channels).
//!
//! Acceptance for Phase D: bench compiles, runs, and reports a
//! number (no perf-floor gate).
//!
//! ```bash
//! cargo bench -p graphreact-dist --bench cross_node_throughput_rtt
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use graphreact_core::runtime::hooks;
use graphreact_core::{
    graph::{Attrs, CommunicationGraph},
    NodeDefinition, Scheduler,
};
use graphreact_dist::{
    Cluster, ClusterConfig, Creation, NetSplitPolicy, NetTickConfig, NodeId, SecretCookie,
    TlsConfig,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::runtime::Runtime;
use tokio::time::Sleep;

const ONE_WAY_DELAY: Duration = Duration::from_micros(500);
const DUPLEX_BUF_SIZE: usize = 256 * 1024;

/// `AsyncRead + AsyncWrite` shim wrapping a `DuplexStream` with a
/// per-frame `poll_flush` delay. Reads pass through immediately —
/// the bytes are already buffered in the inner duplex once flush
/// returns from the writer side.
struct DelayedDuplex {
    inner: DuplexStream,
    delay: Duration,
    pending_flush_delay: Option<Pin<Box<Sleep>>>,
}

impl DelayedDuplex {
    fn new(inner: DuplexStream, delay: Duration) -> Self {
        Self {
            inner,
            delay,
            pending_flush_delay: None,
        }
    }
}

impl AsyncRead for DelayedDuplex {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for DelayedDuplex {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, data)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let me = self.get_mut();
        if me.pending_flush_delay.is_none() {
            me.pending_flush_delay = Some(Box::pin(tokio::time::sleep(me.delay)));
        }
        if let Some(sleep) = me.pending_flush_delay.as_mut() {
            match sleep.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    me.pending_flush_delay = None;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
        Pin::new(&mut me.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn make_payload() -> serde_json::Value {
    let blob: String = "x".repeat(992);
    serde_json::json!({"n": 0u64, "blob": blob})
}

fn pair_tls(name_a: &str, name_b: &str) -> (TlsConfig, TlsConfig) {
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["graphreact-dist rtt CA".into()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_der: CertificateDer<'static> = ca_cert.der().clone();
    let make_leaf = |name: &str| -> TlsConfig {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).expect("leaf params");
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            name.to_string().try_into().expect("san name"),
        ));
        let kp = rcgen::KeyPair::generate().expect("leaf keygen");
        let cert = params.signed_by(&kp, &ca_cert, &ca_kp).expect("leaf sign");
        let cert_der: CertificateDer<'static> = cert.der().clone();
        let key_der: PrivateKeyDer<'static> =
            PrivatePkcs8KeyDer::from(kp.serialize_der()).into();
        TlsConfig::from_pem_bytes(vec![cert_der], key_der, vec![ca_der.clone()], true)
            .expect("tls from pem bytes")
    };
    (make_leaf(name_a), make_leaf(name_b))
}

struct Setup {
    rt: Runtime,
    alice_cluster: Cluster,
    bob_cluster: Cluster,
    alice_scheduler: Scheduler,
    bob_scheduler: Scheduler,
    received: Arc<Mutex<u64>>,
}

fn setup() -> Setup {
    let rt = Runtime::new().expect("tokio runtime");
    let _g = rt.enter();
    let alice_id: NodeId = "alice@127.0.0.1".parse().unwrap();
    let bob_id: NodeId = "bob@127.0.0.1".parse().unwrap();
    let (alice_tls, bob_tls) = pair_tls("alice", "bob");
    let cookie_bytes = b"rtt-cookie".to_vec();

    let received = Arc::new(Mutex::new(0u64));
    let received_for_bob = Arc::clone(&received);
    let mut bob_graph = CommunicationGraph::new();
    bob_graph.add_node("responder".to_string(), Attrs::new());
    let mut bob_scheduler = Scheduler::new(bob_graph);
    let responder_def = NodeDefinition::new("responder", move |_ctx| {
        let received = Arc::clone(&received_for_bob);
        hooks::use_message("bench", move |_msg| {
            *received.lock().unwrap() += 1;
        });
        graphreact_core::runtime::types::PermissionManifest::new(
            vec!["bench".to_string()],
            Vec::<String>::new(),
        )
    });
    bob_scheduler
        .register_node(responder_def, "responder", Attrs::new(), 0)
        .expect("register responder");

    let bob_cfg = ClusterConfig {
        node_id: bob_id.clone(),
        creation: Creation(1),
        // Listen address is unused under the duplex-injected
        // accept path — Cluster::start still binds (any free
        // ephemeral port is fine; we never dial it).
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        cookie: SecretCookie::from_bytes(cookie_bytes.clone()).unwrap(),
        tls: bob_tls.clone(),
        capabilities: vec!["bench-rtt-v1".into()],
        net_tick: NetTickConfig::default(),
        net_split_policy: NetSplitPolicy::default(),
    };
    let bob_cluster = rt
        .block_on(Cluster::start(bob_cfg, &mut bob_scheduler))
        .expect("bob cluster start");

    let mut alice_graph = CommunicationGraph::new();
    alice_graph.add_node("pinger".to_string(), Attrs::new());
    let mut alice_scheduler = Scheduler::new(alice_graph);
    let pinger_def = NodeDefinition::new("pinger", |_ctx| {
        graphreact_core::runtime::types::PermissionManifest::new(
            Vec::<String>::new(),
            vec!["bench".to_string()],
        )
    });
    alice_scheduler
        .register_node(pinger_def, "pinger", Attrs::new(), 0)
        .expect("register pinger");

    let alice_cfg = ClusterConfig {
        node_id: alice_id.clone(),
        creation: Creation(2),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        cookie: SecretCookie::from_bytes(cookie_bytes).unwrap(),
        tls: alice_tls,
        capabilities: vec!["bench-rtt-v1".into()],
        net_tick: NetTickConfig::default(),
        net_split_policy: NetSplitPolicy::default(),
    };
    let alice_cluster = rt
        .block_on(Cluster::start(alice_cfg, &mut alice_scheduler))
        .expect("alice cluster start");

    // Build the duplex pair, wrap each side in DelayedDuplex,
    // then plug into accept_stream + connect_with_stream from
    // Phase A. The TLS handshake runs through the delay shim
    // too — that adds setup-time latency but doesn't affect
    // steady-state throughput.
    let (alice_side, bob_side) = tokio::io::duplex(DUPLEX_BUF_SIZE);
    let alice_delayed = DelayedDuplex::new(alice_side, ONE_WAY_DELAY);
    let bob_delayed = DelayedDuplex::new(bob_side, ONE_WAY_DELAY);

    rt.block_on(async {
        // Both sides drive their handshake concurrently via
        // join! — accept_stream and connect_with_stream both
        // await the handshake exchange, so they need to run in
        // parallel on the same task. (Spawning would require a
        // static-lifetime borrow which we can't satisfy
        // without an `Arc<Cluster>`-style refactor; join is
        // simpler and equivalent.)
        let (alice_res, bob_res) = tokio::join!(
            alice_cluster.connect_with_stream(alice_delayed, bob_id.clone()),
            bob_cluster.accept_stream(bob_delayed),
        );
        alice_res.expect("alice → bob duplex connect");
        bob_res.expect("bob accept");
        // Wait for both sides to register the peer (handshake
        // finishes before promote_connection_* spawns the
        // task; one extra yield lets the registry insert
        // settle).
        for _ in 0..50 {
            if alice_cluster.registry().get_by_name("bob").is_some()
                && bob_cluster.registry().get_by_name("alice").is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("rtt-bench peer never registered post-handshake");
    });

    Setup {
        rt,
        alice_cluster,
        bob_cluster,
        alice_scheduler,
        bob_scheduler,
        received,
    }
}

fn run_batch(setup: &mut Setup, batch_size: u64, payload: &serde_json::Value) {
    *setup.received.lock().unwrap() = 0;
    let peer = setup
        .alice_cluster
        .registry()
        .get_by_name("bob")
        .expect("bob peer");
    let payload_bytes = serde_json::to_vec(payload).expect("payload encode");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);

    setup.rt.block_on(async {
        for n in 0..batch_size {
            let wire = graphreact_dist::wire::Message {
                id: format!("rtt-{n}"),
                msg_type: "bench".to_string(),
                from: "pinger@alice".to_string(),
                to: "responder@bob".to_string(),
                payload: payload_bytes.clone(),
                ttl: 10,
                timestamp: now_ms,
                direct: false,
                inherited_ttl: None,
                trace_context: None,
            };
            peer.send_tx
                .send(graphreact_dist::Packet::Message(wire))
                .await
                .expect("queue Packet::Message");
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        setup.bob_scheduler.process_batch().expect("bob batch");
        let n = *setup.received.lock().unwrap();
        if n >= batch_size {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "rtt-bench timed out: received {n}/{batch_size} after 60s"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    black_box(*setup.received.lock().unwrap());
}

fn bench_rtt_1k_payload(c: &mut Criterion) {
    let payload = make_payload();
    let mut group = c.benchmark_group("cross_node_throughput_rtt");

    // Smaller batch sizes than the loopback bench because per-frame
    // poll_flush delay caps single-writer throughput at ≈ 1 / ONE_WAY_DELAY.
    for &batch_size in &[100u64, 500, 1_000] {
        group.throughput(Throughput::Elements(batch_size));
        group.bench_function(format!("rtt_1ms_tls_1kb/{batch_size}"), |b| {
            b.iter_batched(
                setup,
                |mut s| {
                    run_batch(&mut s, batch_size, &payload);
                    s.alice_cluster.shutdown();
                    s.bob_cluster.shutdown();
                    drop(s.alice_scheduler);
                    drop(s.bob_scheduler);
                    drop(s.rt);
                },
                BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_rtt_1k_payload);
criterion_main!(benches);
