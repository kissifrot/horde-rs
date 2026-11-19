use super::*;
use crate::distributed::ResourceLimits;
use reqwest::{Method, Url, header::HeaderMap};
use std::net::SocketAddr;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{ClientConfig, pki_types::PrivatePkcs8KeyDer},
};

const WAIT: Duration = Duration::from_secs(5);
const ID: Id = [1; 16];

macro_rules! certificate {
    ($name:literal) => {
        CertificateDer::from(
            include_bytes!(concat!("../../../tests/fixtures/grunt-tls/", $name, ".der")).as_slice(),
        )
    };
}
macro_rules! private_key {
    ($name:literal) => {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            include_bytes!(concat!(
                "../../../tests/fixtures/grunt-tls/",
                $name,
                "-key.der"
            ))
            .as_slice(),
        ))
    };
}

fn roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.add(certificate!("ca")).unwrap();
    roots
}

fn endpoint(target: SocketAddr, grace: Duration) -> Arc<Endpoint> {
    let url = Url::parse(&format!("http://{target}/")).unwrap();
    let policy = GruntPolicy::new(
        vec![url.clone()],
        vec!["127.0.0.1/32".parse().unwrap()],
        ResourceLimits {
            max_active_runs: 1,
            max_active_histogram_bytes: 1024 * 1024,
            max_histogram_bytes: 1024,
            max_request_timeout: WAIT,
            max_redirects: 0,
            max_body_bytes: 0,
            max_header_bytes: 0,
        },
    )
    .unwrap();
    Endpoint::new(
        vec![certificate!("server")],
        private_key!("server"),
        roots(),
        vec![certificate!("controller"), certificate!("other")],
        ["test", "different"]
            .into_iter()
            .map(|name| {
                (
                    name.to_owned(),
                    Profile {
                        plan: TestPlan {
                            url: url.clone(),
                            requests: 1,
                            concurrency: 1,
                            timeout: WAIT,
                            method: Method::GET,
                            headers: HeaderMap::new(),
                            body: None,
                            duration: None,
                            rate: None,
                            verbose: false,
                            bounded_latency: true,
                            reuse_connections: false,
                            follow_redirects: false,
                            max_redirects: 0,
                        },
                        budget: Budget {
                            requests: Some(1),
                            concurrency: 1,
                            rate_per_second: 1,
                            duration: None,
                        },
                        limits: AgentLimits {
                            max_samples: 1,
                            max_concurrency: 1,
                            max_rate_per_second: 1,
                            max_duration: WAIT + WAIT,
                        },
                        policy: policy.clone(),
                    },
                )
            })
            .collect(),
        EndpointLimits {
            max_sessions: 2,
            max_records: 1,
            max_retained_result_bytes: MAX_RETAINED_RESULT_BYTES,
            handshake_timeout: WAIT,
            frame_timeout: WAIT,
            reconnect_grace: grace,
        },
    )
    .unwrap()
}

fn client(identity: Option<(CertificateDer<'static>, PrivateKeyDer<'static>)>) -> TlsConnector {
    let builder = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots());
    let mut config = if let Some((certificate, key)) = identity {
        builder
            .with_client_auth_cert(vec![certificate], key)
            .unwrap()
    } else {
        builder.with_no_client_auth()
    };
    config.alpn_protocols = vec![ALPN.to_vec()];
    TlsConnector::from(Arc::new(config))
}

fn controller() -> TlsConnector {
    client(Some((
        certificate!("controller"),
        private_key!("controller"),
    )))
}

async fn open(
    endpoint: &Arc<Endpoint>,
    client: TlsConnector,
    server_name: &'static str,
) -> (
    io::Result<TlsStream<TcpStream>>,
    JoinHandle<Result<(), BoxError>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let endpoint = endpoint.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = timeout(WAIT, listener.accept()).await??;
        endpoint.serve_connection(stream).await
    });
    let stream = TcpStream::connect(address).await.unwrap();
    let tls = timeout(
        WAIT,
        client.connect(server_name.try_into().unwrap(), stream),
    )
    .await
    .unwrap();
    (tls, server)
}

async fn ready(
    endpoint: &Arc<Endpoint>,
    client: TlsConnector,
) -> (TlsStream<TcpStream>, Id, JoinHandle<Result<(), BoxError>>) {
    let (stream, server) = open(endpoint, client, "localhost").await;
    let mut stream = stream.unwrap();
    let Reply::Hello {
        version: VERSION,
        epoch,
    } = receive(&mut stream).await
    else {
        panic!("expected Grunt hello");
    };
    (stream, epoch, server)
}

async fn receive(stream: &mut TlsStream<TcpStream>) -> Reply {
    timeout(WAIT, read_frame(stream))
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

async fn request(
    stream: &mut TlsStream<TcpStream>,
    epoch: Id,
    run_id: Id,
    operation: Operation,
) -> Reply {
    timeout(
        WAIT,
        write_frame(
            stream,
            &Request {
                version: VERSION,
                epoch,
                run_id,
                operation,
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    receive(stream).await
}

async fn close(mut stream: TlsStream<TcpStream>, task: JoinHandle<Result<(), BoxError>>) {
    stream.shutdown().await.unwrap();
    timeout(WAIT, task).await.unwrap().unwrap().unwrap();
}

fn submit() -> Operation {
    Operation::Submit {
        profile: "test".into(),
    }
}

async fn wait_for_status(stream: &mut TlsStream<TcpStream>, epoch: Id, expected: RunStatus) {
    wait_for_run_status(stream, epoch, ID, expected).await;
}

async fn wait_for_run_status(
    stream: &mut TlsStream<TcpStream>,
    epoch: Id,
    run: Id,
    expected: RunStatus,
) {
    timeout(WAIT, async {
        loop {
            let reply = request(stream, epoch, run, Operation::Inspect).await;
            if reply == (Reply::State { status: expected }) {
                break;
            }
            assert!(
                matches!(
                    reply,
                    Reply::State {
                        status: RunStatus::Running | RunStatus::Cancelling
                    }
                ),
                "{reply:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

// Wait for a real workload connection, then let the test decide whether to complete or cancel it.
async fn target_request(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    timeout(WAIT, async {
        let mut tail = [0; 4];
        while &tail != b"\r\n\r\n" {
            tail.rotate_left(1);
            tail[3] = stream.read_u8().await.unwrap();
        }
    })
    .await
    .unwrap();
    stream
}

#[tokio::test]
async fn reconnect_and_duplicate_submission_reuse_the_same_run() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = endpoint(target.local_addr().unwrap(), Duration::from_secs(2));
    let (mut first, epoch, first_task) = ready(&endpoint, controller()).await;
    let (mut concurrent, _, concurrent_task) = ready(&endpoint, controller()).await;
    let (first_submission, duplicate) = tokio::join!(
        request(&mut first, epoch, ID, submit()),
        request(&mut concurrent, epoch, ID, submit()),
    );
    assert_eq!(
        first_submission,
        Reply::State {
            status: RunStatus::Running
        }
    );
    assert_eq!(duplicate, first_submission);
    close(concurrent, concurrent_task).await;
    let mut workload = target_request(&target).await;
    close(first, first_task).await;
    let (mut second, new_epoch, second_task) = ready(&endpoint, controller()).await;
    assert_eq!(epoch, new_epoch);
    assert_eq!(
        request(&mut second, epoch, ID, Operation::Inspect).await,
        Reply::State {
            status: RunStatus::Running
        }
    );
    assert_eq!(
        request(&mut second, epoch, ID, submit()).await,
        Reply::State {
            status: RunStatus::Running
        }
    );
    assert_eq!(
        request(
            &mut second,
            epoch,
            ID,
            Operation::Submit {
                profile: "different".into()
            }
        )
        .await,
        Reply::Error {
            code: ErrorCode::ConflictingSubmission
        }
    );

    let other = client(Some((certificate!("other"), private_key!("other"))));
    let (mut third, _, third_task) = ready(&endpoint, other).await;
    for operation in [Operation::Inspect, Operation::Cancel] {
        assert_eq!(
            request(&mut third, epoch, ID, operation).await,
            Reply::Error {
                code: ErrorCode::UnknownRun
            }
        );
    }
    close(third, third_task).await;
    assert_eq!(
        request(&mut second, epoch, [2; 16], submit()).await,
        Reply::Error {
            code: ErrorCode::Capacity
        }
    );

    workload
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    wait_for_status(&mut second, epoch, RunStatus::Complete).await;
    assert_eq!(
        request(&mut second, epoch, ID, submit()).await,
        Reply::State {
            status: RunStatus::Complete
        }
    );
    assert!(
        timeout(Duration::from_millis(50), target.accept())
            .await
            .is_err()
    );
    close(second, second_task).await;
}

#[tokio::test]
async fn disconnect_past_grace_and_explicit_cancellation_are_terminal() {
    for disconnect in [false, true] {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = endpoint(target.local_addr().unwrap(), Duration::from_millis(100));
        let (mut stream, epoch, task) = ready(&endpoint, controller()).await;
        assert_eq!(
            request(&mut stream, epoch, ID, submit()).await,
            Reply::State {
                status: RunStatus::Running
            }
        );
        let mut workload = target_request(&target).await;
        let joined = if disconnect {
            close(stream, task).await;
            None
        } else {
            assert_eq!(
                request(&mut stream, epoch, ID, Operation::Cancel).await,
                Reply::State {
                    status: RunStatus::Cancelling
                }
            );
            Some((stream, task))
        };
        let mut byte = [0];
        match timeout(WAIT, workload.read(&mut byte)).await.unwrap() {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            result => panic!("expected workload connection to close, got {result:?}"),
        }
        let (mut stream, task) = match joined {
            Some(connection) => connection,
            None => {
                let (stream, _, task) = ready(&endpoint, controller()).await;
                (stream, task)
            }
        };
        wait_for_status(&mut stream, epoch, RunStatus::Cancelled).await;
        for operation in [submit(), Operation::Cancel] {
            assert_eq!(
                request(&mut stream, epoch, ID, operation).await,
                Reply::State {
                    status: RunStatus::Cancelled
                }
            );
        }
        close(stream, task).await;
        assert!(
            timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn mutual_tls_rejects_missing_unlisted_and_wrong_peer_identities() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = endpoint(target.local_addr().unwrap(), Duration::from_secs(1));
    let unauthorized = client(Some((
        certificate!("unauthorized"),
        private_key!("unauthorized"),
    )));
    for (client, name) in [
        (client(None), "localhost"),
        (unauthorized, "localhost"),
        (controller(), "wrong.example"),
    ] {
        let (stream, task) = open(&endpoint, client, name).await;
        if let Ok(mut stream) = stream {
            assert!(!matches!(
                timeout(WAIT, read_frame::<Reply>(&mut stream))
                    .await
                    .unwrap(),
                Ok(Some(_))
            ));
        }
        assert!(timeout(WAIT, task).await.unwrap().unwrap().is_err());
    }
}

#[tokio::test]
async fn stale_epoch_and_oversized_frames_do_not_start_work() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = target.local_addr().unwrap();
    let original = endpoint(address, Duration::from_secs(1));
    let (old_stream, stale, old_task) = ready(&original, controller()).await;
    close(old_stream, old_task).await;
    drop(original);

    let restarted = endpoint(address, Duration::from_secs(1));
    let (mut stream, epoch, task) = ready(&restarted, controller()).await;
    assert_ne!(stale, epoch);
    assert_eq!(
        request(&mut stream, stale, ID, submit()).await,
        Reply::Error {
            code: ErrorCode::StaleEpoch
        }
    );
    assert_eq!(
        request(&mut stream, epoch, ID, Operation::Inspect).await,
        Reply::Error {
            code: ErrorCode::UnknownRun
        }
    );
    assert_eq!(
        request(
            &mut stream,
            epoch,
            ID,
            Operation::SubmitBudget {
                profile: "test".into(),
                budget: Budget {
                    requests: Some(2),
                    concurrency: 1,
                    rate_per_second: 1,
                    duration: None
                },
            }
        )
        .await,
        Reply::Error {
            code: ErrorCode::Rejected
        },
    );
    assert_eq!(
        request(&mut stream, epoch, ID, Operation::Inspect).await,
        Reply::Error {
            code: ErrorCode::UnknownRun
        },
    );
    stream.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
    stream.flush().await.unwrap();
    let error = timeout(WAIT, task).await.unwrap().unwrap().unwrap_err();
    assert_eq!(error.to_string(), "invalid Grunt frame size");
    assert!(
        timeout(Duration::from_millis(50), target.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn saturated_session_limit_rejects_and_releases_connections() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = endpoint(target.local_addr().unwrap(), Duration::from_secs(1));
    let (first, _, first_task) = ready(&endpoint, controller()).await;
    let (second, _, second_task) = ready(&endpoint, controller()).await;

    let (third, third_task) = open(&endpoint, controller(), "localhost").await;
    assert!(
        third.is_err(),
        "a saturated endpoint must reject before TLS"
    );
    let error = timeout(WAIT, third_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), "Grunt session capacity reached");

    close(first, first_task).await;
    let (replacement, _, replacement_task) = ready(&endpoint, controller()).await;
    close(replacement, replacement_task).await;
    close(second, second_task).await;
}

#[tokio::test]
async fn handshake_and_incomplete_frame_timeouts_free_the_session() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut endpoint = endpoint(target.local_addr().unwrap(), Duration::from_secs(1));
    let config = &mut Arc::get_mut(&mut endpoint).unwrap().config;
    config.handshake_timeout = Duration::from_millis(500);
    config.frame_timeout = Duration::from_millis(500);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stalled = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (stream, _) = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    let error = timeout(WAIT, endpoint.serve_connection(stream))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.is::<tokio::time::error::Elapsed>());
    assert_eq!(endpoint.sessions.available_permits(), 2);
    drop(stalled);

    let (mut stalled, _, stalled_task) = ready(&endpoint, controller()).await;
    stalled.write_u32(10).await.unwrap();
    stalled.write_all(b"{").await.unwrap();
    stalled.flush().await.unwrap();
    let error = timeout(WAIT, stalled_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.is::<tokio::time::error::Elapsed>());
    assert_eq!(endpoint.sessions.available_permits(), 2);
    drop(stalled);

    let (replacement, _, replacement_task) = ready(&endpoint, controller()).await;
    close(replacement, replacement_task).await;
    assert!(
        timeout(Duration::from_millis(50), target.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn retained_result_limit_preserves_the_first_result_and_rejects_the_next() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut endpoint = endpoint(target.local_addr().unwrap(), Duration::from_secs(1));
    let config = &mut Arc::get_mut(&mut endpoint).unwrap().config;
    config.max_records = 3;
    config.max_retained_result_bytes = MAX_RESULT_METADATA + 1024;

    let (mut stream, epoch, task) = ready(&endpoint, controller()).await;
    for (run, expected_status) in [(ID, RunStatus::Complete), ([2; 16], RunStatus::Failed)] {
        assert_eq!(
            request(&mut stream, epoch, run, submit()).await,
            Reply::State {
                status: RunStatus::Running
            }
        );
        let mut workload = target_request(&target).await;
        workload
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        wait_for_run_status(&mut stream, epoch, run, expected_status).await;
    }
    assert_eq!(
        request(&mut stream, epoch, [2; 16], Operation::Collect).await,
        Reply::State {
            status: RunStatus::Failed
        }
    );
    let Reply::Result { histogram_bytes } =
        request(&mut stream, epoch, ID, Operation::Collect).await
    else {
        panic!("the first result must remain retrievable");
    };
    assert!(histogram_bytes <= 1024);
    timeout(WAIT, async {
        let metadata: CompletedResult = read_frame_limited(&mut stream, MAX_RESULT_METADATA)
            .await
            .unwrap()
            .unwrap();
        let mut histogram = vec![0; histogram_bytes];
        stream.read_exact(&mut histogram).await.unwrap();
        assert_eq!(metadata.metrics.completed, 1);
        assert_eq!(
            super::super::decode_histogram(
                &histogram,
                1,
                super::super::DecodeLimits {
                    max_envelope_bytes: 1024,
                    max_samples: 1,
                    assigned_requests: Some(1),
                }
            )
            .unwrap()
            .count(),
            1
        );
    })
    .await
    .unwrap();
    close(stream, task).await;
}
