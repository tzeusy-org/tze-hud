//! Which local addresses the gRPC and MCP servers listen on.
//!
//! The only exposure beyond the machine is the owner's tailnet: loopback plus
//! every local address in Tailscale's ranges (`100.64.0.0/10`,
//! `fd7a:115c:a1e0::/48`), and nothing else. There is no bind-all switch.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// Local addresses that belong to Tailscale (CGNAT `100.64.0.0/10`, ULA
/// `fd7a:115c:a1e0::/48`), in input order.
pub fn tailnet_addrs(all: &[IpAddr]) -> Vec<IpAddr> {
    all.iter().copied().filter(is_tailnet).collect()
}

fn is_tailnet(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 0x40
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Every address this host currently has (empty if enumeration fails).
pub fn local_ips() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .map(|ifs| ifs.iter().map(|i| i.ip()).collect())
        .unwrap_or_default()
}

/// The addresses to listen on given the host's addresses: IPv4 loopback first,
/// then each Tailscale address.
pub fn listen_addrs(all: &[IpAddr], port: u16) -> Vec<SocketAddr> {
    std::iter::once(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .chain(tailnet_addrs(all))
        .map(|ip| SocketAddr::new(ip, port))
        .collect()
}

/// Delay before the next look for a Tailscale address: every 10 s for the first
/// 5 minutes after startup, then every 60 s.
fn retry_delay(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_secs(300) {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(60)
    }
}

/// Wait for a Tailscale address to appear and hand each new one to `bind`.
///
/// Tailscale often comes up after the HUD (logon ordering), so a listener that
/// only looked at startup would miss it. Returns once `bind` has been given at
/// least one address, so a steady state costs nothing. Spawn this only when no
/// Tailscale address was present at startup.
pub async fn watch_for_tailnet(mut bind: impl FnMut(IpAddr)) {
    let start = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(retry_delay(start.elapsed())).await;
        let found = tailnet_addrs(&local_ips());
        if !found.is_empty() {
            for ip in found {
                bind(ip);
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn tailnet_addrs_keeps_only_the_tailscale_ranges() {
        let all = ips(&[
            "127.0.0.1",
            "192.168.1.5",
            "100.63.255.255",
            "100.64.0.0",
            "100.101.102.103",
            "100.127.255.255",
            "100.128.0.0",
            "fe80::1",
            "fd7a:115c:a1e0::1",
            "fd7a:115c:a1e0:ab12::7",
            "fd7a:115c:a1e1::1",
        ]);
        assert_eq!(
            tailnet_addrs(&all),
            ips(&[
                "100.64.0.0",
                "100.101.102.103",
                "100.127.255.255",
                "fd7a:115c:a1e0::1",
                "fd7a:115c:a1e0:ab12::7",
            ])
        );
    }

    #[test]
    fn listen_addrs_is_loopback_plus_tailnet_never_wildcard() {
        let all = ips(&["0.0.0.0", "192.168.1.5", "100.100.1.2", "::1"]);
        let got = listen_addrs(&all, 9090);
        assert_eq!(
            got,
            vec![
                "127.0.0.1:9090".parse::<SocketAddr>().unwrap(),
                "100.100.1.2:9090".parse().unwrap()
            ]
        );
    }

    #[test]
    fn retry_backs_off_after_five_minutes() {
        assert_eq!(retry_delay(Duration::ZERO), Duration::from_secs(10));
        assert_eq!(
            retry_delay(Duration::from_secs(299)),
            Duration::from_secs(10)
        );
        assert_eq!(
            retry_delay(Duration::from_secs(300)),
            Duration::from_secs(60)
        );
    }
}
