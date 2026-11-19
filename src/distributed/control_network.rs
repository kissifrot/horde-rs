//! Address policy for the control connection, independent of HTTP target policy.
use super::Error;
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
};

pub(super) fn is_private_control_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => ip.is_unique_local(),
    }
}

/// The local interface and source addresses allowed to reach mutual TLS.
#[derive(Default)]
pub struct ListenerPolicy {
    private: Option<PrivateListener>,
}

struct PrivateListener {
    address: SocketAddr,
    peers: BTreeSet<IpAddr>,
}

impl ListenerPolicy {
    /// Keep a loopback listener when no peers are supplied, or opt into a
    /// specific private interface with at least one exact private peer IP.
    pub fn for_listener(address: SocketAddr, peers: Vec<IpAddr>) -> Result<Self, Error> {
        if address.ip().to_canonical().is_loopback() && peers.is_empty() {
            return Ok(Self::default());
        }
        if !is_private_control_ip(address.ip())
            || peers.is_empty()
            || peers.iter().any(|&ip| !is_private_control_ip(ip))
        {
            return Err(Error(
                "Grunt requires loopback or an explicit private listener and private peer IPs",
            ));
        }
        Ok(Self {
            private: Some(PrivateListener {
                address,
                peers: peers.into_iter().map(|ip| ip.to_canonical()).collect(),
            }),
        })
    }

    pub(super) fn admits(&self, local: SocketAddr, peer: SocketAddr) -> bool {
        let local_ip = local.ip().to_canonical();
        let peer_ip = peer.ip().to_canonical();
        match &self.private {
            None => local_ip.is_loopback() && peer_ip.is_loopback(),
            Some(policy) => {
                local_ip == policy.address.ip().to_canonical()
                    && (policy.address.port() == 0 || local.port() == policy.address.port())
                    && policy.peers.contains(&peer_ip)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact_local_and_peer_addresses_in_private_mode() {
        let local = "10.1.0.5:7071".parse().unwrap();
        let policy =
            ListenerPolicy::for_listener(local, vec!["10.1.0.4".parse().unwrap()]).unwrap();
        let peer = "10.1.0.4:39143".parse().unwrap();
        assert!(policy.admits(local, peer));
        assert!(policy.admits(local, "[::ffff:10.1.0.4]:39143".parse().unwrap()));
        assert!(!policy.admits(local, "10.1.0.6:39143".parse().unwrap()));
        assert!(!policy.admits("10.1.0.6:7071".parse().unwrap(), peer));
        assert!(!policy.admits("10.1.0.5:7072".parse().unwrap(), peer));
        assert!(!ListenerPolicy::default().admits(local, peer));
    }

    #[test]
    fn only_specific_private_unicast_classes_can_opt_in() {
        for allowed in ["10.0.0.5", "172.16.0.5", "192.168.0.5", "fd00::5"] {
            let ip = allowed.parse().unwrap();
            let local = SocketAddr::new(ip, 0);
            assert!(ListenerPolicy::for_listener(local, vec![ip]).is_ok());
            assert!(ListenerPolicy::for_listener(local, vec![]).is_err());
        }
        let local = "10.0.0.5:7071".parse().unwrap();
        for forbidden in [
            "0.0.0.0",
            "::",
            "192.0.2.5",
            "100.64.0.5",
            "169.254.0.5",
            "fe80::5",
            "ff02::1",
            "127.0.0.1",
        ] {
            let ip = forbidden.parse().unwrap();
            assert!(!is_private_control_ip(ip));
            assert!(ListenerPolicy::for_listener(local, vec![ip]).is_err());
            assert!(
                ListenerPolicy::for_listener(SocketAddr::new(ip, 7071), vec![local.ip()]).is_err()
            );
        }
    }
}
