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

/// Every address this host currently has, except tailnet-range addresses on
/// non-Tailscale interfaces (empty if enumeration fails).
///
/// `100.64.0.0/10` is shared address space that ISPs and other VPNs also use,
/// so the range alone does not mean Tailscale. Everything downstream that
/// treats a tailnet-range address as Tailscale (`listen_addrs`,
/// `watch_for_tailnet`, the late-bind check, the pairing endpoint) only ever
/// sees addresses that passed this interface filter.
pub fn local_ips() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .map(|ifs| {
            usable_ips(
                &ifs.iter()
                    .map(|i| (i.name.as_str(), i.ip()))
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap_or_default()
}

/// Tailscale's adapter is `Tailscale` on Windows and `tailscale0` on Linux.
fn is_tailscale_iface(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("tailscale")
}

/// Keep an address unless it is in the tailnet range on a non-Tailscale
/// interface. `(interface name, address)` in, addresses out.
fn usable_ips(ifs: &[(&str, IpAddr)]) -> Vec<IpAddr> {
    ifs.iter()
        .filter(|(name, ip)| !is_tailnet(ip) || is_tailscale_iface(name))
        .map(|(_, ip)| *ip)
        .collect()
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
/// only looked at startup would miss it. `bind` returns whether it bound the
/// address; an address whose bind failed is offered again on the next look.
/// Returns once at least one address is bound, so a steady state costs nothing.
/// Spawn this only when no Tailscale address was bound at startup.
pub async fn watch_for_tailnet(mut bind: impl FnMut(IpAddr) -> bool) {
    let start = tokio::time::Instant::now();
    let mut bound: Vec<IpAddr> = Vec::new();
    loop {
        tokio::time::sleep(retry_delay(start.elapsed())).await;
        for ip in tailnet_addrs(&local_ips()) {
            if !bound.contains(&ip) && bind(ip) {
                bound.push(ip);
            }
        }
        if !bound.is_empty() {
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
    fn tailnet_range_counts_only_on_a_tailscale_interface() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let ifs = [
            ("Ethernet", ip("192.168.1.5")),
            ("Ethernet", ip("100.72.0.9")),
            ("OtherVPN", ip("fd7a:115c:a1e0::7")),
            ("Tailscale", ip("100.100.1.2")),
            ("tailscale0", ip("fd7a:115c:a1e0::1")),
        ];
        let usable = usable_ips(&ifs);
        assert_eq!(
            usable,
            ips(&["192.168.1.5", "100.100.1.2", "fd7a:115c:a1e0::1"])
        );
        assert_eq!(
            tailnet_addrs(&usable),
            ips(&["100.100.1.2", "fd7a:115c:a1e0::1"])
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
