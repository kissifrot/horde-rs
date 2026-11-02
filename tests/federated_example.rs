use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
};

struct DemoServer(Child);
impl Drop for DemoServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn federated_example_redirects_and_requires_correct_credentials() {
    let mut server = DemoServer(
        Command::new("python3")
            .args([
                "examples/federated_login_server.py",
                "--site-port",
                "0",
                "--auth-port",
                "0",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addresses: serde_json::Value = serde_json::from_str(&line).unwrap();
    let site_url = addresses["site_url"].as_str().unwrap();
    let auth_url = addresses["auth_url"].as_str().unwrap();

    let address = site_url.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "GET /protected HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.0 401"));

    let mut scenario: serde_json::Value =
        serde_json::from_str(include_str!("../examples/federated-login.json")).unwrap();
    scenario["variables"]["site_url"] = site_url.into();
    scenario["variables"]["auth_url"] = auth_url.into();
    let path = std::env::temp_dir().join(format!(
        "horde-federated-example-{}.json",
        std::process::id()
    ));
    for (password, follow, expected_failures, protected_completed) in [
        ("zugzug", true, 0, 2),
        ("wrong", true, 2, 0),
        ("zugzug", false, 2, 0),
    ] {
        scenario["variables"]["password"] = password.into();
        std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_horde"));
        command
            .arg("--scenario")
            .arg(&path)
            .args(["-n", "2", "-c", "2", "--json"]);
        if follow {
            command.arg("-L");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["scenarios"]["failed"], expected_failures);
        assert_eq!(report["steps"][2]["completed"], protected_completed);
    }
    std::fs::remove_file(path).unwrap();
}
