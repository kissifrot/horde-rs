//! Redirect handling with a consistent, restrictive Referer policy.
use reqwest::{
    Client, Method, Request, Response, StatusCode, Url,
    header::{self, HeaderValue},
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

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
    mut request: Request,
    limit: usize,
    timeout: Duration,
    followed: Option<&AtomicUsize>,
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
        count += 1;
        if let Some(followed) = followed {
            followed.fetch_add(1, Ordering::Relaxed);
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
        request = next_request;
    }
}
