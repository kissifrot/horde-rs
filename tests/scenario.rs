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
                    assert!(request.contains("x-test: zug-zug"), "{request}");
                    assert!(!request.contains("cookie:"));
                    let id = ids.fetch_add(1, Ordering::Relaxed) + 1;
                    thread::sleep(std::time::Duration::from_millis(75));
                    write!(stream, "HTTP/1.1 200 OK\r\nSet-Cookie: session={id}; Path=/; HttpOnly\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    assert!(request.starts_with("get /next "), "{request}");
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
        .args(["-n", "2", "-c", "2", "--json", "--verbose"])
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
    assert_eq!(report["steps"][1]["completed"], 0);
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("step \"first\" completed status=500"), "{log}");
    assert!(log.contains("errors=http"), "{log}");
    assert!(log.contains("] failed"), "{log}");
    assert!(!log.contains("skipped"), "{log}");
    assert!(report["steps"][1]["mean_latency_ms"].is_null());
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
