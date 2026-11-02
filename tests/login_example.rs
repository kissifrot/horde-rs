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
fn login_example_requires_authentication_and_keeps_sessions() {
    let mut server = DemoServer(
        Command::new("python3")
            .args(["examples/login_server.py", "--port", "0"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let base = line.trim().strip_prefix("Listening on ").unwrap();
    let address = base.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "GET /protected HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.0 401"), "{response}");

    let mut scenario: serde_json::Value =
        serde_json::from_str(include_str!("../examples/login.json")).unwrap();
    scenario["variables"]["base_url"] = base.into();
    let path =
        std::env::temp_dir().join(format!("horde-login-example-{}.json", std::process::id()));
    for (password, expected_failures, protected_completed) in [("zugzug", 0, 2), ("wrong", 2, 0)] {
        scenario["variables"]["password"] = password.into();
        std::fs::write(&path, serde_json::to_vec(&scenario).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_horde"))
            .arg("--scenario")
            .arg(&path)
            .args(["-n", "2", "-c", "2", "--json"])
            .output()
            .unwrap();
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
