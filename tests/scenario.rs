use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

#[test]
fn scenario_user_agent_defaults_and_step_overrides() {
    for (case, custom, expected) in [
        (
            "default",
            None,
            concat!("Horde/", env!("CARGO_PKG_VERSION")),
        ),
        ("custom", Some("Horde-load/2"), "Horde-load/2"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for expected in [expected, "Step-agent/1"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut data = [0; 4096];
                let n = stream.read(&mut data).unwrap();
                let request = String::from_utf8_lossy(&data[..n]);
                let agents: Vec<_> = request
                    .lines()
                    .filter_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("user-agent")
                            .then_some(value.trim())
                    })
                    .collect();
                assert_eq!(agents, [expected]);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        let file = std::env::temp_dir().join(format!(
            "horde-scenario-agent-{case}-{}.json",
            std::process::id()
        ));
        std::fs::write(&file, serde_json::to_vec(&serde_json::json!({"version":1,"name":"agent", "steps":[
            {"name":"default", "url":format!("http://{address}/")},
            {"name":"override", "url":format!("http://{address}/"), "headers":{"User-Agent":"Step-agent/1"}}
        ]})).unwrap()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
        command.arg("--scenario").arg(&file).arg("--json");
        if let Some(agent) = custom {
            command.args(["-A", agent]);
        }
        let output = command.output().unwrap();
        std::fs::remove_file(file).unwrap();
        server.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["scenarios"]["failed"], 0);
    }
}

#[test]
fn scenario_rejects_invalid_user_agent() {
    let file = std::env::temp_dir().join(format!(
        "horde-scenario-invalid-agent-{}.json",
        std::process::id()
    ));
    std::fs::write(
        &file,
        r#"{"version":1,"name":"agent","steps":[{"name":"one","url":"http://127.0.0.1/"}]}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&file)
        .args(["-A", "invalid\nagent"])
        .output()
        .unwrap();
    std::fs::remove_file(file).unwrap();
    assert!(!output.status.success());
}

#[test]
fn scenario_users_keep_isolated_cookies_across_steps() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let ids = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&observed);
    let server = thread::spawn(move || {
        let mut handlers = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let ids = Arc::clone(&ids);
            let seen = Arc::clone(&seen);
            handlers.push(thread::spawn(move || {
                let mut data = [0; 4096];
                let n = stream.read(&mut data).unwrap();
                let request = String::from_utf8_lossy(&data[..n]).to_ascii_lowercase();
                if request.starts_with("get /start ") {
                    assert!(!request.contains("referer:"), "{request}");
                    assert!(request.contains("x-test: zug-zug"), "{request}");
                    assert!(!request.contains("cookie:"));
                    let id = ids.fetch_add(1, Ordering::Relaxed) + 1;
                    thread::sleep(std::time::Duration::from_millis(75));
                    write!(stream, "HTTP/1.1 200 OK\r\nSet-Cookie: session={id}; Path=/; HttpOnly\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    assert!(request.starts_with("get /next "), "{request}");
                    assert!(request.contains(&format!("referer: http://{address}/start")), "{request}");
                    let cookie = request.lines().find(|line| line.starts_with("cookie:")).unwrap().to_owned();
                    seen.lock().unwrap().push(cookie);
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
            }));
        }
        for handler in handlers {
            handler.join().unwrap();
        }
    });
    let file =
        std::env::temp_dir().join(format!("horde-scenario-users-{}.json", std::process::id()));
    std::fs::write(&file, serde_json::to_vec(&serde_json::json!({"version":1,"name":"journey", "variables":{"base":format!("http://{address}"), "token":"zug-zug"}, "steps":[{"name":"start", "url":"{{base}}/start", "headers":{"X-Test":"{{token}}"}},{"name":"next", "url":"{{base}}/next", "wait_ms":30}]})).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args(["--scenario"])
        .arg(&file)
        .args([
            "-n",
            "2",
            "-c",
            "2",
            "--json",
            "--verbose",
            "--auto-referer",
        ])
        .output()
        .unwrap();
    std::fs::remove_file(file).unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scenarios"]["completed"], 2);
    assert_eq!(report["scenarios"]["failed"], 0);
    assert_eq!(report["steps"][1]["completed"], 2);
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(
        log.contains("[user 1 scenario ") && log.contains("[user 2 scenario "),
        "{log}"
    );
    assert!(log.contains("step \"start\" completed status=200"), "{log}");
    assert!(log.contains("waiting 30 ms before step \"next\""), "{log}");
    assert_eq!(log.matches("succeeded").count(), 2, "{log}");
    assert!(
        !log.contains("zug-zug") && !log.contains("session="),
        "{log}"
    );
    assert!(
        log.lines()
            .all(|line| line.starts_with("[+") && line.contains("s] [user ")),
        "{log}"
    );
    assert!(report["scenarios"]["mean_duration_ms"].as_f64().unwrap() >= 30.0);
    let mut cookies = observed.lock().unwrap().clone();
    cookies.sort();
    assert_eq!(cookies, ["cookie: session=1", "cookie: session=2"]);
}

#[test]
fn failed_step_stops_scenario_and_reports_skipped_steps() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut data = [0; 4096];
        assert!(stream.read(&mut data).unwrap() > 0);
        write!(
            stream,
            "HTTP/1.1 500 Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    });
    let file = std::env::temp_dir().join(format!(
        "horde-scenario-failure-{}.json",
        std::process::id()
    ));
    std::fs::write(&file, serde_json::to_vec(&serde_json::json!({"version":1,"name":"failure", "steps":[{"name":"first", "url":format!("http://{address}/")},{"name":"skipped", "url":format!("http://{address}/next")}]})).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&file)
        .arg("-v")
        .arg("--json")
        .output()
        .unwrap();
    std::fs::remove_file(file).unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scenarios"]["failed"], 1);
    assert_eq!(report["steps"][0]["failed"], 1);
    assert_eq!(report["steps"][0]["status_codes"]["500"], 1);
    assert_eq!(report["steps"][0]["failure_details"]["status"], 1);
    assert!(report["steps"][0]["latency_ms"]["p95"].as_f64().unwrap() > 0.0);
    assert_eq!(report["steps"][1]["completed"], 0);
    assert!(report["steps"][1]["latency_ms"]["p95"].is_null());
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("step \"first\" completed status=500"), "{log}");
    assert!(log.contains("errors=status"), "{log}");
    assert!(log.contains("] failed"), "{log}");
    assert!(!log.contains("skipped"), "{log}");
    assert!(report["steps"][1]["mean_latency_ms"].is_null());
}

#[test]
fn scenario_ci_thresholds_report_success_failure_and_skipped_steps() {
    for (case, status, limit, bounded, expected_exit, expect_skipped) in [
        ("pass", 200, "10000", false, 0, false),
        ("p95", 200, "0", false, 2, false),
        ("bounded", 200, "10000", true, 0, false),
        ("failed", 500, "10000", false, 2, true),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let requests = if status == 500 { 1 } else { 2 };
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut data = [0; 4096];
                assert!(stream.read(&mut data).unwrap() > 0);
                write!(
                    stream,
                    "HTTP/1.1 {status} Result\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        let file = std::env::temp_dir().join(format!(
            "horde-threshold-{case}-{}.json",
            std::process::id()
        ));
        let scenario = serde_json::json!({"version":1,"name":"thresholds", "steps":[{"name":"one","url":format!("http://{addr}/")},{"name":"two","url":format!("http://{addr}/")}]});
        std::fs::write(&file, serde_json::to_vec(&scenario).unwrap()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
        command.arg("--scenario").arg(&file).args([
            "--json",
            "--max-failure-rate",
            "0",
            "--max-step-p95-ms",
            limit,
        ]);
        if bounded {
            command.arg("--bounded-latency");
        }
        let output = command.output().unwrap();
        std::fs::remove_file(file).unwrap();
        server.join().unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["thresholds"]["step_p95_ms"]["passed"],
            !expect_skipped && limit != "0"
        );
        assert_eq!(
            report["thresholds"]["failure_rate_percent"]["passed"],
            status == 200
        );
        if expect_skipped {
            assert!(report["thresholds"]["step_p95_ms"]["steps"][1]["observed"].is_null());
            assert_eq!(
                report["thresholds"]["step_p95_ms"]["steps"][1]["passed"],
                false
            );
        }
        if bounded {
            assert_eq!(
                report["latency_method"],
                "hdr_histogram_upper_bound_3_significant_digits"
            );
        }
    }
}

#[test]
fn invalid_scenarios_are_rejected_before_running() {
    for invalid in [
        r#"{"version":1,"name":"x","steps":[]}"#,
        r#"{"version":1,"name":"x","steps":[{"name":"a","url":"file:///etc/passwd"}]}"#,
        r#"{"version":1,"name":"x","steps":[{"name":"a","url":"http://localhost"},{"name":"a","url":"http://localhost"}]}"#,
        r#"{"version":1,"name":"x","steps":[{"name":"a","url":"http://{{missing}}/"}]}"#,
        r#"{"version":1,"name":"x","steps":[{"name":"a","url":"http://localhost/{{bad"}]}"#,
        r#"{"version":1,"name":"x","variables":{"bad-key":"x"},"steps":[{"name":"a","url":"http://localhost/"}]}"#,
        r#"{"version":1,"name":"x","steps":[{"name":"a","url":"http://localhost/","body":"x","form":{"x":"y"}}]}"#,
        r#"{"version":1,"name":"x","steps":[{"name":"a","url":"http://localhost/","form":{},"json_body":{}}]}"#,
    ] {
        assert!(horde_rs::scenario::Scenario::from_json(invalid.as_bytes()).is_err());
    }
}

#[test]
fn versioned_schema_matches_scenario_validation() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../schemas/scenario-v1.schema.json")).unwrap();
    jsonschema::meta::validate(&schema).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let example: serde_json::Value =
        serde_json::from_str(include_str!("../examples/journey.json")).unwrap();
    assert!(validator.is_valid(&example));
    assert!(
        horde_rs::scenario::Scenario::from_json(&serde_json::to_vec(&example).unwrap()).is_ok()
    );
    for document in [
        serde_json::json!({"name":"x", "steps":[{"name":"a", "url":"http://localhost/"}]}),
        serde_json::json!({"version":2,"name":"x", "steps":[{"name":"a", "url":"http://localhost/"}]}),
        serde_json::json!({"version":1,"name":"x", "steps":[{"name":"a", "url":"http://localhost/", "typo":true}]}),
        serde_json::json!({"version":1,"name":"x", "steps":[{"name":"a", "url":"http://localhost/", "wait_ms":-1}]}),
        serde_json::json!({"version":1,"name":"x", "variables":{"invalid-name":"x"}, "steps":[{"name":"a", "url":"http://localhost/"}]}),
    ] {
        assert!(!validator.is_valid(&document), "{document}");
        assert!(
            horde_rs::scenario::Scenario::from_json(&serde_json::to_vec(&document).unwrap())
                .is_err(),
            "{document}"
        );
    }
    let unsupported =
        br#"{"version":2,"name":"x","steps":[{"name":"a","url":"http://localhost/"}]}"#;
    assert!(
        horde_rs::scenario::Scenario::from_json(unsupported)
            .unwrap_err()
            .to_string()
            .contains("unsupported scenario version: 2")
    );
}

#[test]
fn scenario_variables_expand_in_request_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut data = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let n = stream.read(&mut chunk).unwrap();
            assert!(n > 0);
            data.extend_from_slice(&chunk[..n]);
            if data.ends_with(b"hello zug-zug") {
                break;
            }
        }
        assert!(data.starts_with(b"POST / HTTP/1.1"));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    });
    let file =
        std::env::temp_dir().join(format!("horde-scenario-body-{}.json", std::process::id()));
    std::fs::write(&file, serde_json::to_vec(&serde_json::json!({"version":1,"name":"body", "variables":{"name":"zug-zug"}, "steps":[{"name":"post", "url":format!("http://{address}/"), "method":"POST", "body":"hello {{name}}"}]})).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&file)
        .arg("--json")
        .output()
        .unwrap();
    std::fs::remove_file(file).unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scenarios"]["failed"], 0);
}

#[test]
fn scenario_assertions_check_status_body_and_final_url() {
    for (index, status, body, expected, should_fail) in [
        (
            0,
            200,
            "login failed",
            serde_json::json!({"body_contains":"welcome"}),
            true,
        ),
        (
            1,
            401,
            "bad credentials",
            serde_json::json!({"status":[401],"body_contains":"bad credentials"}),
            false,
        ),
        (
            2,
            302,
            "redirect",
            serde_json::json!({"status":[200]}),
            true,
        ),
        (
            3,
            200,
            "welcome",
            serde_json::json!({"final_url":"http://127.0.0.1:1/other"}),
            true,
        ),
        (
            4,
            200,
            "welcome",
            serde_json::json!({"body_contains":"welcome"}),
            false,
        ),
        (
            5,
            200,
            "welcome",
            serde_json::json!({"headers":{"X-Test":"expected"}}),
            false,
        ),
        (
            6,
            200,
            "welcome",
            serde_json::json!({"headers":{"X-Test":"wrong"}}),
            true,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nX-Test: expected\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            let middle = body.len() / 2;
            stream.write_all(&body.as_bytes()[..middle]).unwrap();
            thread::sleep(std::time::Duration::from_millis(5));
            stream.write_all(&body.as_bytes()[middle..]).unwrap();
        });
        let file = std::env::temp_dir().join(format!(
            "horde-assertion-{index}-{}.json",
            std::process::id()
        ));
        let scenario = serde_json::json!({"version":1, "name":"assertions", "steps":[{"name":"check", "url":format!("http://{address}/"), "expect":expected}]});
        std::fs::write(&file, serde_json::to_vec(&scenario).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_horde"))
            .arg("--scenario")
            .arg(&file)
            .args(["--json", "-v"])
            .output()
            .unwrap();
        std::fs::remove_file(file).unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["scenarios"]["failed"],
            usize::from(should_fail),
            "case {index}"
        );
        assert_eq!(report["steps"][0]["status_codes"][status.to_string()], 1);
        if should_fail {
            assert!(String::from_utf8_lossy(&output.stderr).contains("errors="));
        }
    }
}

#[test]
fn scenario_extracts_header_and_json_for_later_url_and_header() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = [0; 4096];
            let n = stream.read(&mut data).unwrap();
            let request = String::from_utf8_lossy(&data[..n]).to_ascii_lowercase();
            if index == 0 {
                let body = r#"{"session":{"token":"zug-zug"}}"#;
                write!(stream, "HTTP/1.1 200 OK\r\nX-Next: /next\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            } else {
                assert!(request.starts_with("get /next "), "{request}");
                assert!(request.contains("x-token: zug-zug"), "{request}");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        }
    });
    let path = std::env::temp_dir().join(format!("horde-extraction-{}.json", std::process::id()));
    let scenario = serde_json::json!({"version":1,"name":"extract", "variables":{"base":format!("http://{addr}")}, "steps":[
        {"name":"first", "url":"{{base}}/", "extract":{"next":{"from":"header","name":"X-Next"}, "token":{"from":"json","pointer":"/session/token"}}},
        {"name":"second", "url":"{{base}}{{next}}", "headers":{"X-Token":"{{token}}"}}
    ]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .args(["--json", "-v"])
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scenarios"]["failed"], 0);
    assert_eq!(report["steps"][1]["completed"], 1);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("zug-zug"));
}

#[test]
fn extracted_values_do_not_leak_into_the_next_iteration() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for index in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            let header = if index == 0 { "X-Token: first\r\n" } else { "" };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\n{header}Content-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
    });
    let path = std::env::temp_dir().join(format!(
        "horde-extraction-reset-{}.json",
        std::process::id()
    ));
    let scenario = serde_json::json!({"version":1,"name":"reset", "steps":[
        {"name":"get", "url":format!("http://{addr}/"), "extract":{"token":{"from":"header","name":"X-Token"}}},
        {"name":"use", "url":format!("http://{addr}/"), "headers":{"X-Token":"{{token}}"}}
    ]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .args(["-n", "2", "-c", "1", "--json", "-v"])
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scenarios"]["failed"], 1);
    assert_eq!(report["steps"][1]["completed"], 1);
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("errors=extraction"), "{log}");
    assert_eq!(report["steps"][0]["failure_details"]["extraction"], 1);
    assert!(!log.contains("first"), "{log}");
}

#[test]
fn invalid_dynamic_url_counts_as_variable_failure_without_status() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut data = [0; 4096];
        assert!(stream.read(&mut data).unwrap() > 0);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nX-Next: not-a-url\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    });
    let path = std::env::temp_dir().join(format!(
        "horde-variable-failure-{}.json",
        std::process::id()
    ));
    let scenario = serde_json::json!({"version":1,"name":"dynamic", "steps":[{"name":"extract","url":format!("http://{addr}/"),"extract":{"next":{"from":"header","name":"X-Next"}}},{"name":"invalid","url":"{{next}}"}]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .arg("--json")
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["steps"][1]["failure_details"]["variable"], 1);
    assert_eq!(report["steps"][1]["status_codes"], serde_json::json!({}));
    assert!(report["steps"][1]["latency_ms"]["p95"].as_f64().is_some());
}

#[test]
fn structured_form_and_json_escape_extracted_values() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for index in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            let mut chunk = [0; 4096];
            let (header_end, body_len) = loop {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                data.extend_from_slice(&chunk[..n]);
                if let Some(end) = data.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:").map(str::trim))
                        .map_or(0, |value| value.parse::<usize>().unwrap());
                    if data.len() >= end + 4 + length {
                        break (end, length);
                    }
                }
            };
            let headers = String::from_utf8_lossy(&data[..header_end]).to_ascii_lowercase();
            let body = &data[header_end + 4..header_end + 4 + body_len];
            if index == 0 {
                let payload = serde_json::json!({"token":"a&b= c\"é"}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                )
                .unwrap();
            } else if index == 1 {
                assert!(
                    headers.contains("content-type: application/x-www-form-urlencoded"),
                    "{headers}"
                );
                let form: std::collections::BTreeMap<String, String> =
                    serde_urlencoded::from_bytes(body).unwrap();
                assert_eq!(form["token"], "a&b= c\"é");
                assert_eq!(form["literal"], "one & two");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            } else {
                assert!(
                    headers.contains("content-type: application/custom+json"),
                    "{headers}"
                );
                let json: serde_json::Value = serde_json::from_slice(body).unwrap();
                assert_eq!(json, serde_json::json!({"items":["a&b= c\"é",true,42]}));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        }
    });
    let path = std::env::temp_dir().join(format!("horde-structured-{}.json", std::process::id()));
    let scenario = serde_json::json!({"version":1,"name":"structured", "steps":[
        {"name":"get","url":format!("http://{addr}/"),"extract":{"token":{"from":"json","pointer":"/token"}}},
        {"name":"form","url":format!("http://{addr}/"),"method":"POST","form":{"token":"{{token}}","literal":"one & two"}},
        {"name":"json","url":format!("http://{addr}/"),"method":"POST","headers":{"Content-Type":"application/custom+json"},"json_body":{"items":["{{token}}",true,42]}}
    ]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .args(["--json", "-v"])
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scenarios"]["failed"], 0);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("a&b="));
}

#[test]
fn extraction_accepts_limit_and_rejects_larger_bodies() {
    for (extra, expected_failure) in [(0, false), (1, true)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            let prefix = b"{\"token\":\"ok\",\"padding\":\"";
            let suffix = b"\"}";
            let size = 2 * 1024 * 1024 + extra;
            let mut body = Vec::with_capacity(size);
            body.extend_from_slice(prefix);
            body.resize(size - suffix.len(), b'x');
            body.extend_from_slice(suffix);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            let _ = stream.write_all(&body);
        });
        let path = std::env::temp_dir().join(format!(
            "horde-extraction-limit-{}.json",
            std::process::id()
        ));
        let scenario = serde_json::json!({"version":1,"name":"limit", "steps":[{"name":"read", "url":format!("http://{addr}/"), "extract":{"token":{"from":"json","pointer":"/token"}}}]});
        std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_horde"))
            .arg("--scenario")
            .arg(&path)
            .args(["--json", "-v"])
            .output()
            .unwrap();
        std::fs::remove_file(path).unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["scenarios"]["failed"], usize::from(expected_failure));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).contains("errors=extraction"),
            expected_failure
        );
    }
}
