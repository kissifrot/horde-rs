//! Redirect handling with a consistent, restrictive Referer policy.
use reqwest::{
    Client, Method, Request, Response, StatusCode, Url,
    header::{self, HeaderValue},
};
use std::time::{Duration, Instant};

pub(crate) fn referer(previous: &Url, next: &Url) -> Option<HeaderValue> {
    if previous.scheme() == "https" && next.scheme() == "http" {
        return None;
    }
    let mut value = previous.clone();
    let _ = value.set_username("");
    let _ = value.set_password(None);
    value.set_fragment(None);
    if previous.origin() != next.origin() {
        value.set_path("/");
        value.set_query(None);
    }
    HeaderValue::from_str(value.as_str()).ok()
}

#[derive(Debug)]
pub(crate) enum RedirectError {
    Request(reqwest::Error),
    Limit,
    Timeout,
    InvalidScheme,
    DeniedByPolicy,
}

impl RedirectError {
    pub(crate) fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout) || matches!(self, Self::Request(e) if e.is_timeout())
    }
    pub(crate) fn is_connect(&self) -> bool {
        matches!(self, Self::Request(e) if e.is_connect())
    }
}

/// Follow redirects while retaining a single timeout across all hops and the final body.
pub(crate) async fn send(
    client: &Client,
    request: Request,
    limit: usize,
    timeout: Duration,
    followed: Option<&crate::counter::AtomicCounter>,
) -> Result<Response, RedirectError> {
    send_checked(client, request, limit, timeout, followed, |_| true).await
}

pub(crate) async fn send_checked(
    client: &Client,
    mut request: Request,
    limit: usize,
    timeout: Duration,
    followed: Option<&crate::counter::AtomicCounter>,
    allowed: impl Fn(&Request) -> bool,
) -> Result<Response, RedirectError> {
    let started = Instant::now();
    let mut count = 0;
    loop {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .filter(|t| !t.is_zero())
            .ok_or(RedirectError::Timeout)?;
        *request.timeout_mut() = Some(remaining);
        let mut next_request = request
            .try_clone()
            .expect("Horde request bodies are reusable");
        let response = client
            .execute(request)
            .await
            .map_err(RedirectError::Request)?;
        let status = response.status();
        if !matches!(
            status,
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            return Ok(response);
        }
        let Some(next) = response
            .headers()
            .get(header::LOCATION)
            .and_then(|location| std::str::from_utf8(location.as_bytes()).ok())
            .and_then(|location| response.url().join(location).ok())
        else {
            return Ok(response);
        };
        if next.scheme() != "http" && next.scheme() != "https" {
            return Err(RedirectError::InvalidScheme);
        }
        if count >= limit {
            return Err(RedirectError::Limit);
        }
        let previous = response.url();
        let headers = next_request.headers_mut();
        if previous.origin() != next.origin() {
            for name in [
                header::AUTHORIZATION,
                header::COOKIE,
                header::PROXY_AUTHORIZATION,
                header::WWW_AUTHENTICATE,
                header::HOST,
            ] {
                headers.remove(name);
            }
            headers.remove("cookie2");
        }
        headers.remove(header::REFERER);
        if let Some(value) = referer(previous, &next) {
            headers.insert(header::REFERER, value);
        }
        let switch_to_get = matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND)
            && next_request.method() == Method::POST;
        if switch_to_get || status == StatusCode::SEE_OTHER {
            if next_request.method() != Method::HEAD {
                *next_request.method_mut() = Method::GET;
            }
            *next_request.body_mut() = None;
            for name in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::CONTENT_ENCODING,
                header::TRANSFER_ENCODING,
            ] {
                next_request.headers_mut().remove(name);
            }
        }
        *next_request.url_mut() = next;
        if !allowed(&next_request) {
            return Err(RedirectError::DeniedByPolicy);
        }
        count += 1;
        if let Some(followed) = followed {
            followed.increment();
        }
        request = next_request;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{
            ServerConfig,
            pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        },
    };

    #[tokio::test]
    async fn https_to_http_redirect_drops_referer_and_credentials() {
        let ca = include_bytes!("../tests/fixtures/tls/ca.der");
        let certificate =
            CertificateDer::from(include_bytes!("../tests/fixtures/tls/server.der").to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            include_bytes!("../tests/fixtures/tls/server-key.der").to_vec(),
        ));
        let config = ServerConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
        let tls = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tls_addr = tls.local_addr().unwrap();
        let target_addr = target.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = tls.accept().await.unwrap();
            let mut stream = TlsAcceptor::from(Arc::new(config))
                .accept(stream)
                .await
                .unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let (mut stream, _) = target.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert_eq!(stream.read(&mut byte).await.unwrap(), 1);
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(request).unwrap().to_ascii_lowercase()
        });
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(reqwest::Certificate::from_der(ca).unwrap())
            .build()
            .unwrap();
        let request = client
            .get(format!(
                "https://localhost:{}/private?token=secret",
                tls_addr.port()
            ))
            .header(header::AUTHORIZATION, "Bearer secret")
            .header(header::COOKIE, "session=secret")
            .header(header::REFERER, "https://localhost/private?token=secret")
            .build()
            .unwrap();
        let response = timeout(
            Duration::from_secs(5),
            send(&client, request, 1, Duration::from_secs(4), None),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let request = timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert!(request.starts_with("get /target "), "{request}");
        assert!(!request.contains("referer:"), "{request}");
        assert!(!request.contains("authorization:"), "{request}");
        assert!(!request.contains("cookie:"), "{request}");
        assert!(!request.contains("secret"), "{request}");
    }
}
