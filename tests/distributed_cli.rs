#![cfg(feature = "distributed-transport")]

mod common;

use common::CommandTimeout;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Process(Option<Child>);

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Process {
    #[cfg(unix)]
    fn finish(mut self) -> Output {
        let child = self.0.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "distributed CLI did not stop");
            thread::sleep(Duration::from_millis(10));
        }
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/grunt-tls")
        .join(name)
}

fn identity(command: &mut Command, name: &str) {
    command
        .arg("--ca")
        .arg(fixture("ca.der"))
        .arg("--cert")
        .arg(fixture(&format!("{name}.der")))
        .arg("--key")
        .arg(fixture(&format!("{name}-key.der")));
}

fn start_grunt(target: SocketAddr) -> (SocketAddr, Process) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
    command
        .args(["grunt", "--listen", "127.0.0.1:0", "--target"])
        .arg(format!("http://{target}/"))
        .arg("--controller-cert")
        .arg(fixture("controller.der"));
    identity(&mut command, "server");
    let mut child = Process(Some(command.stdout(Stdio::piped()).spawn().unwrap()));
    let pipe = child.0.as_mut().unwrap().stdout.take().unwrap();
    let (ready, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(pipe).read_line(&mut line).map(|_| line);
        let _ = ready.send(result);
    });
    let addr = receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (addr, child)
}

fn controller(grunts: &[SocketAddr], requests: u64) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
    command
        .args([
            "controller",
            "--concurrency",
            "2",
            "--rate",
            "997",
            "--max-p95-ms",
            "250",
            "--max-failure-rate",
            "5",
            "--deadline",
            "10",
            "--requests",
        ])
        .arg(requests.to_string());
    for grunt in grunts {
        command.arg("--grunt").arg(grunt.to_string());
    }
    identity(&mut command, "controller");
    command
}

fn read_headers(stream: &mut TcpStream) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    assert!(reader.read_line(&mut line).unwrap() > 0);
    assert!(line.starts_with("GET / "));
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            return;
        }
    }
}

fn start_target(
    allowed_requests: usize,
    slow_failures: usize,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        for request in 0..allowed_requests {
            let (mut stream, _) = common::accept(&listener);
            read_headers(&mut stream);
            let response = if request >= allowed_requests - slow_failures {
                thread::sleep(Duration::from_millis(300));
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno".as_slice()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx".as_slice()
            };
            stream.write_all(response).unwrap();
        }
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
    (address, thread)
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid report ({error}): {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn controller_splits_budget_and_collects_and_merges_two_grunts() {
    let (fast, fast_target) = start_target(21, 0);
    let (slow, slow_target) = start_target(20, 2);
    let (fast_grunt, _fast_process) = start_grunt(fast);
    let (slow_grunt, _slow_process) = start_grunt(slow);
    let output = controller(&[fast_grunt, slow_grunt], 41)
        .output_timeout()
        .unwrap();
    let report = report(&output);
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["partial"], false);
    assert_eq!(report["metrics"]["started"], 41);
    assert_eq!(report["metrics"]["completed"], 41);
    assert_eq!(report["metrics"]["failed"], 2);
    assert_eq!(report["metrics"]["http_failures"], 2);
    assert_eq!(report["metrics"]["response_bytes"], 43);
    assert_eq!(
        report["metrics"]["status_codes"],
        serde_json::json!({"200":39, "503":2})
    );
    assert_eq!(
        report["thresholds"],
        serde_json::json!({"failure_rate":"passed", "p95":"passed"})
    );
    assert!(report["latency_ms"]["p95"].as_f64().unwrap() < 250.0);
    assert!(report["latency_ms"]["p99"].as_f64().unwrap() >= 300.0);
    assert!(
        report["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| agent["status"] == "complete")
    );
    fast_target.join().unwrap();
    slow_target.join().unwrap();
}

#[cfg(unix)]
#[test]
fn interrupted_controller_cancels_the_pending_grunt_and_keeps_completed_metrics() {
    let (fast, fast_target) = start_target(1, 0);
    let slow = TcpListener::bind("127.0.0.1:0").unwrap();
    let (fast_grunt, _fast_process) = start_grunt(fast);
    let (slow_grunt, _slow_process) = start_grunt(slow.local_addr().unwrap());
    let child = Process(Some(
        controller(&[fast_grunt, slow_grunt], 2)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let (mut request, _) = common::accept(&slow);
    read_headers(&mut request);
    fast_target.join().unwrap();
    thread::sleep(Duration::from_millis(150));
    assert!(
        Command::new("kill")
            .args(["-s", "INT"])
            .arg(child.0.as_ref().unwrap().id().to_string())
            .status()
            .unwrap()
            .success()
    );
    let output = child.finish();
    let report = report(&output);
    assert_eq!(output.status.code(), Some(130), "{report}");
    assert_eq!(report["partial"], true);
    assert_eq!(report["interruption"], "sigint");
    assert_eq!(
        report["metrics"]["completed"],
        1,
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        report["thresholds"],
        serde_json::json!({"failure_rate":"not_evaluable", "p95":"not_evaluable"})
    );
    for (address, status) in [(fast_grunt, "complete"), (slow_grunt, "cancelled")] {
        assert!(
            report["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|agent| agent["address"] == address.to_string() && agent["status"] == status),
            "{report}"
        );
    }
    request
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    match request.read(&mut [0]) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("cancelled work connection still open: {result:?}"),
    }
}

#[test]
fn unavailable_grunt_leaves_a_partial_report_without_reassigning_its_share() {
    let (target, target_server) = start_target(1, 0);
    let (working, _process) = start_grunt(target);
    // The address exists, but no TLS handshake can complete.
    let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
    let missing = unavailable.local_addr().unwrap();
    let output = controller(&[working, missing], 2).output_timeout().unwrap();
    let report = report(&output);
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["partial"], true);
    assert_eq!(
        report["metrics"]["completed"],
        1,
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["thresholds"]["p95"], "not_evaluable");
    assert!(
        report["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["address"] == missing.to_string() && agent["status"] == "missing")
    );
    target_server.join().unwrap();
}
