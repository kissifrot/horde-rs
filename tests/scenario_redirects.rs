use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

#[test]
fn standalone_redirect_restricts_referer_and_sensitive_headers() {
    let site = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let site_addr = site.local_addr().unwrap();
    let target_addr = target.local_addr().unwrap();
    let site_server = thread::spawn(move || {
        let (mut stream, _) = site.accept().unwrap();
        let mut buffer = [0; 4096];
        let n = stream.read(&mut buffer).unwrap();
        assert!(String::from_utf8_lossy(&buffer[..n]).contains("secret=private"));
        write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let target_server = thread::spawn(move || {
        let (mut stream, _) = target.accept().unwrap();
        let mut buffer = [0; 4096];
        let n = stream.read(&mut buffer).unwrap();
        let request = String::from_utf8_lossy(&buffer[..n]).to_ascii_lowercase();
        assert!(
            request.contains(&format!("referer: http://{site_addr}/\r\n")),
            "{request}"
        );
        assert!(!request.contains("secret=private"), "{request}");
        assert!(
            !request.contains("authorization:") && !request.contains("cookie:"),
            "{request}"
        );
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args([
            "-L",
            "--json",
            "-H",
            "Authorization: Bearer secret",
            "-H",
            "Cookie: secret=private",
        ])
        .arg(format!("http://{site_addr}/private?secret=private"))
        .output()
        .unwrap();
    site_server.join().unwrap();
    target_server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["completed"], 1);
    assert_eq!(report["failed"], 0);
    assert_eq!(report["redirects_followed"], 1);
}

#[test]
fn redirect_chain_uses_one_timeout_including_final_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let mut buffer = [0; 4096];
        assert!(first.read(&mut buffer).unwrap() > 0);
        thread::sleep(std::time::Duration::from_millis(600));
        write!(first, "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        let (mut second, _) = listener.accept().unwrap();
        assert!(second.read(&mut buffer).unwrap() > 0);
        write!(
            second,
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        thread::sleep(std::time::Duration::from_millis(650));
        let _ = second.write_all(b"x");
    });
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args(["-L", "--json", "-t", "1"])
        .arg(format!("http://{addr}/start"))
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["failed"], 1);
    assert_eq!(report["failure_details"]["timeout"], 1);
    assert_eq!(report["redirects_followed"], 1);
}

#[test]
fn auto_referer_uses_final_redirect_url_and_respects_explicit_header() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for index in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 4096];
            let count = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..count]).to_ascii_lowercase();
            match index {
                0 => {
                    assert!(!request.contains("referer:"));
                    write!(stream, "HTTP/1.1 302 Found\r\nLocation: /landed?from=redirect\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
                1 => {
                    assert!(
                        request.contains(&format!("referer: http://{addr}/start")),
                        "{request}"
                    );
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
                2 => {
                    assert!(
                        request.contains(&format!("referer: http://{addr}/landed?from=redirect")),
                        "{request}"
                    );
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
                _ => {
                    assert!(
                        request.contains("referer: https://explicit.example/path"),
                        "{request}"
                    );
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
            }
        }
    });
    let path = std::env::temp_dir().join(format!(
        "horde-scenario-referer-{}.json",
        std::process::id()
    ));
    let scenario = serde_json::json!({"version":1, "name":"referer", "steps":[
        {"name":"redirect", "url":format!("http://{addr}/start")},
        {"name":"next", "url":format!("http://{addr}/next")},
        {"name":"explicit", "url":format!("http://{addr}/last"), "headers":{"Referer":"https://explicit.example/path"}}
    ]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .args(["--auto-referer", "-L", "--json"])
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
}

#[test]
fn scenario_redirects_are_not_followed_by_default() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0; 4096];
        assert!(stream.read(&mut buffer).unwrap() > 0);
        write!(stream, "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let path = std::env::temp_dir().join(format!(
        "horde-scenario-no-redirect-{}.json",
        std::process::id()
    ));
    std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"version":1,"name":"redirect", "steps":[{"name":"login", "url":format!("http://{addr}/")}] })).unwrap()).unwrap();
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
    assert_eq!(report["scenarios"]["failed"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("completed status=302"));
}

#[test]
fn scenario_follows_redirects_across_site_and_authentication_server() {
    let site = TcpListener::bind("127.0.0.1:0").unwrap();
    let auth = TcpListener::bind("127.0.0.1:0").unwrap();
    let site_addr = site.local_addr().unwrap();
    let auth_port = auth.local_addr().unwrap().port();
    let site_server = thread::spawn(move || {
        let (mut first, _) = site.accept().unwrap();
        let mut buffer = [0; 4096];
        assert!(first.read(&mut buffer).unwrap() > 0);
        write!(first, "HTTP/1.1 302 Found\r\nLocation: http://localhost:{auth_port}/authorize\r\nSet-Cookie: site_session=site; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        let (mut callback, _) = site.accept().unwrap();
        let read = callback.read(&mut buffer).unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase();
        assert!(request.starts_with("get /callback "), "{request}");
        assert!(
            request.contains(&format!("referer: http://localhost:{auth_port}/")),
            "{request}"
        );
        assert!(request.contains("site_session=site"), "{request}");
        assert!(!request.contains("auth_session=auth"), "{request}");
        write!(
            callback,
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    });
    let auth_server = thread::spawn(move || {
        let (mut stream, _) = auth.accept().unwrap();
        let mut buffer = [0; 4096];
        let read = stream.read(&mut buffer).unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase();
        assert!(request.starts_with("get /authorize "), "{request}");
        assert!(
            request.contains(&format!("referer: http://{site_addr}/")),
            "{request}"
        );
        assert!(!request.contains("site_session=site"), "{request}");
        assert!(!request.contains("authorization:"), "{request}");
        write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://{site_addr}/callback\r\nSet-Cookie: auth_session=auth; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let path = std::env::temp_dir().join(format!(
        "horde-scenario-redirect-{}.json",
        std::process::id()
    ));
    let scenario = serde_json::json!({"version":1,"name":"redirect", "steps":[{"name":"login", "url":format!("http://{site_addr}/login?secret=private"), "headers":{"Authorization":"Bearer example"}}]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .args(["-L", "--json", "-v"])
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    site_server.join().unwrap();
    auth_server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scenarios"]["failed"], 0);
    assert_eq!(report["steps"][0]["completed"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("completed status=200"));
}

#[test]
fn scenario_redirect_limit_fails_the_step() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 4096];
            assert!(stream.read(&mut buffer).unwrap() > 0);
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        }
    });
    let path = std::env::temp_dir().join(format!(
        "horde-scenario-redirect-limit-{}.json",
        std::process::id()
    ));
    let scenario = serde_json::json!({"version":1,"name":"loop", "steps":[{"name":"loop", "url":format!("http://{addr}/")}]});
    std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .arg("--scenario")
        .arg(&path)
        .args(["-L", "--max-redirects", "1", "--json", "-v"])
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scenarios"]["failed"], 1);
    assert_eq!(report["steps"][0]["failed"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("errors=redirect"));
}
