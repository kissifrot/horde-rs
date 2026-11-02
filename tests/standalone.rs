use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

#[test]
fn cli_runs_requests_and_reports_http_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for index in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /test HTTP/1.1"));
            assert!(request.to_ascii_lowercase().contains("x-horde: zug-zug"));
            let status = if index == 0 {
                "500 Internal Server Error"
            } else {
                "200 OK"
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK"
            )
            .unwrap();
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args([
            "-n",
            "4",
            "-c",
            "2",
            "-H",
            "X-Horde: zug-zug",
            &format!("http://{address}/test"),
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Completed requests:      4"), "{stdout}");
    assert!(stdout.contains("Failed requests:         1"), "{stdout}");
    assert!(stdout.contains("HTTP 4xx/5xx:          1"), "{stdout}");
    assert!(stdout.contains("Redirects followed:      0"), "{stdout}");
    assert!(stdout.contains("Redirect limit failures: 0"), "{stdout}");
    assert!(stdout.contains("Response bytes:          8"), "{stdout}");
    assert!(stdout.contains("  200: 3"), "{stdout}");
    assert!(stdout.contains("  500: 1"), "{stdout}");
    for label in [
        "Completed requests:",
        "Redirect limit failures:",
        "Latency p50:",
        "Latency p100:",
    ] {
        let line = stdout.lines().find(|line| line.starts_with(label)).unwrap();
        assert!(!line[25..].starts_with(' '), "{line}");
        assert!(line[label.len()..25].chars().all(|c| c == ' '), "{line}");
    }
}

#[test]
fn reports_status_even_when_body_is_incomplete() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).unwrap() > 0);
        write!(
            stream,
            "HTTP/1.1 201 Created\r\nContent-Length: 10\r\nConnection: close\r\n\r\nOK"
        )
        .unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg(format!("http://{address}/"))
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Failed requests:         1"), "{stdout}");
    assert!(stdout.contains("  201: 1"), "{stdout}");
}

#[test]
fn rejects_invalid_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args(["-n", "0", "http://localhost/"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    for args in [
        vec!["-L", "--max-redirects", "0", "http://localhost/"],
        vec!["--max-redirects", "5", "http://localhost/"],
        vec!["--rate", "0", "http://localhost/"],
        vec!["--rate", "1000000001", "http://localhost/"],
        vec!["--max-failure-rate", "101", "http://localhost/"],
        vec!["--max-failure-rate", "NaN", "http://localhost/"],
        vec!["--max-p95-ms", "-1", "http://localhost/"],
        vec!["--max-p95-ms", "inf", "http://localhost/"],
    ] {
        assert!(
            !Command::new(env!("CARGO_BIN_EXE_horde"))
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

#[test]
fn ci_thresholds_report_results_and_exit_status() {
    for (failure_limit, p95_limit, success) in [
        ("50", "10000", true),
        ("49", "10000", false),
        ("50", "0", false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for status in ["200 OK", "500 Error"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).unwrap() > 0);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        let output = Command::new(env!("CARGO_BIN_EXE_horde"))
            .args([
                "-n",
                "2",
                "--json",
                "--max-failure-rate",
                failure_limit,
                "--max-p95-ms",
                p95_limit,
                &format!("http://{address}/"),
            ])
            .output()
            .unwrap();
        server.join().unwrap();
        assert_eq!(output.status.success(), success);
        if !success {
            assert_eq!(output.status.code(), Some(2));
        }
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["thresholds"]["failure_rate_percent"]["observed"],
            50.0
        );
        assert_eq!(
            report["thresholds"]["failure_rate_percent"]["passed"],
            failure_limit == "50"
        );
        assert_eq!(report["thresholds"]["p95_ms"]["passed"], p95_limit != "0");
    }
}

#[test]
fn rate_is_shared_across_workers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args([
            "-n",
            "4",
            "-c",
            "4",
            "--rate",
            "5",
            "--json",
            &format!("http://{address}/"),
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["completed"], 4);
    assert!(report["elapsed_seconds"].as_f64().unwrap() >= 0.55);
}

#[test]
fn connection_reuse_can_be_disabled_and_connections_are_counted() {
    use std::io::BufRead;
    for reuse in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = 0;
            let mut connections = 0;
            while requests < 2 {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                connections += 1;
                let mut stream = std::io::BufReader::new(stream);
                for _ in 0..if reuse { 2 } else { 1 } {
                    let mut request = String::new();
                    loop {
                        let mut line = String::new();
                        assert!(stream.read_line(&mut line).unwrap() > 0);
                        if line == "\r\n" {
                            break;
                        }
                        request.push_str(&line);
                    }
                    assert_eq!(
                        request.to_ascii_lowercase().contains("connection: close"),
                        !reuse
                    );
                    write!(
                        stream.get_mut(),
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                    )
                    .unwrap();
                    requests += 1;
                }
            }
            connections
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
        command.args(["-n", "2", "-c", "1", "--json"]);
        if !reuse {
            command.arg("--no-connection-reuse");
        }
        let output = command.arg(format!("http://{address}/")).output().unwrap();
        let actual = server.join().unwrap();
        assert!(output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(actual, if reuse { 1 } else { 2 });
        assert_eq!(report["connections_opened"], actual);
    }
}

#[test]
fn user_agent_can_be_set_and_overrides_header_option() {
    for (args, expected) in [
        (vec![], Some(concat!("Horde/", env!("CARGO_PKG_VERSION")))),
        (vec!["--user-agent", "Horde/1.0"], Some("Horde/1.0")),
        (vec!["-H", "User-Agent: other"], Some("other")),
        (
            vec!["-H", "User-Agent: other", "-A", "Horde/2.0"],
            Some("Horde/2.0"),
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let agents: Vec<_> = request
                .lines()
                .filter_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("user-agent")
                        .then_some(value.trim())
                })
                .collect();
            assert_eq!(agents, expected.into_iter().collect::<Vec<_>>());
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        let output = Command::new(env!("CARGO_BIN_EXE_horde"))
            .args(args)
            .arg(format!("http://{address}/"))
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(output.status.success());
    }
}

#[test]
fn requests_preserve_credentials_and_repeated_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let n = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..n]).to_ascii_lowercase();
            assert!(
                request.contains(
                    "authorization: basic dXNlcjpwYXNz"
                        .to_ascii_lowercase()
                        .as_str()
                )
            );
            assert!(request.contains("x-test: one"));
            assert!(request.contains("x-test: two"));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args([
            "-n",
            "2",
            "-H",
            "X-Test: one",
            "-H",
            "X-Test: two",
            &format!("http://user:pass@{address}/"),
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Failed requests:         0")
    );
}

#[test]
fn redirects_are_opt_in_and_bounded() {
    for (follow, limit, redirects, expected_status, expected_failed) in [
        (false, None, 1, "  302: 1", "Failed requests:         0"),
        (true, None, 10, "  200: 1", "Failed requests:         0"),
        (
            true,
            None,
            11,
            "No HTTP responses received",
            "Failed requests:         1",
        ),
        (true, Some(2), 2, "  200: 1", "Failed requests:         0"),
        (
            true,
            Some(2),
            3,
            "No HTTP responses received",
            "Failed requests:         1",
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let requests = if follow {
                redirects + usize::from(redirects <= limit.unwrap_or(10))
            } else {
                1
            };
            for index in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).unwrap() > 0);
                if index < redirects {
                    write!(stream, "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK"
                    )
                    .unwrap();
                }
            }
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
        if follow {
            command.arg("--follow-redirects");
        }
        if let Some(limit) = limit {
            command.args(["--max-redirects", &limit.to_string()]);
        }
        let output = command.arg(format!("http://{address}/")).output().unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(expected_status), "{stdout}");
        assert!(stdout.contains(expected_failed), "{stdout}");
        let followed = if follow {
            redirects.min(limit.unwrap_or(10))
        } else {
            0
        };
        let limit_failures = usize::from(follow && redirects > limit.unwrap_or(10));
        assert!(
            stdout.contains(&format!("Redirects followed:      {followed}")),
            "{stdout}"
        );
        assert!(
            stdout.contains(&format!("Redirect limit failures: {limit_failures}")),
            "{stdout}"
        );
        assert!(stdout.contains("Other:                 0"), "{stdout}");
    }
}

#[test]
fn reports_redirects_before_a_transport_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).unwrap() > 0);
        write!(stream, "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        assert!(stream.read(&mut request).unwrap() > 0);
    });
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args(["-L", &format!("http://{address}/")])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Failed requests:         1"), "{stdout}");
    assert!(stdout.contains("Redirects followed:      1"), "{stdout}");
    assert!(stdout.contains("Redirect limit failures: 0"), "{stdout}");
}

#[test]
fn json_report_and_request_body_survive_redirect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
                if request
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                    .is_some_and(|i| request.len() >= i + 4 + 7)
                {
                    break;
                }
            }
            assert!(request.starts_with(b"POST "));
            assert!(request.ends_with(b"payload"));
            if index == 0 {
                write!(stream, "HTTP/1.1 307 Temporary Redirect\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                write!(
                    stream,
                    "HTTP/1.1 500 Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        }
    });
    let path = std::env::temp_dir().join(format!("horde-body-{}", std::process::id()));
    std::fs::write(&path, b"payload").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args(["-L", "-X", "POST", "--json", "--body-file"])
        .arg(&path)
        .arg(format!("http://{address}/"))
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["completed"], 1);
    assert_eq!(report["failure_details"]["http"], 1);
    assert_eq!(report["redirects_followed"], 1);
    assert_eq!(report["connections_opened"], 2);
    assert_eq!(report["status_codes"]["500"], 1);
}

#[test]
fn duration_stops_new_requests_and_finishes_in_flight() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = stop.clone();
    let server = thread::spawn(move || {
        while !done.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0; 4096];
                    assert!(stream.read(&mut request).unwrap() > 0);
                    thread::sleep(std::time::Duration::from_millis(600));
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args(["--duration", "1", "--json", &format!("http://{address}/")])
        .output()
        .unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["completed"].as_u64().unwrap() >= 1);
    assert_eq!(report["failed"], 0);
    assert!(report["elapsed_seconds"].as_f64().unwrap() >= 1.0);
}
