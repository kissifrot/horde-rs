#![cfg(feature = "distributed-transport")]

mod common;

use common::CommandTimeout;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
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

fn identity(command: &mut Command, name: &str, root: &str) {
    command
        .arg("--ca")
        .arg(fixture(root))
        .arg("--cert")
        .arg(fixture(&format!("{name}.der")))
        .arg("--key")
        .arg(fixture(&format!("{name}-key.der")));
}

fn grunt_command(listen: &str, target: &str) -> Command {
    grunt_with_certificate(listen, target, "server")
}

fn grunt_with_certificate(listen: &str, target: &str, server_certificate: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
    command
        .args(["grunt", "--listen", listen, "--target", target])
        .arg("--controller-cert")
        .arg(fixture("controller.der"));
    identity(&mut command, server_certificate, "ca.der");
    command
}

fn start_grunt(target: SocketAddr) -> (SocketAddr, Process) {
    start_grunt_command(grunt_command("127.0.0.1:0", &format!("http://{target}/")))
}

fn start_grunt_command(mut command: Command) -> (SocketAddr, Process) {
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
    controller_with_root(grunts.iter().map(ToString::to_string), requests, "ca.der")
}

fn controller_with_root(
    grunts: impl IntoIterator<Item = String>,
    requests: u64,
    root: &str,
) -> Command {
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
        command.arg("--grunt").arg(grunt);
    }
    identity(&mut command, "controller", root);
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
fn preflight_requires_the_hello_after_controller_authorization() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let (address, _grunt) = start_grunt(target.local_addr().unwrap());
    for (credential, ready) in [("controller", true), ("unauthorized", false)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
        command.args(["preflight", "--grunt", &address.to_string()]);
        identity(&mut command, credential, "ca.der");
        let output = command.output_timeout().unwrap();
        let report = report(&output);
        assert_eq!(output.status.code(), Some(i32::from(!ready)), "{report}");
        assert_eq!(report["ready"], ready);
        let peer = &report["grunts"][0];
        assert_eq!(peer["address"], address.to_string());
        assert_eq!(peer["status"], if ready { "ready" } else { "failed" });
        assert_eq!(peer["error"].is_null(), ready);
    }
    target.set_nonblocking(true).unwrap();
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
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

#[test]
fn private_ingress_requires_exact_addresses_and_keeps_the_target_on_loopback() {
    for (listen, peer, target, reason) in [
        (
            "10.0.0.2:0",
            None,
            "http://127.0.0.1:9/",
            "explicit private listener",
        ),
        (
            "0.0.0.0:0",
            Some("10.0.0.1"),
            "http://127.0.0.1:9/",
            "explicit private listener",
        ),
        (
            "192.0.2.2:0",
            Some("10.0.0.1"),
            "http://127.0.0.1:9/",
            "explicit private listener",
        ),
        (
            "10.0.0.2:0",
            Some("10.0.0.1"),
            "http://10.0.0.3:9/",
            "target requires a literal loopback IP",
        ),
        (
            "10.0.0.2:0",
            Some("10.0.0.1"),
            "http://localhost:9/",
            "target requires a literal loopback IP",
        ),
    ] {
        let mut command = grunt_command(listen, target);
        if let Some(peer) = peer {
            command.args(["--allow-controller-ip", peer]);
        }
        let output = command.output_timeout().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty(), "listener must not start");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(reason),
            "{output:?}"
        );
    }
}

#[test]
fn controller_requires_opt_in_and_rejects_non_private_peers() {
    for (address, private) in [
        ("10.0.0.2:9", false),
        ("192.0.2.2:9", true),
        ("127.0.0.1:9", true),
    ] {
        let mut command = controller(&[address.parse().unwrap()], 1);
        if private {
            command.arg("--private-network");
        }
        let output = command.output_timeout().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("invalid controller configuration or Grunt address"),
            "{output:?}"
        );
    }
}

#[test]
#[ignore = "requires HORDE_TEST_PRIVATE_IP set to an address assigned to this test machine"]
fn private_ingress_filters_the_source_before_mutual_tls() {
    let ip: IpAddr = std::env::var("HORDE_TEST_PRIVATE_IP")
        .unwrap()
        .parse()
        .unwrap();
    let listen = SocketAddr::new(ip, 0).to_string();
    let unlisted_peer = if ip == "172.31.255.254".parse::<IpAddr>().unwrap() {
        "172.31.255.253"
    } else {
        "172.31.255.254"
    };

    let mut denied = grunt_command(&listen, "http://127.0.0.1:9/");
    denied.args(["--allow-controller-ip", unlisted_peer]);
    let (denied_address, _denied_grunt) = start_grunt_command(denied);
    let mut tcp = TcpStream::connect_timeout(&denied_address, Duration::from_secs(2)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    match tcp.read(&mut [0]) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("unlisted source reached TLS handshake: {result:?}"),
    }

    let (target, target_server) = start_target(1, 0);
    let mut allowed = grunt_command(&listen, &format!("http://{target}/"));
    allowed.arg("--allow-controller-ip").arg(ip.to_string());
    let (allowed_address, _allowed_grunt) = start_grunt_command(allowed);
    let output = controller_with_root([format!("{allowed_address}=localhost")], 1, "ca.der")
        .arg("--private-network")
        .output_timeout()
        .unwrap();
    let report = report(&output);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["metrics"]["completed"], 1);
    assert_eq!(report["partial"], false);
    target_server.join().unwrap();
}

#[test]
fn controller_authenticates_each_grunt_with_its_own_dns_or_ip_san() {
    let (dns_target, dns_server) = start_target(1, 0);
    let (ip_target, ip_server) = start_target(1, 0);
    let (dns_grunt, _dns_process) = start_grunt_command(grunt_with_certificate(
        "127.0.0.1:0",
        &format!("http://{dns_target}/"),
        "identity-dns",
    ));
    let (ip_grunt, _ip_process) = start_grunt_command(grunt_with_certificate(
        "127.0.0.1:0",
        &format!("http://{ip_target}/"),
        "identity-ip",
    ));
    let output = controller_with_root(
        [
            format!("{dns_grunt}=grunt-a.test"),
            format!("{ip_grunt}=127.0.0.1"),
        ],
        2,
        "identity-ca.der",
    )
    .output_timeout()
    .unwrap();
    let report = report(&output);
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["metrics"]["completed"], 2);
    assert_eq!(report["partial"], false);
    dns_server.join().unwrap();
    ip_server.join().unwrap();
}

#[test]
fn controller_refuses_expired_unknown_and_wrong_grunt_certificates_before_submission() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let target_url = format!("http://{}/", target.local_addr().unwrap());
    for (certificate, reason) in [
        ("identity-expired", "certificate expired"),
        ("identity-foreign", "UnknownIssuer"),
        ("identity-ip", "certificate not valid for name"),
    ] {
        let (grunt, _process) = start_grunt_command(grunt_with_certificate(
            "127.0.0.1:0",
            &target_url,
            certificate,
        ));
        let output = controller_with_root([format!("{grunt}=grunt-a.test")], 1, "identity-ca.der")
            .output_timeout()
            .unwrap();
        let report = report(&output);
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["partial"], true);
        assert_eq!(report["metrics"]["started"], 0);
        assert_eq!(report["agents"][0]["status"], "missing");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(reason),
            "{certificate}: expected {reason:?}, got {stderr:?}"
        );
    }
    target.set_nonblocking(true).unwrap();
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn controller_rejects_reused_names_for_private_grunts_before_connecting() {
    let output = controller_with_root(
        [
            "10.0.0.2:7071=grunt-a.test".to_owned(),
            "10.0.0.3:7071=GRUNT-A.TEST".to_owned(),
        ],
        2,
        "identity-ca.der",
    )
    .arg("--private-network")
    .output_timeout()
    .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("private Grunts require distinct TLS identities")
    );
}

#[test]
fn abrupt_controller_exit_closes_both_grunts_workloads_after_disconnect() {
    let first_target = TcpListener::bind("127.0.0.1:0").unwrap();
    let second_target = TcpListener::bind("127.0.0.1:0").unwrap();
    let (first_grunt, _first_process) = start_grunt(first_target.local_addr().unwrap());
    let (second_grunt, _second_process) = start_grunt(second_target.local_addr().unwrap());
    let mut child = Process(Some(
        controller(&[first_grunt, second_grunt], 4)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let (mut first, _) = common::accept(&first_target);
    let (mut second, _) = common::accept(&second_target);
    read_headers(&mut first);
    read_headers(&mut second);
    let process = child.0.as_mut().unwrap();
    process.kill().unwrap();
    assert!(!process.wait().unwrap().success());

    for (mut stream, listener) in [(first, first_target), (second, second_target)] {
        // The HTTP request timeout is 10 s, while disconnect grace is 2 s.
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        match stream.read(&mut [0]) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            result => panic!("Grunt did not cancel after the controller disappeared: {result:?}"),
        }
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[cfg(unix)]
#[test]
fn abrupt_grunt_exit_marks_it_missing_and_preserves_healthy_share() {
    let (healthy_target, healthy_server) = start_target(1, 0);
    let failed_target = TcpListener::bind("127.0.0.1:0").unwrap();
    let (healthy_grunt, _healthy_process) = start_grunt(healthy_target);
    let (failed_grunt, mut failed_process) = start_grunt(failed_target.local_addr().unwrap());
    let child = Process(Some(
        controller(&[healthy_grunt, failed_grunt], 2)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let (mut pending, _) = common::accept(&failed_target);
    read_headers(&mut pending);
    let grunt = failed_process.0.as_mut().unwrap();
    grunt.kill().unwrap();
    assert!(!grunt.wait().unwrap().success());

    let output = child.finish();
    let report = report(&output);
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["partial"], true);
    assert_eq!(report["metrics"]["completed"], 1);
    assert_eq!(report["thresholds"]["p95"], "not_evaluable");
    for (address, status) in [(healthy_grunt, "complete"), (failed_grunt, "missing")] {
        assert!(
            report["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|g| g["address"] == address.to_string() && g["status"] == status),
            "{report}"
        );
    }
    healthy_server.join().unwrap();
}
