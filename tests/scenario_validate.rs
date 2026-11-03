use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

#[test]
fn validation_checks_scenario_without_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let connections = Arc::new(AtomicUsize::new(0));
    let server = {
        let stop = Arc::clone(&stop);
        let connections = Arc::clone(&connections);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        connections.fetch_add(1, Ordering::Relaxed);
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut request = [0; 4096];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        })
    };
    let path = std::env::temp_dir().join(format!("horde-validate-{}.json", std::process::id()));
    let scenario = serde_json::json!({"version":1,"name":"check", "variables":{
        "secret":{"env":"HORDE_VALIDATE_TEST_SECRET"}}, "steps":[{
        "name":"one", "url":format!("http://{address}/"), "headers":{"X-Token":"{{secret}}"}
    }]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let run = |args: &[&str], secret: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
        command.arg("--scenario").arg(&path).args(args);
        if let Some(secret) = secret {
            command.env("HORDE_VALIDATE_TEST_SECRET", secret);
        } else {
            command.env_remove("HORDE_VALIDATE_TEST_SECRET");
        }
        command.output().unwrap()
    };
    let text = run(&["--validate"], Some("SAMPLE_SECRET_VALUE"));
    let json = run(&["--validate", "--json"], Some("SAMPLE_SECRET_VALUE"));
    let missing = run(&["--validate"], None);
    let incompatible = run(&["--validate", "-n", "2"], Some("SAMPLE_SECRET_VALUE"));
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert_eq!(connections.load(Ordering::Relaxed), 0);
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    assert_eq!(text.stdout, b"Scenario is valid.\n");
    assert!(json.status.success());
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"schema_version":1,"type":"scenario_validation","valid":true})
    );
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("HORDE_VALIDATE_TEST_SECRET is not set")
    );
    assert!(!incompatible.status.success());
    for output in [&text, &json, &missing, &incompatible] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SAMPLE_SECRET_VALUE"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SAMPLE_SECRET_VALUE"));
    }
    std::fs::remove_file(path).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--validate")
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn validation_reports_invalid_scenario() {
    let path = std::env::temp_dir().join(format!(
        "horde-validate-invalid-{}.json",
        std::process::id()
    ));
    std::fs::write(
        &path,
        r#"{"version":1,"name":"invalid","steps":[{"name":"one","url":"not-a-url"}]}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .args(["--validate", "--json"])
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("expected an absolute HTTP or HTTPS URL")
    );
}
