//! Which local addresses the gRPC and MCP servers listen on.
//!
//! The only exposure beyond the machine is the owner's tailnet: loopback plus
//! every local address in Tailscale's ranges (`100.64.0.0/10`,
//! `fd7a:115c:a1e0::/48`), and nothing else. There is no bind-all switch.

use std::collections::HashSet;
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

/// Retry only outstanding bind failures: 10 s for the first five minutes,
/// then 60 s. Address discovery itself is driven by OS notifications.
fn retry_delay(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_secs(300) {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(60)
    }
}

trait AddressChanges {
    async fn changed(&mut self) -> std::io::Result<()>;
}

/// Keep every local Tailscale address served, including later families and
/// failed startup/handoff binds. `bound` contains successful listeners only.
/// Register before taking the first snapshot so startup changes cannot be lost.
/// No timer is armed when all current addresses are served (or none exist).
pub(crate) async fn watch_for_tailnet(
    bound: Vec<IpAddr>,
    bind: impl FnMut(IpAddr) -> bool,
    #[cfg(test)] fixture: Option<TailnetFixture>,
) {
    #[cfg(test)]
    if let Some(mut fixture) = fixture {
        let snapshot = fixture.addresses.clone();
        let bound = bound.into_iter().chain(fixture.already_bound).collect();
        maintain_tailnet_listeners(
            bound,
            &mut fixture.addresses,
            || snapshot.borrow().clone(),
            bind,
        )
        .await;
        return;
    }
    match address_changes::Notifications::new() {
        Ok(mut changes) => maintain_tailnet_listeners(bound, &mut changes, local_ips, bind).await,
        Err(e) => tracing::error!(error = %e, "cannot watch local address changes"),
    }
}

/// Bind a non-inheritable, nonblocking socket before transport-specific adoption.
pub(crate) fn bind_listener(
    addr: SocketAddr,
    #[cfg(test)] injected: Option<&InjectedBind>,
) -> std::io::Result<std::net::TcpListener> {
    #[cfg(test)]
    if let Some(bind) = injected {
        let listener = bind(addr)?;
        listener.set_nonblocking(true)?;
        return Ok(listener);
    }
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

#[cfg(test)]
type InjectedBind =
    std::sync::Arc<dyn Fn(SocketAddr) -> std::io::Result<std::net::TcpListener> + Send + Sync>;

/// Inject events/outcomes at the actual MCP/gRPC listener-management callers.
/// Synthetic tailnet addresses map to owned loopback sockets, never live VPN IPs.
#[cfg(test)]
pub(crate) struct TailnetFixture {
    /// Startup owns only loopback; synthetic served v4 seeds the watcher below.
    pub(crate) initial_ips: Vec<IpAddr>,
    already_bound: Vec<IpAddr>,
    addresses: tokio::sync::watch::Receiver<Vec<IpAddr>>,
    pub(crate) bind: InjectedBind,
}

#[cfg(test)]
impl AddressChanges for tokio::sync::watch::Receiver<Vec<IpAddr>> {
    async fn changed(&mut self) -> std::io::Result<()> {
        tokio::sync::watch::Receiver::changed(self)
            .await
            .map_err(std::io::Error::other)
    }
}

#[cfg(test)]
pub(crate) struct TailnetControl {
    pub(crate) addresses: tokio::sync::watch::Sender<Vec<IpAddr>>,
    pub(crate) fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
    attempts: tokio::sync::mpsc::UnboundedReceiver<(IpAddr, Option<SocketAddr>)>,
    pub(crate) v4: IpAddr,
    pub(crate) v6: IpAddr,
}

#[cfg(test)]
impl TailnetControl {
    pub(crate) async fn next_attempt(&mut self) -> (IpAddr, Option<SocketAddr>) {
        tokio::time::timeout(Duration::from_secs(5), self.attempts.recv())
            .await
            .expect("listener manager did not consume the address event")
            .expect("listener manager exited")
    }

    pub(crate) async fn next_bound(&mut self, expected: IpAddr) -> SocketAddr {
        tokio::time::timeout(Duration::from_secs(5), async {
            // OS events may coalesce or arrive while the first snapshot is read.
            // Drain earlier failures without mistaking one for this recovery.
            while let Some((requested, bound)) = self.attempts.recv().await {
                assert_eq!(requested, expected);
                if let Some(bound) = bound {
                    return bound;
                }
            }
            panic!("listener manager exited before binding");
        })
        .await
        .expect("listener manager did not recover the late bind")
    }
}

#[cfg(test)]
pub(crate) fn tailnet_fixture() -> (TailnetFixture, TailnetControl) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let v4 = "100.100.1.2".parse().unwrap();
    let v6 = "fd7a:115c:a1e0::1".parse().unwrap();
    let (addresses, receiver) = tokio::sync::watch::channel(vec![v4]);
    let fail = Arc::new(AtomicBool::new(true));
    let failing = Arc::clone(&fail);
    let (tx, attempts) = tokio::sync::mpsc::unbounded_channel();
    let fixture = TailnetFixture {
        initial_ips: Vec::new(),
        already_bound: vec![v4],
        addresses: receiver,
        bind: Arc::new(move |addr| {
            let result = if failing.load(Ordering::Acquire) {
                Err(std::io::Error::from(std::io::ErrorKind::AddrNotAvailable))
            } else {
                std::net::TcpListener::bind(if addr.is_ipv6() {
                    "[::1]:0"
                } else {
                    "127.0.0.1:0"
                })
            };
            tx.send((
                addr.ip(),
                result.as_ref().ok().map(|l| l.local_addr().unwrap()),
            ))
            .unwrap();
            result
        }),
    };
    (
        fixture,
        TailnetControl {
            addresses,
            fail,
            attempts,
            v4,
            v6,
        },
    )
}

async fn maintain_tailnet_listeners(
    bound: Vec<IpAddr>,
    changes: &mut impl AddressChanges,
    mut addresses: impl FnMut() -> Vec<IpAddr>,
    mut bind: impl FnMut(IpAddr) -> bool,
) {
    let start = tokio::time::Instant::now();
    let mut bound: HashSet<IpAddr> = bound.into_iter().collect();
    loop {
        let mut failed = false;
        // Deduplicate snapshots too: a failed bind gets one attempt per wake.
        let mut seen = HashSet::new();
        for ip in tailnet_addrs(&addresses()) {
            if !bound.contains(&ip) && seen.insert(ip) {
                if bind(ip) {
                    bound.insert(ip);
                } else {
                    failed = true;
                }
            }
        }
        let changed = if failed {
            tokio::select! {
                changed = changes.changed() => changed,
                _ = tokio::time::sleep(retry_delay(start.elapsed())) => Ok(()),
            }
        } else {
            changes.changed().await
        };
        if let Err(e) = changed {
            tracing::error!(error = %e, "local address notifications stopped");
            return;
        }
    }
}

#[cfg(target_os = "windows")]
mod address_changes {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{BOOLEAN, HANDLE};
    use windows::Win32::NetworkManagement::IpHelper::{
        CancelMibChangeNotify2, MIB_NOTIFICATION_TYPE, MIB_UNICASTIPADDRESS_ROW,
        NotifyUnicastIpAddressChange,
    };
    use windows::Win32::Networking::WinSock::AF_UNSPEC;

    pub(super) struct Notifications {
        handle: HANDLE,
        // Stable allocation: Windows retains this pointer until cancellation.
        sender: Option<Box<tokio::sync::mpsc::Sender<()>>>,
        receiver: tokio::sync::mpsc::Receiver<()>,
    }

    // The opaque registration handle is only used to cancel notifications.
    // Windows may call the callback on any thread; Sender is thread safe.
    unsafe impl Send for Notifications {}

    unsafe extern "system" fn changed(
        context: *const c_void,
        _row: *const MIB_UNICASTIPADDRESS_ROW,
        _kind: MIB_NOTIFICATION_TYPE,
    ) {
        // SAFETY: Notifications owns the stable Box until CancelMibChangeNotify2
        // has drained callbacks. Never wait on the async task from this callback.
        let sender = unsafe { &*context.cast::<tokio::sync::mpsc::Sender<()>>() };
        let _ = sender.try_send(());
    }

    impl Notifications {
        pub(super) fn new() -> std::io::Result<Self> {
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            let sender = Box::new(sender);
            let mut handle = HANDLE::default();
            // SAFETY: the context remains live through registration/cancellation;
            // AF_UNSPEC subscribes to both families, without a startup callback.
            unsafe {
                NotifyUnicastIpAddressChange(
                    AF_UNSPEC,
                    Some(changed),
                    Some((&*sender as *const tokio::sync::mpsc::Sender<()>).cast()),
                    BOOLEAN(0),
                    &mut handle,
                )
            }
            .ok()
            .map_err(std::io::Error::other)?;
            Ok(Self {
                handle,
                sender: Some(sender),
                receiver,
            })
        }
    }

    impl super::AddressChanges for Notifications {
        async fn changed(&mut self) -> std::io::Result<()> {
            self.receiver
                .recv()
                .await
                .ok_or_else(|| std::io::Error::other("address notification channel closed"))
        }
    }

    impl Drop for Notifications {
        fn drop(&mut self) {
            // SAFETY: never called from the callback; cancellation waits for any
            // running callbacks before the sender allocation is freed.
            let result = unsafe { CancelMibChangeNotify2(self.handle) };
            if let Err(e) = result.ok() {
                tracing::error!(error = %e, "cannot cancel local address notifications");
                // Preserve the callback context if Windows could still use it.
                std::mem::forget(self.sender.take());
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod address_changes {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    pub(super) struct Notifications(tokio::io::unix::AsyncFd<OwnedFd>);

    impl Notifications {
        pub(super) fn new() -> std::io::Result<Self> {
            // SAFETY: socket returns an owned fd; all fields passed to bind are
            // initialized and the sockaddr length matches the concrete struct.
            let fd = unsafe {
                libc::socket(
                    libc::AF_NETLINK,
                    libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    libc::NETLINK_ROUTE,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            address.nl_family = libc::AF_NETLINK as _;
            address.nl_groups =
                (libc::RTMGRP_IPV4_IFADDR | libc::RTMGRP_IPV6_IFADDR | libc::RTMGRP_LINK) as _;
            let result = unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&address as *const libc::sockaddr_nl).cast(),
                    std::mem::size_of_val(&address) as _,
                )
            };
            if result < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: OwnedFd keeps this descriptor open and unchanged until
            // AsyncFd drops it. Only incoming notifications need readiness.
            unsafe {
                tokio::io::unix::AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE)
            }
            .map(Self)
            .map_err(Into::into)
        }
    }

    impl super::AddressChanges for Notifications {
        async fn changed(&mut self) -> std::io::Result<()> {
            loop {
                let mut ready = self.0.readable().await?;
                match ready.try_io(|fd| {
                    let mut buffer = [0u8; 8192];
                    // SAFETY: buffer is valid and writable for its full length.
                    let read = unsafe {
                        libc::recv(
                            fd.get_ref().as_raw_fd(),
                            buffer.as_mut_ptr().cast(),
                            buffer.len(),
                            0,
                        )
                    };
                    if read < 0 {
                        let error = std::io::Error::last_os_error();
                        // A dropped netlink datagram still means resnapshot.
                        if error.raw_os_error() == Some(libc::ENOBUFS) {
                            return Ok(());
                        }
                        Err(error)
                    } else {
                        Ok(())
                    }
                }) {
                    Ok(result) => return result,
                    Err(_) => continue,
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod address_changes {
    pub(super) struct Notifications;
    impl Notifications {
        pub(super) fn new() -> std::io::Result<Self> {
            Err(std::io::Error::other(
                "address notifications unsupported on this host",
            ))
        }
    }
    impl super::AddressChanges for Notifications {
        async fn changed(&mut self) -> std::io::Result<()> {
            std::future::pending().await
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

    /// One shared lifecycle gate for the manager used by MCP and gRPC.
    /// Binding is injected: successful addresses stand in for actual listener
    /// registration, so failures must never enter discovery/status state.
    #[tokio::test(start_paused = true)]
    async fn listener_lifecycle_follows_changes_and_retries_only_missing_binds() {
        use std::cell::RefCell;
        use std::future::Future;
        use std::pin::Pin;
        use std::rc::Rc;
        use std::task::Poll;

        struct Changes(tokio::sync::mpsc::Receiver<()>);
        impl AddressChanges for Changes {
            async fn changed(&mut self) -> std::io::Result<()> {
                self.0
                    .recv()
                    .await
                    .ok_or_else(|| std::io::Error::other("closed"))
            }
        }
        // Poll once without allowing Tokio's paused clock to auto-advance.
        async fn poll_pending(future: Pin<&mut impl Future<Output = ()>>) {
            let mut future = future;
            std::future::poll_fn(|cx| {
                assert!(future.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
        }

        assert_eq!(retry_delay(Duration::ZERO), Duration::from_secs(10));
        assert_eq!(
            retry_delay(Duration::from_secs(299)),
            Duration::from_secs(10)
        );
        assert_eq!(
            retry_delay(Duration::from_secs(300)),
            Duration::from_secs(60)
        );

        let v4 = ips(&["100.100.1.2"])[0];
        let v6 = ips(&["fd7a:115c:a1e0::1"])[0];
        let visible = usable_ips(&[
            ("Tailscale", v4),
            ("Tailscale", v6),
            ("Tailscale", v6),
            ("Ethernet", ips(&["100.72.0.9"])[0]),
            ("OtherVPN", ips(&["fd7a:115c:a1e0::7"])[0]),
            ("Ethernet", ips(&["192.168.1.5"])[0]),
            ("Ethernet", ips(&["0.0.0.0"])[0]),
            ("Ethernet", ips(&["::"])[0]),
        ]);
        // Already served v4 + later v6 (both retry rates), partial eager or
        // handoff failure, and no successful tailnet binds after handoff.
        for (seed, late, idle) in [
            (vec![v4], true, 300),
            (vec![v4], true, 0),
            (vec![v4], false, 0),
            (vec![], false, 0),
        ] {
            let snapshot = Rc::new(RefCell::new(if late { vec![v4] } else { visible.clone() }));
            let snapshots = Rc::new(RefCell::new(0));
            let attempts = Rc::new(RefCell::new(Vec::new()));
            let registered = Rc::new(RefCell::new(seed.clone()));
            let fail_v6 = Rc::new(RefCell::new(true));
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let mut changes = Changes(rx);
            let mut lifecycle = Box::pin(maintain_tailnet_listeners(
                seed.clone(),
                &mut changes,
                || {
                    *snapshots.borrow_mut() += 1;
                    snapshot.borrow().clone()
                },
                |ip| {
                    attempts.borrow_mut().push(ip);
                    if ip == v6 && *fail_v6.borrow() {
                        return false;
                    }
                    assert!(!registered.borrow().contains(&ip), "duplicate listener");
                    registered.borrow_mut().push(ip);
                    true
                },
            ));
            poll_pending(lifecycle.as_mut()).await;
            assert_eq!(&*registered.borrow(), &[v4]);
            if late {
                assert!(attempts.borrow().is_empty());
                let before_idle = *snapshots.borrow();
                tokio::time::advance(Duration::from_secs(idle)).await;
                poll_pending(lifecycle.as_mut()).await;
                assert_eq!(*snapshots.borrow(), before_idle, "idle must have no timer");
                *snapshot.borrow_mut() = visible.clone();
                tx.try_send(()).unwrap();
                poll_pending(lifecycle.as_mut()).await;
            } else if seed.is_empty() {
                assert_eq!(&*attempts.borrow(), &[v4, v6]);
                attempts.borrow_mut().remove(0);
            }
            assert_eq!(&*attempts.borrow(), &[v6]);
            assert_eq!(&*registered.borrow(), &[v4], "failed address advertised");

            // A repeated OS event retries only the missing family, never v4.
            tx.try_send(()).unwrap();
            poll_pending(lifecycle.as_mut()).await;
            assert_eq!(&*attempts.borrow(), &[v6, v6]);
            *fail_v6.borrow_mut() = false;
            let delay = if idle == 300 { 60 } else { 10 };
            tokio::time::advance(Duration::from_secs(delay - 1)).await;
            poll_pending(lifecycle.as_mut()).await;
            assert_eq!(attempts.borrow().len(), 2, "retry fired early");
            tokio::time::advance(Duration::from_secs(1)).await;
            poll_pending(lifecycle.as_mut()).await;
            assert_eq!(&*attempts.borrow(), &[v6, v6, v6]);
            assert_eq!(&*registered.borrow(), &[v4, v6]);

            let settled = *snapshots.borrow();
            tokio::time::advance(Duration::from_secs(3600)).await;
            poll_pending(lifecycle.as_mut()).await;
            assert_eq!(
                *snapshots.borrow(),
                settled,
                "successful retry left a timer"
            );
            tx.try_send(()).unwrap();
            poll_pending(lifecycle.as_mut()).await;
            assert_eq!(attempts.borrow().len(), 3, "event duplicated a listener");
        }
    }
}
