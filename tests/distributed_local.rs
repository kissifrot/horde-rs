#![cfg(feature = "distributed-offline")]

mod common;
use horde_rs::{
    TestPlan,
    distributed::{
        self, AgentLimits, AgentStatus, Aggregate, Budget, DecodeLimits, GruntPolicy, LocalOutcome,
        ResourceLimits, StopReason, ThresholdResult,
    },
};
use reqwest::{Method, Url, header::HeaderMap};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn plan(port: u16, timeout: Duration) -> TestPlan {
    TestPlan {
        url: Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
        requests: 1,
        concurrency: 1,
        timeout,
        method: Method::GET,
        headers: HeaderMap::new(),
        body: None,
        duration: None,
        rate: None,
        verbose: false,
        bounded_latency: false,
        reuse_connections: true,
        follow_redirects: false,
        max_redirects: 10,
    }
}
fn resources() -> ResourceLimits {
    ResourceLimits {
        max_active_runs: 2,
        max_active_histogram_bytes: 8 * 1024 * 1024,
        max_histogram_bytes: distributed::MAX_ENVELOPE_BYTES,
        max_request_timeout: Duration::from_secs(10),
        max_redirects: 10,
        max_body_bytes: 1024,
        max_header_bytes: 1024,
    }
}

fn policy(urls: Vec<Url>, resources: ResourceLimits) -> GruntPolicy {
    GruntPolicy::new(
        urls,
        vec!["127.0.0.1/32".parse().unwrap(), "::1/128".parse().unwrap()],
        resources,
    )
    .unwrap()
}

async fn run_local(
    plan: TestPlan,
    budget: Budget,
    limits: AgentLimits,
    cancel: distributed::Cancellation,
) -> Result<LocalOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let policy = policy(vec![plan.url.clone()], resources());
    distributed::run_local(plan, budget, limits, &policy, cancel).await
}

fn budget() -> Budget {
    Budget {
        requests: Some(1),
        concurrency: 1,
        rate_per_second: 1000,
        duration: None,
    }
}
fn limits(deadline: Duration) -> AgentLimits {
    AgentLimits {
        max_samples: 100,
        max_concurrency: 2,
        max_rate_per_second: 1000,
        max_duration: deadline,
    }
}
fn listener() -> TcpListener {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(false).unwrap();
    l
}
fn read_request(stream: &mut std::net::TcpStream) {
    let mut data = [0; 4096];
    assert!(stream.read(&mut data).unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duration_drains_the_started_request() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&server);
        read_request(&mut stream);
        thread::sleep(Duration::from_millis(150));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let rx = distributed::Cancellation::new();
    let result = run_local(
        plan(port, Duration::from_secs(1)),
        Budget {
            requests: None,
            duration: Some(Duration::from_millis(60)),
            ..budget()
        },
        limits(Duration::from_secs(2)),
        rx,
    )
    .await
    .unwrap();
    match result {
        LocalOutcome::Complete {
            report,
            reason,
            started,
        } => {
            assert_eq!(reason, StopReason::Duration);
            assert_eq!(started, 1);
            assert_eq!(report.completed, 1);
            assert_eq!(report.failed, 0);
        }
        _ => panic!("expected normal drain"),
    }
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_aborts_and_joins_request_with_partial_body() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let (done_tx, done_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&server);
        read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nx")
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0u8; 1];
        let closed = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            _ => false,
        };
        done_tx.send(closed).unwrap();
    });
    let rx = distributed::Cancellation::new();
    let started = Instant::now();
    let result = run_local(
        plan(port, Duration::from_secs(3)),
        budget(),
        limits(Duration::from_millis(180)),
        rx,
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        LocalOutcome::Partial {
            reason: StopReason::AgentDeadline
        }
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(done_rx.recv_timeout(Duration::from_secs(3)).unwrap());
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_aborts_and_joins_request_with_partial_body() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&server);
        read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nx")
            .unwrap();
        ready_tx.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0u8; 1];
        let closed = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            _ => false,
        };
        closed_tx.send(closed).unwrap();
    });
    let tx = distributed::Cancellation::new();
    let rx = tx.clone();
    let task = tokio::spawn(run_local(
        plan(port, Duration::from_secs(3)),
        budget(),
        limits(Duration::from_secs(4)),
        rx,
    ));
    ready_rx.await.unwrap();
    tx.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        LocalOutcome::Partial {
            reason: StopReason::Cancelled
        }
    ));
    assert!(closed_rx.recv_timeout(Duration::from_secs(3)).unwrap());
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_before_start_never_opens_connection() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    server.set_nonblocking(true).unwrap();
    let rx = distributed::Cancellation::new();
    rx.cancel();
    let result = run_local(
        plan(port, Duration::from_secs(1)),
        budget(),
        limits(Duration::from_secs(2)),
        rx,
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        LocalOutcome::Partial {
            reason: StopReason::Cancelled
        }
    ));
    assert_eq!(
        server.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_timeout_before_deadline_is_a_completed_failure() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&server);
        read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nx")
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0u8; 1];
        let _ = stream.read(&mut byte);
    });
    let rx = distributed::Cancellation::new();
    let result = run_local(
        plan(port, Duration::from_millis(80)),
        budget(),
        limits(Duration::from_secs(2)),
        rx,
    )
    .await
    .unwrap();
    match result {
        LocalOutcome::Complete {
            report,
            reason,
            started,
        } => {
            assert_eq!(reason, StopReason::RequestBudget);
            assert_eq!(started, 1);
            assert_eq!(report.completed, 1);
            assert_eq!(report.failed, 1);
            assert_eq!(report.timeout_failures, 1);
        }
        _ => panic!("expected completed timeout"),
    }
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_during_rate_wait_does_not_launch_reserved_request() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (extra_tx, extra_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&server);
        read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        ready_tx.send(()).unwrap();
        server.set_nonblocking(true).unwrap();
        thread::sleep(Duration::from_millis(250));
        extra_tx.send(server.accept().is_ok()).unwrap();
    });
    let tx = distributed::Cancellation::new();
    let rx = tx.clone();
    let task = tokio::spawn(run_local(
        plan(port, Duration::from_secs(3)),
        Budget {
            requests: Some(2),
            concurrency: 2,
            rate_per_second: 1,
            duration: None,
        },
        limits(Duration::from_secs(4)),
        rx,
    ));
    ready_rx.await.unwrap();
    tx.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        LocalOutcome::Partial {
            reason: StopReason::Cancelled
        }
    ));
    assert!(!extra_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duration_stops_a_reserved_rate_slot_and_counts_actual_starts() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&server);
        read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let rx = distributed::Cancellation::new();
    let result = run_local(
        plan(port, Duration::from_secs(1)),
        Budget {
            requests: Some(2),
            concurrency: 2,
            rate_per_second: 1,
            duration: Some(Duration::from_millis(150)),
        },
        limits(Duration::from_secs(2)),
        rx,
    )
    .await
    .unwrap();
    match result {
        LocalOutcome::Complete {
            report,
            reason,
            started,
        } => {
            assert_eq!(reason, StopReason::Duration);
            assert_eq!(started, 1);
            assert_eq!(report.completed, 1);
            assert!(report.elapsed >= Duration::from_millis(150));
        }
        _ => panic!("expected complete duration"),
    }
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duration_only_run_marks_sample_cap_partial() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&server);
        read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let rx = distributed::Cancellation::new();
    let result = run_local(
        plan(port, Duration::from_secs(1)),
        Budget {
            requests: None,
            duration: Some(Duration::from_secs(1)),
            ..budget()
        },
        AgentLimits {
            max_samples: 1,
            ..limits(Duration::from_secs(2))
        },
        rx,
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        LocalOutcome::Partial {
            reason: StopReason::SampleLimit
        }
    ));
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_cancels_a_redirect_chain_and_closes_the_connection() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut first, _) = common::accept(&server);
        read_request(&mut first);
        first.write_all(b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        drop(first);
        let (mut second, _) = common::accept(&server);
        read_request(&mut second);
        second
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\nx")
            .unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0u8; 1];
        let closed = match second.read(&mut byte) {
            Ok(0) => true,
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            _ => false,
        };
        closed_tx.send(closed).unwrap();
    });
    let rx = distributed::Cancellation::new();
    let mut p = plan(port, Duration::from_secs(3));
    p.follow_redirects = true;
    let result = run_local(p, budget(), limits(Duration::from_millis(200)), rx)
        .await
        .unwrap();
    assert!(matches!(
        result,
        LocalOutcome::Partial {
            reason: StopReason::AgentDeadline
        }
    ));
    assert!(closed_rx.recv_timeout(Duration::from_secs(3)).unwrap());
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sample_cap_wakes_other_workers_waiting_for_rate_slots() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&server);
        read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let rx = distributed::Cancellation::new();
    let budget = Budget {
        requests: None,
        concurrency: 3,
        rate_per_second: 1,
        duration: Some(Duration::from_secs(5)),
    };
    let mut local_limits = limits(Duration::from_secs(6));
    local_limits.max_concurrency = 3;
    local_limits.max_samples = 1;
    let started = Instant::now();
    let result = run_local(plan(port, Duration::from_secs(2)), budget, local_limits, rx)
        .await
        .unwrap();
    assert!(matches!(
        result,
        LocalOutcome::Partial {
            reason: StopReason::SampleLimit
        }
    ));
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "workers did not wake after the sample cap"
    );
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_before_response_headers_closes_connection() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&server);
        read_request(&mut stream);
        ready_tx.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0u8; 1];
        let closed = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            _ => false,
        };
        closed_tx.send(closed).unwrap();
    });
    let tx = distributed::Cancellation::new();
    let rx = tx.clone();
    let task = tokio::spawn(run_local(
        plan(port, Duration::from_secs(3)),
        budget(),
        limits(Duration::from_secs(4)),
        rx,
    ));
    ready_rx.await.unwrap();
    tx.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        LocalOutcome::Partial {
            reason: StopReason::Cancelled
        }
    ));
    assert!(closed_rx.recv_timeout(Duration::from_secs(3)).unwrap());
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_joins_all_concurrent_in_flight_requests() {
    let server = listener();
    let port = server.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut handlers = Vec::new();
        let (headers_tx, headers_rx) = mpsc::channel();
        for _ in 0..4 {
            let (mut stream, _) = common::accept(&server);
            let closed_tx = closed_tx.clone();
            let headers_tx = headers_tx.clone();
            handlers.push(thread::spawn(move || {
                read_request(&mut stream);
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\nx")
                    .unwrap();
                headers_tx.send(()).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut byte = [0u8; 1];
                let closed = match stream.read(&mut byte) {
                    Ok(0) => true,
                    Err(e) => matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ),
                    _ => false,
                };
                closed_tx.send(closed).unwrap();
            }));
        }
        for _ in 0..4 {
            headers_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        ready_tx.send(()).unwrap();
        for handler in handlers {
            handler.join().unwrap();
        }
        server.set_nonblocking(true).unwrap();
        assert_eq!(
            server.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
    let tx = distributed::Cancellation::new();
    let rx = tx.clone();
    let budget = Budget {
        requests: Some(100),
        concurrency: 4,
        rate_per_second: 1000,
        duration: None,
    };
    let mut local_limits = limits(Duration::from_secs(5));
    local_limits.max_concurrency = 4;
    let task = tokio::spawn(run_local(
        plan(port, Duration::from_secs(4)),
        budget,
        local_limits,
        rx,
    ));
    ready_rx.await.unwrap();
    tx.cancel();
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        LocalOutcome::Partial {
            reason: StopReason::Cancelled
        }
    ));
    for _ in 0..4 {
        assert!(closed_rx.recv_timeout(Duration::from_secs(3)).unwrap());
    }
    server.join().unwrap();
}

fn decode_limits(agent_limits: AgentLimits) -> DecodeLimits {
    DecodeLimits {
        max_envelope_bytes: distributed::MAX_ENVELOPE_BYTES,
        max_samples: agent_limits.max_samples,
        assigned_requests: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_grunts_split_budget_and_merge_observed_results() {
    let shares = distributed::split_budget(
        Budget {
            requests: Some(41),
            concurrency: 2,
            rate_per_second: 1999,
            duration: None,
        },
        2,
    )
    .unwrap();
    assert_eq!(
        shares
            .iter()
            .map(|b| (b.requests, b.concurrency, b.rate_per_second))
            .collect::<Vec<_>>(),
        [(Some(21), 1, 1000), (Some(20), 1, 999)]
    );

    let serve = |requests: usize, slow: bool| {
        let server = listener();
        let port = server.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            for index in 0..requests {
                let (mut stream, _) = common::accept(&server);
                read_request(&mut stream);
                let response = if slow && index >= requests - 2 {
                    thread::sleep(Duration::from_millis(250));
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno".as_slice()
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx".as_slice()
                };
                stream.write_all(response).unwrap();
            }
        });
        (port, worker)
    };
    let (fast_port, fast_server) = serve(21, false);
    let (slow_port, slow_server) = serve(20, true);
    let agent_limits = limits(Duration::from_secs(10));
    let (fast, slow) = tokio::join!(
        run_local(
            plan(fast_port, Duration::from_secs(3)),
            shares[0],
            agent_limits,
            distributed::Cancellation::new()
        ),
        run_local(
            plan(slow_port, Duration::from_secs(3)),
            shares[1],
            agent_limits,
            distributed::Cancellation::new()
        ),
    );
    fast_server.join().unwrap();
    slow_server.join().unwrap();

    let run_id = [1; 16];
    let agents = ["fast".to_owned(), "slow".to_owned()];
    let mut aggregate = Aggregate::new(run_id, &agents).unwrap();
    let slow = slow.unwrap();
    let LocalOutcome::Complete { report, .. } = &slow else {
        panic!("expected a complete Grunt outcome");
    };
    let slow_p95 = report.percentile(95).unwrap();
    assert!(slow_p95 >= Duration::from_millis(250));

    for (index, outcome) in [fast.unwrap(), slow].into_iter().enumerate() {
        aggregate
            .add_local_outcome(
                run_id,
                &agents[index],
                outcome,
                shares[index],
                decode_limits(agent_limits),
            )
            .unwrap();
    }
    assert!(!aggregate.is_partial());
    let metrics = aggregate.metrics();
    assert_eq!(
        (metrics.started, aggregate.completed(), aggregate.failed()),
        (41, 41, 2)
    );
    assert_eq!(metrics.http_failures, 2);
    assert_eq!(metrics.response_bytes, 43);
    assert_eq!(metrics.connections_opened, 41);
    assert_eq!(metrics.status_codes, [(200, 39), (503, 2)].into());
    for agent in &agents {
        assert_eq!(aggregate.agents()[agent], Some(AgentStatus::Complete));
        assert!(aggregate.agent_elapsed(agent).is_some());
    }

    // Rank 39 of 41 is fast, while rank 19 of the slow Grunt's 20 is slow.
    let histogram = aggregate.histogram().unwrap();
    assert_eq!(histogram.count(), 41);
    assert!(histogram.percentile(95).unwrap() < Duration::from_millis(250));
    assert!(histogram.percentile(99).unwrap() >= Duration::from_millis(250));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_grunt_keeps_aggregate_partial_without_contributing_metrics() {
    let finished = listener();
    let finished_port = finished.local_addr().unwrap().port();
    let finished_server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&finished);
        read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx")
            .unwrap();
    });
    let pending = listener();
    let pending_port = pending.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let pending_server = thread::spawn(move || {
        // Complete one request before stalling the next one at concurrency 1.
        {
            let (mut stream, _) = common::accept(&pending);
            read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\ny")
                .unwrap();
        }
        let (mut stream, _) = common::accept(&pending);
        read_request(&mut stream);
        ready_tx.send(()).unwrap();
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            result => panic!("expected cancelled connection to close, got {result:?}"),
        }
    });

    let share = budget();
    let cancelled_share = Budget {
        requests: Some(2),
        ..share
    };
    let agent_limits = limits(Duration::from_secs(8));
    let cancel = distributed::Cancellation::new();
    let (finished, cancelled, ()) = tokio::join!(
        run_local(
            plan(finished_port, Duration::from_secs(5)),
            share,
            agent_limits,
            distributed::Cancellation::new()
        ),
        run_local(
            plan(pending_port, Duration::from_secs(5)),
            cancelled_share,
            agent_limits,
            cancel.clone()
        ),
        async {
            tokio::time::timeout(Duration::from_secs(4), ready_rx)
                .await
                .unwrap()
                .unwrap();
            cancel.cancel();
        },
    );
    finished_server.join().unwrap();
    pending_server.join().unwrap();
    let run_id = [2; 16];
    let mut aggregate = Aggregate::new(run_id, &["finished".into(), "cancelled".into()]).unwrap();
    aggregate
        .add_local_outcome(
            run_id,
            "finished",
            finished.unwrap(),
            share,
            decode_limits(agent_limits),
        )
        .unwrap();
    aggregate
        .add_local_outcome(
            run_id,
            "cancelled",
            cancelled.unwrap(),
            cancelled_share,
            decode_limits(agent_limits),
        )
        .unwrap();
    assert!(aggregate.is_partial());
    assert_eq!(aggregate.agents()["finished"], Some(AgentStatus::Complete));
    assert_eq!(
        aggregate.agents()["cancelled"],
        Some(AgentStatus::Cancelled)
    );
    assert_eq!(aggregate.agent_elapsed("cancelled"), None);
    assert_eq!(
        (
            aggregate.metrics().started,
            aggregate.completed(),
            aggregate.failed()
        ),
        (1, 1, 0)
    );
    assert_eq!(aggregate.metrics().response_bytes, 1);
    assert_eq!(aggregate.histogram().unwrap().count(), 1);
    let thresholds = aggregate
        .evaluate_thresholds(Some(100.0), Some(10_000.0))
        .unwrap();
    assert_eq!(thresholds.failure_rate, ThresholdResult::NotEvaluable);
    assert_eq!(thresholds.p95, ThresholdResult::NotEvaluable);
}

#[tokio::test]
async fn grunt_rejects_unapproved_origin_and_ip_before_connecting() {
    let target = listener();
    let mut request = plan(target.local_addr().unwrap().port(), Duration::from_secs(1));
    let allowed = policy(vec![request.url.clone()], resources());
    request.url.set_scheme("https").unwrap();
    let error = distributed::run_local(
        request,
        budget(),
        limits(Duration::from_secs(2)),
        &allowed,
        distributed::Cancellation::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "Grunt target denied by policy");

    let request = plan(target.local_addr().unwrap().port(), Duration::from_secs(1));
    let denied_ip = GruntPolicy::new(
        vec![request.url.clone()],
        vec!["192.0.2.0/24".parse().unwrap()],
        resources(),
    )
    .unwrap();
    let error = distributed::run_local(
        request,
        budget(),
        limits(Duration::from_secs(2)),
        &denied_ip,
        distributed::Cancellation::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "Grunt target denied by policy");
    assert_no_connection(&target);
}

fn assert_no_connection(listener: &TcpListener) {
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn grunt_checks_every_redirect_origin() {
    let first = listener();
    let middle = listener();
    let denied = listener();
    let mut request = plan(first.local_addr().unwrap().port(), Duration::from_secs(2));
    request.follow_redirects = true;
    let middle_url = format!("http://{}/", middle.local_addr().unwrap());
    let denied_url = format!("http://{}/", denied.local_addr().unwrap());
    let local_policy = policy(
        vec![request.url.clone(), middle_url.parse().unwrap()],
        resources(),
    );
    let server = thread::spawn(move || {
        for (listener, location) in [(first, middle_url), (middle, denied_url)] {
            let (mut stream, _) = common::accept(&listener);
            read_request(&mut stream);
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        }
    });
    let result = distributed::run_local(
        request,
        budget(),
        limits(Duration::from_secs(3)),
        &local_policy,
        distributed::Cancellation::new(),
    )
    .await
    .unwrap();
    let LocalOutcome::Complete { report, .. } = result else {
        panic!("expected completed request attempt");
    };
    assert_eq!(report.other_failures, 1);
    assert_eq!(report.redirects_followed, 1);
    assert_eq!(report.connections_opened, 2);
    assert_no_connection(&denied);
    server.join().unwrap();
}

#[tokio::test]
async fn approved_hostname_cannot_connect_to_unapproved_dns_addresses() {
    let target = listener();
    let mut request = plan(target.local_addr().unwrap().port(), Duration::from_secs(2));
    request.url.set_host(Some("localhost")).unwrap();
    let dns_policy = GruntPolicy::new(
        vec![request.url.clone()],
        vec!["192.0.2.0/24".parse().unwrap()],
        resources(),
    )
    .unwrap();
    let result = distributed::run_local(
        request,
        budget(),
        limits(Duration::from_secs(3)),
        &dns_policy,
        distributed::Cancellation::new(),
    )
    .await
    .unwrap();
    let LocalOutcome::Complete { report, .. } = result else {
        panic!("expected completed request attempt");
    };
    assert_eq!(report.connection_failures, 1);
    assert_eq!(report.connections_opened, 0);
    assert_no_connection(&target);
}

#[tokio::test]
async fn grunt_rejects_oversized_plans_before_connecting() {
    let listener = listener();
    let base = plan(
        listener.local_addr().unwrap().port(),
        Duration::from_secs(1),
    );
    let config = ResourceLimits {
        max_body_bytes: 1,
        max_header_bytes: 5,
        max_request_timeout: Duration::from_secs(1),
        max_redirects: 1,
        ..resources()
    };
    let local_policy = policy(vec![base.url.clone()], config);
    let mut body = base.clone();
    body.body = Some(bytes::Bytes::from_static(b"xx"));
    let mut headers = base.clone();
    headers.headers.append("xy", "z".parse().unwrap());
    headers.headers.append("xy", "w".parse().unwrap());
    for request in [
        body,
        headers,
        TestPlan {
            timeout: Duration::from_secs(2),
            ..base.clone()
        },
        TestPlan {
            follow_redirects: true,
            max_redirects: 2,
            ..base.clone()
        },
    ] {
        let error = distributed::run_local(
            request,
            budget(),
            limits(Duration::from_secs(3)),
            &local_policy,
            distributed::Cancellation::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "Grunt request exceeds resource policy");
    }
    for (config, expected) in [
        (
            ResourceLimits {
                max_histogram_bytes: 83,
                ..config
            },
            "Grunt histogram result exceeds policy",
        ),
        (
            ResourceLimits {
                max_active_histogram_bytes: 1,
                ..config
            },
            "Grunt histogram memory limit reached",
        ),
    ] {
        let policy = policy(vec![base.url.clone()], config);
        let error = distributed::run_local(
            base.clone(),
            budget(),
            limits(Duration::from_secs(3)),
            &policy,
            distributed::Cancellation::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
    assert_no_connection(&listener);
}

#[test]
fn grunt_ignores_environment_proxy() {
    use common::CommandTimeout;
    let proxy = listener();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "grunt_proxy_fixture", "--ignored", "--nocapture"])
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output_timeout()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_connection(&proxy);
}

#[tokio::test]
#[ignore = "subprocess fixture for grunt_ignores_environment_proxy"]
async fn grunt_proxy_fixture() {
    let target = listener();
    let port = target.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = common::accept(&target);
        let mut reader = std::io::BufReader::new(&mut stream);
        use std::io::BufRead;
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("GET / HTTP/1.1"));
        let expected_host = format!("host: localhost:{port}\r\n");
        let mut received_host = false;
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            received_host |= line.eq_ignore_ascii_case(&expected_host);
        }
        assert!(received_host);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let mut request = plan(port, Duration::from_secs(2));
    request.url.set_host(Some("localhost")).unwrap();
    let result = run_local(
        request,
        budget(),
        limits(Duration::from_secs(3)),
        distributed::Cancellation::new(),
    )
    .await
    .unwrap();
    let LocalOutcome::Complete { report, .. } = result else {
        panic!("expected completed request attempt");
    };
    assert_eq!(report.failed, 0);
    assert_eq!(report.status_codes.get(&200), Some(&1));
    server.join().unwrap();
}

#[tokio::test]
async fn grunt_shares_run_and_histogram_limits_and_releases_them_after_cancellation() {
    for (config, expected) in [
        (
            ResourceLimits {
                max_active_runs: 1,
                ..resources()
            },
            "Grunt active run limit reached",
        ),
        (
            ResourceLimits {
                max_active_histogram_bytes: 1024 * 1024,
                ..resources()
            },
            "Grunt histogram memory limit reached",
        ),
    ] {
        let target = listener();
        let request = plan(target.local_addr().unwrap().port(), Duration::from_secs(2));
        let grunt = policy(vec![request.url.clone()], config);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = common::accept(&target);
            read_request(&mut socket);
            started_tx.send(()).unwrap();
            let mut byte = [0];
            let closed = match socket.read(&mut byte) {
                Ok(0) => true,
                Err(err) => matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ),
                _ => false,
            };
            assert!(closed);
        });
        let cancel = distributed::Cancellation::new();
        let agent_limits = limits(Duration::from_secs(5));
        let (outcome, rejected) = tokio::join!(
            distributed::run_local(
                request.clone(),
                budget(),
                agent_limits,
                &grunt,
                cancel.clone()
            ),
            async {
                tokio::time::timeout(Duration::from_secs(2), started_rx)
                    .await
                    .unwrap()
                    .unwrap();
                let refused = distributed::run_local(
                    request.clone(),
                    budget(),
                    agent_limits,
                    &grunt.clone(),
                    distributed::Cancellation::new(),
                )
                .await;
                cancel.cancel();
                refused
            }
        );
        assert_eq!(rejected.unwrap_err().to_string(), expected);
        assert!(matches!(
            outcome.unwrap(),
            LocalOutcome::Partial {
                reason: StopReason::Cancelled
            }
        ));
        server.join().unwrap();

        let outcome = distributed::run_local(request, budget(), agent_limits, &grunt, cancel)
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            LocalOutcome::Partial {
                reason: StopReason::Cancelled
            }
        ));
    }
}
