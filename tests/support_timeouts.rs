mod common;

use std::{
    net::TcpListener,
    time::{Duration, Instant},
};

#[test]
fn missing_connection_returns_a_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let start = Instant::now();
    let error = common::accept_with_timeout(&listener, Duration::from_millis(50)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn stalled_process_is_stopped_at_the_deadline() {
    let start = Instant::now();
    let error = common::output_with_timeout(
        std::process::Command::new("sh").args(["-c", "exec sleep 5"]),
        Duration::from_millis(100),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(2));
}
