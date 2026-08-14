//! Repeated real-daemon failover acceptance for Phase A durability.
//!
//! This test is opt-in because it requires the same disposable Postgres and
//! MinIO services as `graphreact-persistence/tests/cluster_services.rs`, plus a
//! working Go toolchain. It deliberately suspends the active process with
//! SIGSTOP so the former owner remains alive while its lease expires. After the
//! standby takes over, SIGCONT proves the old owner observes fencing and exits
//! before it can drive another scheduler batch.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;
use graphreact_persistence::DeploymentId;

const TAKEOVER_DEADLINE: Duration = Duration::from_secs(30);
const FORMER_OWNER_EXIT_DEADLINE: Duration = Duration::from_secs(10);
const REPEATED_FAILOVER_CYCLES: usize = 3;

const GO_GRAPH: &str = r#"
<Counter lang="go">
func CounterBody(ctx Ctx) Manifest {
    return Manifest{
        ID:      ctx.ID,
        Accepts: []string{},
        Emits:   []string{},
    }
}
</Counter>

<Graph id="cluster-failover">
  <Counter id="counter-1" />
</Graph>
"#;

struct PemArtifacts {
    _tempdir: tempfile::TempDir,
    active_cert: PathBuf,
    active_key: PathBuf,
    standby_cert: PathBuf,
    standby_key: PathBuf,
    ca: PathBuf,
}

fn write_pem_artifacts() -> PemArtifacts {
    let directory = tempfile::tempdir().expect("certificate tempdir");
    let ca_key = rcgen::KeyPair::generate().expect("CA key");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["grx-failover-test-ca".into()]).expect("CA params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).expect("CA certificate");
    let ca_path = directory.path().join("ca.pem");
    std::fs::write(&ca_path, ca_cert.pem()).expect("write CA");

    let leaf = |name: &str| {
        let key = rcgen::KeyPair::generate().expect("leaf key");
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).expect("leaf params");
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            name.to_owned().try_into().expect("DNS name"),
        ));
        let cert = params
            .signed_by(&key, &ca_cert, &ca_key)
            .expect("leaf certificate");
        let cert_path = directory.path().join(format!("{name}-cert.pem"));
        let key_path = directory.path().join(format!("{name}-key.pem"));
        std::fs::write(&cert_path, cert.pem()).expect("write leaf certificate");
        std::fs::write(&key_path, key.serialize_pem()).expect("write leaf key");
        (cert_path, key_path)
    };
    let (active_cert, active_key) = leaf("active");
    let (standby_cert, standby_key) = leaf("standby");
    PemArtifacts {
        _tempdir: directory,
        active_cert,
        active_key,
        standby_cert,
        standby_key,
        ca: ca_path,
    }
}

struct DaemonProcess {
    child: Child,
    ready: mpsc::Receiver<String>,
    stderr: Arc<Mutex<String>>,
    stopped: bool,
}

impl DaemonProcess {
    fn spawn(
        replica_id: &str,
        node_name: &str,
        graph: &Path,
        deployment_id: &DeploymentId,
        certificate: &Path,
        key: &Path,
        ca: &Path,
    ) -> Self {
        let binary = StdCommand::cargo_bin("grx").expect("locate grx binary");
        let database_url = required_env("GRX_CLUSTER_TEST_DATABASE_URL");
        let bucket = required_env("GRX_CLUSTER_TEST_S3_BUCKET");
        let endpoint = required_env("GRX_CLUSTER_TEST_S3_ENDPOINT");
        let region =
            std::env::var("GRX_CLUSTER_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
        let mut child = StdCommand::new(binary.get_program())
            .args([
                "cluster",
                "daemon",
                "--path",
                graph.to_str().expect("UTF-8 graph path"),
                "--durability",
                "cluster",
                "--deployment-id",
                deployment_id.as_str(),
                "--replica-id",
                replica_id,
                "--node-id",
                &format!("{node_name}@127.0.0.1"),
                "--listen",
                "127.0.0.1:0",
                "--cookie-env",
                "GRX_FAILOVER_COOKIE",
                "--cert",
            ])
            .arg(certificate)
            .arg("--key")
            .arg(key)
            .arg("--ca")
            .arg(ca)
            .args(["--runtime", "go"])
            .env("GRX_FAILOVER_COOKIE", "phase-a-failover-cookie")
            .env("GRX_PERSISTENCE_DATABASE_URL", database_url)
            .env("GRX_PERSISTENCE_S3_BUCKET", bucket)
            .env("GRX_PERSISTENCE_S3_ENDPOINT", endpoint)
            .env("GRX_PERSISTENCE_S3_REGION", region)
            .env(
                "GRX_PERSISTENCE_S3_PREFIX",
                format!("daemon-failover/{}", deployment_id.as_str()),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn durable daemon");

        let stdout = child.stdout.take().expect("daemon stdout");
        let (ready_tx, ready) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.starts_with("grx-daemon-ready ") {
                    let _ = ready_tx.send(line);
                }
            }
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_capture = Arc::clone(&stderr);
        let stderr_pipe = child.stderr.take().expect("daemon stderr");
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                let mut captured = stderr_capture.lock().expect("stderr capture lock");
                captured.push_str(&line);
                captured.push('\n');
            }
        });

        Self {
            child,
            ready,
            stderr,
            stopped: false,
        }
    }

    fn wait_ready(&self, deadline: Duration) -> String {
        self.ready.recv_timeout(deadline).unwrap_or_else(|error| {
            panic!(
                "daemon did not become ready within {deadline:?}: {error}; stderr: {}",
                self.stderr.lock().expect("stderr capture lock")
            )
        })
    }

    fn assert_standby(&self, observation: Duration) {
        match self.ready.recv_timeout(observation) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                "standby exited before takeover; stderr: {}",
                self.stderr.lock().expect("stderr capture lock")
            ),
            Ok(line) => panic!("standby became active before lease loss: {line}"),
        }
    }

    fn signal(&mut self, signal: &str) {
        let status = StdCommand::new("kill")
            .args([signal, &self.child.id().to_string()])
            .status()
            .expect("invoke kill");
        assert!(status.success(), "{signal} daemon process");
        self.stopped = signal == "-STOP";
    }

    fn wait_for_exit(&mut self, deadline: Duration) -> ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("poll daemon exit") {
                return status;
            }
            assert!(
                started.elapsed() < deadline,
                "daemon did not exit within {deadline:?}; stderr: {}",
                self.stderr.lock().expect("stderr capture lock")
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        if self.stopped {
            let _ = StdCommand::new("kill")
                .args(["-CONT", &self.child.id().to_string()])
                .status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for daemon failover tests"))
}

fn graph_copy(root: &Path, name: &str) -> PathBuf {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).expect("create graph directory");
    let graph = directory.join("failover.grx");
    std::fs::write(&graph, GO_GRAPH).expect("write graph fixture");
    graph
}

#[test]
#[ignore = "requires GRX_CLUSTER_TEST_* services and a Go toolchain"]
fn suspended_active_is_fenced_after_repeated_takeovers() {
    let pem = write_pem_artifacts();
    let graphs = tempfile::tempdir().expect("graph tempdir");

    for cycle in 0..REPEATED_FAILOVER_CYCLES {
        let deployment_id = DeploymentId::new();
        let active_graph = graph_copy(graphs.path(), &format!("cycle-{cycle}-active"));
        let standby_graph = graph_copy(graphs.path(), &format!("cycle-{cycle}-standby"));
        let mut active = DaemonProcess::spawn(
            &format!("replica-a-{cycle}"),
            "active",
            &active_graph,
            &deployment_id,
            &pem.active_cert,
            &pem.active_key,
            &pem.ca,
        );
        active.wait_ready(TAKEOVER_DEADLINE);

        let mut standby = DaemonProcess::spawn(
            &format!("replica-b-{cycle}"),
            "standby",
            &standby_graph,
            &deployment_id,
            &pem.standby_cert,
            &pem.standby_key,
            &pem.ca,
        );
        standby.assert_standby(Duration::from_secs(2));

        active.signal("-STOP");
        let takeover_started = Instant::now();
        standby.wait_ready(TAKEOVER_DEADLINE);
        let takeover_elapsed = takeover_started.elapsed();
        assert!(
            takeover_elapsed < TAKEOVER_DEADLINE,
            "cycle {cycle} exceeded the 30-second takeover objective"
        );

        active.signal("-CONT");
        let former_owner_exit_started = Instant::now();
        let former_owner_status = active.wait_for_exit(FORMER_OWNER_EXIT_DEADLINE);
        let former_owner_exit_elapsed = former_owner_exit_started.elapsed();
        assert!(
            !former_owner_status.success(),
            "cycle {cycle} former owner exited successfully instead of reporting lease loss"
        );
        let active_stderr = active.stderr.lock().expect("stderr capture lock").clone();
        assert!(
            active_stderr.contains("cluster durability lease lost"),
            "cycle {cycle} former owner did not report fencing: {active_stderr}"
        );
        eprintln!(
            "failover cycle {cycle}: takeover={takeover_elapsed:?}, former_owner_exit={former_owner_exit_elapsed:?}"
        );

        standby.signal("-TERM");
        let standby_status = standby.wait_for_exit(FORMER_OWNER_EXIT_DEADLINE);
        assert!(
            standby_status.success(),
            "cycle {cycle} standby did not shut down cleanly: {}",
            standby.stderr.lock().expect("stderr capture lock")
        );
    }
}
