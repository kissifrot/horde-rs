use std::{path::Path, process::Command};

fn run(path: &Path, content: &str) -> String {
    std::fs::write(path, content).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(error.starts_with("Error: scenario "), "{error}");
    assert!(error.contains(&path.display().to_string()), "{error}");
    assert!(!error.contains("SAMPLE_SECRET_VALUE"), "{error}");
    error
}

#[test]
fn diagnostics_identify_file_schema_and_semantic_errors_without_values() {
    let path = std::env::temp_dir().join(format!("horde-diagnostics-{}.json", std::process::id()));
    let error = run(&path, "{\n\"version\": 1,\n\"steps\": [}");
    assert!(error.contains("invalid JSON at line 3"), "{error}");
    assert!(error.contains("expected a JSON value"), "{error}");
    let error = run(
        &path,
        r#"{"version":1,"name":"x","steps":[{"name":"one","url":"http://localhost/","wait_ms":1234567.0}]}"#,
    );
    assert!(!error.contains("1234567"), "{error}");
    let cases = [
        (
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"one","url":"http://localhost/","waait_ms":3}]}),
            vec!["/steps/0", "waait_ms"],
        ),
        (
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"one","url":"http://localhost/","form":"SAMPLE_SECRET_VALUE"}]}),
            vec!["/steps/0/form", "object"],
        ),
        (
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"one","url":"http://localhost/","extract":{"token":{"from":"json","pointr":"SAMPLE_SECRET_VALUE"}}}]}),
            vec!["/steps/0/extract/token", "pointer"],
        ),
        (
            serde_json::json!({"version":1,"name":"x","variables":{"token":{"env":"BAD-NAME"}},"steps":[{"name":"one","url":"http://localhost/"}]}),
            vec!["/variables/token", "environment variable"],
        ),
        (
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"one","url":"http://localhost/"},{"name":"two","url":"http://localhost/{{missing}}"}]}),
            vec!["step 2", "two", "url", "missing"],
        ),
        (
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"same","url":"http://localhost/"},{"name":"same","url":"http://localhost/"}]}),
            vec!["step 2", "same", "name", "unique"],
        ),
        (
            serde_json::json!({"version":1,"name":"x","variables":{"secret":"SAMPLE_SECRET_VALUE"},"steps":[{"name":"one","url":"{{secret}}"}]}),
            vec!["step 1", "url", "absolute HTTP"],
        ),
        (
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"one","url":"http://localhost/","method":"BAD METHOD"}]}),
            vec!["step 1", "method", "invalid HTTP method"],
        ),
        (
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"one","url":"http://localhost/","extract":{"token":{"from":"json","pointer":"wrong~9"}}}]}),
            vec!["step 1", "extract", "token", "JSON pointer"],
        ),
        (
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"one","url":"http://localhost/","extract":{"token":{"from":"html","selector":"[","attribute":"value"}}}]}),
            vec!["step 1", "extract", "token", "HTML selector"],
        ),
    ];
    for (case, expected) in cases {
        let error = run(&path, &serde_json::to_string(&case).unwrap());
        for text in expected {
            assert!(error.contains(text), "missing {text:?}: {error}");
        }
    }
    std::fs::remove_file(&path).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("cannot read scenario") && error.contains(&path.display().to_string()),
        "{error}"
    );
}

#[test]
fn verbose_extraction_diagnostics_do_not_expose_response_values() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    let cases = [
        (
            "invalid_json",
            "SAMPLE_SECRET_VALUE",
            serde_json::json!({"from":"json","pointer":"/token"}),
            "response is not valid JSON",
        ),
        (
            "missing_json",
            r#"{"value":"SAMPLE_SECRET_VALUE"}"#,
            serde_json::json!({"from":"json","pointer":"/token"}),
            "JSON pointer does not identify a string",
        ),
        (
            "missing_html",
            "<input value='SAMPLE_SECRET_VALUE'>",
            serde_json::json!({"from":"html","selector":"input","attribute":"data-token"}),
            "HTML element or attribute missing",
        ),
    ];
    for (case, body, extraction, expected) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let path = std::env::temp_dir().join(format!(
            "horde-extract-diagnostics-{case}-{}.json",
            std::process::id()
        ));
        let scenario = serde_json::json!({"version":1,"name":"diagnostics","steps":[{
            "name":"read", "url":format!("http://{address}/"), "extract":{"token":extraction}
        }]});
        std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_horde"))
            .arg("--scenario")
            .arg(&path)
            .args(["--json", "--verbose"])
            .output()
            .unwrap();
        std::fs::remove_file(path).unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        let log = String::from_utf8_lossy(&output.stderr);
        assert!(
            log.contains("errors=extraction")
                && log.contains("extraction_variable=\"token\"")
                && log.contains(expected),
            "{log}"
        );
        assert!(!log.contains("SAMPLE_SECRET_VALUE"), "{log}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SAMPLE_SECRET_VALUE"));
    }
}
