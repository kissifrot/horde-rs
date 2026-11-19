//! Trusted local configuration for the in-process Grunt.
use super::{AgentLimits, Budget, Error, histogram};
use ipnet::IpNet;
use reqwest::{
    Method, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    header,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Resource ceilings shared by runs using the same Grunt policy.
#[derive(Clone, Copy, Debug)]
pub struct ResourceLimits {
    pub max_active_runs: usize,
    /// Budget for active histogram count arrays, including worker and merge arrays.
    pub max_active_histogram_bytes: usize,
    /// Limit on the encoded histogram envelope. Admission uses a worst-case bound.
    pub max_histogram_bytes: usize,
    pub max_request_timeout: Duration,
    pub max_redirects: usize,
    pub max_body_bytes: usize,
    /// Sum of explicit request header name and value lengths, including repetitions.
    pub max_header_bytes: usize,
}

/// Exact allowed HTTP(S) origins and allowed destination CIDRs.
/// Share clones of one policy to enforce the same active resource limits.
#[derive(Clone)]
pub struct GruntPolicy(Arc<Policy>);

struct Policy {
    origins: Vec<Url>,
    networks: Vec<IpNet>,
    resources: ResourceLimits,
    runs: Arc<Semaphore>,
    histogram_bytes: Arc<Semaphore>,
}

pub(crate) struct Reservation {
    _run: OwnedSemaphorePermit,
    _histogram_bytes: OwnedSemaphorePermit,
}

impl GruntPolicy {
    pub fn new(
        origins: Vec<Url>,
        networks: Vec<IpNet>,
        resources: ResourceLimits,
    ) -> Result<Self, Error> {
        if origins.is_empty()
            || networks.is_empty()
            || origins
                .iter()
                .any(|url| !valid_destination(url) || url.path() != "/" || url.query().is_some())
        {
            return Err(Error("invalid Grunt destination policy"));
        }
        if !(1..=Semaphore::MAX_PERMITS).contains(&resources.max_active_runs)
            || !(1..=Semaphore::MAX_PERMITS).contains(&resources.max_active_histogram_bytes)
            || resources.max_histogram_bytes == 0
            || resources.max_request_timeout.is_zero()
        {
            return Err(Error("invalid Grunt resource policy"));
        }
        Ok(Self(Arc::new(Policy {
            origins,
            networks,
            resources,
            runs: Arc::new(Semaphore::new(resources.max_active_runs)),
            histogram_bytes: Arc::new(Semaphore::new(resources.max_active_histogram_bytes)),
        })))
    }

    fn allows_ip(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.0.networks.iter().any(|network| network.contains(&ip))
    }

    pub(crate) fn allows_url(&self, url: &Url) -> bool {
        valid_destination(url)
            && self
                .0
                .origins
                .iter()
                .any(|allowed| allowed.origin() == url.origin())
            && url
                .host_str()
                .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
                .is_none_or(|ip| self.allows_ip(ip))
    }

    fn checked_addresses(
        &self,
        addresses: impl IntoIterator<Item = SocketAddr>,
    ) -> Result<Vec<SocketAddr>, Error> {
        let mut checked = Vec::new();
        for address in addresses {
            if checked.len() == 64 || !self.allows_ip(address.ip()) {
                return Err(Error("Grunt DNS result denied by policy"));
            }
            checked.push(address);
        }
        if checked.is_empty() {
            return Err(Error("Grunt DNS result is empty"));
        }
        Ok(checked)
    }

    pub(crate) fn allows_headers(&self, headers: &header::HeaderMap) -> bool {
        headers
            .iter()
            .try_fold(0usize, |total, (name, value)| {
                total
                    .checked_add(name.as_str().len())?
                    .checked_add(value.as_bytes().len())
            })
            .is_some_and(|bytes| bytes <= self.0.resources.max_header_bytes)
    }

    pub(crate) fn reserve(
        &self,
        plan: &crate::TestPlan,
        budget: Budget,
        limits: AgentLimits,
    ) -> Result<Reservation, Error> {
        if !self.allows_url(&plan.url) {
            return Err(Error("Grunt target denied by policy"));
        }
        if plan.method == Method::CONNECT
            || plan.headers.keys().any(|name| {
                matches!(
                    *name,
                    header::HOST
                        | header::CONTENT_LENGTH
                        | header::TRANSFER_ENCODING
                        | header::UPGRADE
                        | header::CONNECTION
                        | header::PROXY_AUTHORIZATION
                )
            })
        {
            return Err(Error("Grunt method or header denied by policy"));
        }
        let resources = self.0.resources;

        if plan.timeout > resources.max_request_timeout
            || (plan.follow_redirects && plan.max_redirects > resources.max_redirects)
            || plan
                .body
                .as_ref()
                .is_some_and(|body| body.len() > resources.max_body_bytes)
            || !self.allows_headers(&plan.headers)
        {
            return Err(Error("Grunt request exceeds resource policy"));
        }

        let samples = budget.requests.unwrap_or(limits.max_samples);
        if histogram::max_bytes_for_samples(samples) > resources.max_histogram_bytes {
            return Err(Error("Grunt histogram result exceeds policy"));
        }
        let workers = budget.concurrency.min(samples);
        let bytes = workers
            .checked_add(1)
            .and_then(|slots| slots.checked_mul(histogram::COUNT_ARRAY_BYTES as u64))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or(Error("Grunt histogram memory reservation too large"))?;
        let run = self
            .0
            .runs
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error("Grunt active run limit reached"))?;
        let histogram_bytes = self
            .0
            .histogram_bytes
            .clone()
            .try_acquire_many_owned(bytes)
            .map_err(|_| Error("Grunt histogram memory limit reached"))?;
        Ok(Reservation {
            _run: run,
            _histogram_bytes: histogram_bytes,
        })
    }
}

fn valid_destination(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

impl Resolve for GruntPolicy {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.clone();
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map_err(|_| Error("Grunt DNS resolution failed"))?;
            // Return the checked addresses directly to the connector. There is no
            // second lookup between validation and TCP connection establishment.
            Ok(Box::new(policy.checked_addresses(addresses)?.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dns_answer_must_pass_even_after_an_approved_lookup() {
        let policy = GruntPolicy::new(
            vec![Url::parse("https://example.test").unwrap()],
            vec!["127.0.0.1/32".parse().unwrap()],
            ResourceLimits {
                max_active_runs: 1,
                max_active_histogram_bytes: 1,
                max_histogram_bytes: 1,
                max_request_timeout: Duration::from_secs(1),
                max_redirects: 0,
                max_body_bytes: 0,
                max_header_bytes: 0,
            },
        )
        .unwrap();
        let good = "127.0.0.1:0".parse().unwrap();
        let mapped_denied = "[::ffff:127.0.0.2]:0".parse().unwrap();
        assert_eq!(policy.checked_addresses([good]).unwrap(), [good]);
        assert!(policy.checked_addresses([good, mapped_denied]).is_err());
        assert!(policy.checked_addresses([]).is_err());
    }
}
