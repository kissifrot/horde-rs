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
    assert!(stdout.contains("Completed requests: 4"), "{stdout}");
    assert!(stdout.contains("Failed requests:    1"), "{stdout}");
    assert!(stdout.contains("Response bytes:    8"), "{stdout}");
}

#[test]
fn rejects_invalid_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_horde"))
        .args(["-n", "0", "http://localhost/"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
