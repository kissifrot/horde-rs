#![cfg(unix)]

mod common;
#[path = "../src/signals.rs"]
#[cfg_attr(not(feature = "distributed-offline"), allow(dead_code))]
mod signals;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn send(pid: u32, kind: &str) {
    assert!(
        Command::new("kill")
            .args([kind, &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
}
fn wait(child: &mut Child, timeout: Duration) {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < timeout, "child did not exit");
        thread::sleep(Duration::from_millis(10));
    }
}
fn slow_server() -> (
    u16,
    mpsc::Receiver<()>,
    mpsc::Receiver<bool>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&listener);
        let mut buf = [0u8; 4096];
        assert!(stream.read(&mut buf).unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nx")
            .unwrap();
        ready_tx.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let closed = match stream.read(&mut buf) {
            Ok(0) => true,
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            _ => false,
        };
        closed_tx.send(closed).unwrap();
    });
    (port, ready_rx, closed_rx, server)
}
fn exercise_cli(signal: &str, code: i32, scenario: bool, json: bool) {
    let (port, ready, closed, server) = slow_server();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_horde"));
    cmd.args(["-n", "100", "-c", "1", "--max-failure-rate", "0"]);
    if json {
        cmd.arg("--json");
    }
    let scenario_path =
        std::env::temp_dir().join(format!("horde-signal-{}-{port}.json", std::process::id()));
    if scenario {
        let scenario = serde_json::json!({"version":1,"name":"interrupt","steps":[{"name":"slow","url":format!("http://127.0.0.1:{port}/")}]});
        std::fs::write(&scenario_path, scenario.to_string()).unwrap();
        cmd.arg("--scenario").arg(&scenario_path);
    } else {
        cmd.arg(format!("http://127.0.0.1:{port}/"));
    }
    let mut child = ChildGuard(Some(
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    ready.recv_timeout(Duration::from_secs(4)).unwrap();
    send(child.0.as_ref().unwrap().id(), signal);
    wait(child.0.as_mut().unwrap(), Duration::from_secs(4));
    let out = child.0.take().unwrap().wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    if json {
        let output: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(output["type"], "interrupted");
        assert_eq!(output["partial"], true);
        assert_eq!(output["thresholds"]["status"], "not_evaluable");
        assert_eq!(
            output["reason"],
            if code == 130 { "sigint" } else { "sigterm" }
        );
    } else {
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("Run interrupted"));
        assert!(text.contains("not evaluable"));
    }
    assert!(closed.recv_timeout(Duration::from_secs(2)).unwrap());
    server.join().unwrap();
    if scenario {
        std::fs::remove_file(scenario_path).unwrap();
    }
}
#[test]
fn sigint_single_url_closes_request_and_emits_partial_json() {
    exercise_cli("-INT", 130, false, true);
}
#[test]
fn sigterm_scenario_closes_request_and_emits_partial_json() {
    exercise_cli("-TERM", 143, true, true);
}
#[test]
fn sigterm_single_url_emits_partial_text() {
    exercise_cli("-TERM", 143, false, false);
}

#[test]
#[ignore = "subprocess fixture, run by second_signal_forces_exit_before_grace_expires"]
fn signal_watchdog_fixture() {
    assert!(std::env::var_os("HORDE_SIGNAL_WATCHDOG_FIXTURE").is_some());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let cancel = horde_rs::Cancellation::new();
        let _rx = signals::install(cancel).unwrap();
        println!("WATCHDOG_READY");
        std::io::stdout().flush().unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    panic!("fixture should have been stopped");
}
#[test]
fn second_signal_forces_exit_before_grace_expires() {
    let mut child = ChildGuard(Some(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "signal_watchdog_fixture",
                "--nocapture",
            ])
            .env("HORDE_SIGNAL_WATCHDOG_FIXTURE", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let stdout = child.0.as_mut().unwrap().stdout.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("WATCHDOG_READY") {
                let _ = ready_tx.send(());
                break;
            }
        }
    });
    ready_rx.recv_timeout(Duration::from_secs(4)).unwrap();
    let stderr = child.0.as_mut().unwrap().stderr.take().unwrap();
    let (seen_tx, seen_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("Interrupt received") {
                let _ = seen_tx.send(());
                break;
            }
        }
    });
    send(child.0.as_ref().unwrap().id(), "-INT");
    seen_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    send(child.0.as_ref().unwrap().id(), "-INT");
    wait(child.0.as_mut().unwrap(), Duration::from_secs(3));
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(child.0.as_mut().unwrap().wait().unwrap().code(), Some(130));
}

#[cfg(feature = "distributed-offline")]
#[test]
#[ignore = "subprocess fixture, run by sigint_cancels_local_grunt_and_closes_request"]
fn grunt_signal_fixture() {
    let port = std::env::var_os("HORDE_GRUNT_FIXTURE_PORT").expect("fixture requires a port");
    let port: u16 = port.to_str().unwrap().parse().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use horde_rs::{
            TestPlan,
            distributed::{AgentLimits, Budget, LocalOutcome, StopReason, run_local},
        };
        let cancel = horde_rs::Cancellation::new();
        let signals = signals::install(cancel.clone()).unwrap();
        let plan = TestPlan {
            url: reqwest::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
            requests: 1,
            concurrency: 1,
            timeout: Duration::from_secs(10),
            method: reqwest::Method::GET,
            headers: reqwest::header::HeaderMap::new(),
            body: None,
            duration: None,
            rate: None,
            verbose: false,
            bounded_latency: true,
            reuse_connections: true,
            follow_redirects: false,
            max_redirects: 10,
        };
        let budget = Budget {
            requests: Some(1),
            concurrency: 1,
            rate_per_second: 10,
            duration: None,
        };
        let limits = AgentLimits {
            max_samples: 1,
            max_concurrency: 1,
            max_rate_per_second: 10,
            max_duration: Duration::from_secs(15),
        };
        assert!(matches!(
            run_local(plan, budget, limits, cancel).await.unwrap(),
            LocalOutcome::Partial {
                reason: StopReason::Cancelled
            }
        ));
        let _ = signals::report(&signals, true);
    });
}
#[cfg(feature = "distributed-offline")]
#[test]
fn sigint_cancels_local_grunt_and_closes_request() {
    let (port, ready, closed, server) = slow_server();
    let mut child = ChildGuard(Some(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "grunt_signal_fixture",
                "--nocapture",
            ])
            .env("HORDE_GRUNT_FIXTURE_PORT", port.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    ready.recv_timeout(Duration::from_secs(4)).unwrap();
    send(child.0.as_ref().unwrap().id(), "-INT");
    wait(child.0.as_mut().unwrap(), Duration::from_secs(4));
    let out = child.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"partial\":true"));
    assert!(closed.recv_timeout(Duration::from_secs(2)).unwrap());
    server.join().unwrap();
}

#[test]
fn sigint_joins_all_concurrent_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (headers_tx, headers_rx) = mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = common::accept(&listener);
            let h = headers_tx.clone();
            let c = closed_tx.clone();
            workers.push(thread::spawn(move || {
                let mut buf = [0u8; 4096];
                assert!(stream.read(&mut buf).unwrap() > 0);
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nx")
                    .unwrap();
                h.send(()).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                let closed = match stream.read(&mut buf) {
                    Ok(0) => true,
                    Err(e) => matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ),
                    _ => false,
                };
                c.send(closed).unwrap();
            }));
        }
        for _ in 0..4 {
            headers_rx.recv_timeout(Duration::from_secs(4)).unwrap();
        }
        ready_tx.send(()).unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
    let mut child = ChildGuard(Some(
        Command::new(env!("CARGO_BIN_EXE_horde"))
            .args([
                "--json",
                "-n",
                "100",
                "-c",
                "4",
                &format!("http://127.0.0.1:{port}/"),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    ready_rx.recv_timeout(Duration::from_secs(4)).unwrap();
    send(child.0.as_ref().unwrap().id(), "-INT");
    wait(child.0.as_mut().unwrap(), Duration::from_secs(4));
    let out = child.0.take().unwrap().wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["partial"], true);
    for _ in 0..4 {
        assert!(closed_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    }
    server.join().unwrap();
}
