use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

#[test]
fn environment_values_expand_in_urls_headers_forms_and_assertions() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("POST /login "), "{line}");
        let mut length = 0;
        let mut token = None;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
            if name.eq_ignore_ascii_case("x-token") {
                token = Some(value.trim().to_owned());
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        assert_eq!(token.as_deref(), Some("secret-value"));
        assert_eq!(body, b"password=secret-value&username=user+%26%2B");
        write!(
            reader.get_mut(),
            "HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nsecret-value"
        )
        .unwrap();
    });
    let path = std::env::temp_dir().join(format!("horde-scenario-env-{}.json", std::process::id()));
    let scenario = serde_json::json!({"version":1,"name":"env", "variables": {
        "base":{"env":"HORDE_TEST_ENV_BASE"}, "user":{"env":"HORDE_TEST_ENV_USER"},
        "secret":{"env":"HORDE_TEST_ENV_SECRET"}}, "steps":[{
        "name":"login", "url":"{{base}}/login", "method":"POST", "headers":{"X-Token":"{{secret}}"},
        "form":{"username":"{{user}}", "password":"{{secret}}"}, "expect":{"body_contains":"{{secret}}"}
    }]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .args(["--json", "--verbose"])
        .env("HORDE_TEST_ENV_BASE", format!("http://{address}"))
        .env("HORDE_TEST_ENV_USER", "user &+")
        .env("HORDE_TEST_ENV_SECRET", "secret-value")
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
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret-value"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-value"));
}

#[test]
fn missing_environment_variable_fails_before_requests() {
    let path = std::env::temp_dir().join(format!(
        "horde-scenario-env-missing-{}.json",
        std::process::id()
    ));
    let scenario = serde_json::json!({"version":1,"name":"env", "variables":{
        "secret":{"env":"HORDE_TEST_ENV_MISSING"}}, "steps":[{"name":"one","url":"http://127.0.0.1:9/"}]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .args(["--json", "--verbose"])
        .env_remove("HORDE_TEST_ENV_MISSING")
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("environment variable HORDE_TEST_ENV_MISSING is not set"),
        "{error}"
    );
    assert!(!error.contains("[user "), "{error}");
}

#[cfg(unix)]
#[test]
fn non_utf8_environment_variable_is_rejected_without_its_value() {
    use std::os::unix::ffi::OsStringExt;
    let path = std::env::temp_dir().join(format!(
        "horde-scenario-env-invalid-{}.json",
        std::process::id()
    ));
    let scenario = serde_json::json!({"version":1,"name":"env", "variables":{
        "secret":{"env":"HORDE_TEST_ENV_INVALID"}}, "steps":[{"name":"one","url":"http://127.0.0.1:9/"}]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .env(
            "HORDE_TEST_ENV_INVALID",
            std::ffi::OsString::from_vec(b"sensitive\xff".to_vec()),
        )
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("environment variable HORDE_TEST_ENV_INVALID is not valid UTF-8"),
        "{error}"
    );
    assert!(!error.contains("sensitive"), "{error}");
}
