//! Is Windows Firewall blocking inbound traffic to the HUD's tailnet address?
//!
//! The HUD listens on loopback plus its Tailscale addresses (`net_addrs`). A
//! self-connect cannot tell whether remote agents get through: traffic from the
//! host to its own tailnet IP never crosses the inbound filter. So the Windows
//! firewall policy is read through COM (`INetFwPolicy2`, no admin needed) and
//! [`decide`] classifies it. The check runs on demand and is cached for
//! [`CACHE_TTL`]; it is never on the frame path.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long an answer is reused by `/admin/status` and the pairing card.
pub const CACHE_TTL: Duration = Duration::from_secs(5);

/// Where the owner is pointed for the fix.
pub const FIX_HINT: &str = "see docs/operations/windows-install.md#remote-agents";

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const PROFILE_DOMAIN: u32 = 1;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const PROFILE_PRIVATE: u32 = 2;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const PROFILE_PUBLIC: u32 = 4;

const PROTO_TCP: i32 = 6;
const PROTO_ANY: i32 = 256;

/// One inbound-relevant firewall rule, as read from `INetFwRule`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    pub enabled: bool,
    pub inbound: bool,
    pub allow: bool,
    /// Profile bitmask the rule applies to.
    pub profiles: u32,
    /// Program path; `None` or empty means any program.
    pub application: Option<String>,
    /// `NET_FW_IP_PROTOCOL` value (6 = TCP, 256 = any).
    pub protocol: i32,
    /// Comma list of ports and ranges; `None`, empty or `*` means any.
    pub local_ports: Option<String>,
    /// Comma list of addresses, CIDRs and ranges; empty or `*` means any.
    pub remote_addresses: String,
}

/// The firewall policy state that matters for inbound decisions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FirewallSnapshot {
    /// Profiles whose firewall is on.
    pub enabled: u32,
    /// Profiles whose default inbound action is Block.
    pub default_block: u32,
    /// Profiles currently in effect on this machine.
    pub current: u32,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailnetInbound {
    Allowed,
    Blocked {
        reason: BlockReason,
        rule: Option<String>,
    },
    Unknown {
        error: String,
    },
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    BlockRule,
    NoAllowRule,
}

impl TailnetInbound {
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    /// The `tailnet_inbound` object of `/admin/status`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Allowed => json!({"state": "allowed"}),
            Self::NotApplicable => json!({"state": "not_applicable"}),
            Self::Unknown { error } => json!({"state": "unknown", "error": error}),
            Self::Blocked { reason, rule } => json!({
                "state": "blocked",
                "reason": match reason {
                    BlockReason::BlockRule => "block_rule",
                    BlockReason::NoAllowRule => "no_allow_rule",
                },
                "rule": rule,
                "fix": FIX_HINT,
            }),
        }
    }
}

/// Classify tailnet inbound reachability for this program and its ports.
///
/// Windows rule precedence: a matching block rule wins over any allow rule.
/// Only rules on a profile that is both active and enabled count. `tailnet_bound`
/// false (nothing listens on a tailnet address) is `NotApplicable`; a failed
/// snapshot read is `Unknown`.
pub fn decide(
    tailnet_bound: bool,
    snapshot: &Result<FirewallSnapshot, String>,
    exe: &Path,
    ports: &[u16],
) -> TailnetInbound {
    if !tailnet_bound {
        return TailnetInbound::NotApplicable;
    }
    let snap = match snapshot {
        Ok(s) => s,
        Err(error) => {
            return TailnetInbound::Unknown {
                error: error.clone(),
            };
        }
    };
    let active = snap.current & snap.enabled;
    if active == 0 {
        return TailnetInbound::Allowed;
    }
    let exe = normalize_path(&exe.to_string_lossy());
    let applicable = |r: &&Rule| r.enabled && r.inbound && r.profiles & active != 0;
    let proto_ok = |r: &Rule| r.protocol == PROTO_TCP || r.protocol == PROTO_ANY;
    let app_ok = |r: &Rule| {
        r.application
            .as_deref()
            .is_none_or(|a| a.is_empty() || normalize_path(&expand_env(a, &env_lookup)) == exe)
    };
    // A block rule applies if it hits any of our ports.
    let blocks_a_port = |r: &Rule| {
        let spec = r.local_ports.as_deref().unwrap_or("*");
        ports.is_empty() || ports.iter().any(|p| port_in(spec, *p))
    };

    let candidates = snap.rules.iter().filter(applicable).filter(|r| proto_ok(r));
    if let Some(r) = candidates.clone().find(|r| {
        !r.allow && app_ok(r) && blocks_a_port(r) && remote_hits(&r.remote_addresses, false)
    }) {
        return TailnetInbound::Blocked {
            reason: BlockReason::BlockRule,
            rule: Some(r.name.clone()),
        };
    }
    // Coverage is per port across the union of allow rules, so one rule per
    // port (the common setup) is enough. Block rules above stay any-match.
    let allows = || {
        candidates
            .clone()
            .filter(|r| r.allow && app_ok(r) && remote_hits(&r.remote_addresses, true))
    };
    let covered = if ports.is_empty() {
        allows().next().is_some()
    } else {
        ports
            .iter()
            .all(|p| allows().any(|r| port_in(r.local_ports.as_deref().unwrap_or("*"), *p)))
    };
    if covered {
        return TailnetInbound::Allowed;
    }
    if snap.default_block & active != 0 {
        return TailnetInbound::Blocked {
            reason: BlockReason::NoAllowRule,
            rule: None,
        };
    }
    TailnetInbound::Allowed
}

/// Expand `%VAR%` references (rule paths such as `%ProgramFiles%\\x.exe`);
/// unknown variables are left as written.
fn expand_env(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after
            .find('%')
            .and_then(|end| lookup(&after[..end]).map(|v| (end, v)))
        {
            Some((end, value)) => {
                out.push_str(&value);
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Ports to check: those of the tailnet binds plus `extra`, minus 0 (a
/// disabled listener) and duplicates.
fn probe_ports(tailnet: &[SocketAddr], extra: &[u16]) -> Vec<u16> {
    let mut ports: Vec<u16> = tailnet
        .iter()
        .map(SocketAddr::port)
        .chain(extra.iter().copied())
        .collect();
    ports.retain(|p| *p != 0);
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn normalize_path(p: &str) -> String {
    p.trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .to_ascii_lowercase()
}

/// Whether `port` is in a firewall port spec (`80,8000-9000`, `*`, empty).
/// Keyword specs such as `RPC` match nothing.
fn port_in(spec: &str, port: u16) -> bool {
    let spec = spec.trim();
    if spec.is_empty() || spec == "*" {
        return true;
    }
    spec.split(',').any(|part| {
        let part = part.trim();
        match part.split_once('-') {
            Some((a, b)) => matches!(
                (a.trim().parse::<u16>(), b.trim().parse::<u16>()),
                (Ok(a), Ok(b)) if a <= port && port <= b
            ),
            None => part.parse::<u16>().is_ok_and(|p| p == port),
        }
    })
}

/// Tailscale ranges as `(is_v6, lo, hi)`.
const TAILNET: [(bool, u128, u128); 2] = [
    (false, 0x6440_0000, 0x647F_FFFF),
    (
        true,
        0xfd7a_115c_a1e0_0000_0000_0000_0000_0000,
        0xfd7a_115c_a1e0_ffff_ffff_ffff_ffff_ffff,
    ),
];

fn ip_bits(ip: IpAddr) -> (bool, u128) {
    match ip {
        IpAddr::V4(v) => (false, u32::from(v) as u128),
        IpAddr::V6(v) => (true, u128::from(v)),
    }
}

/// One `RemoteAddresses` entry as `(is_v6, lo, hi)`; `None` for keywords such
/// as `LocalSubnet` and anything unparseable.
fn parse_remote(entry: &str) -> Option<(bool, u128, u128)> {
    let entry = entry.trim();
    if let Some((a, b)) = entry.split_once('-') {
        let (v6, lo) = ip_bits(a.trim().parse().ok()?);
        let (v6b, hi) = ip_bits(b.trim().parse().ok()?);
        return (v6 == v6b).then_some((v6, lo, hi));
    }
    if let Some((ip, mask)) = entry.split_once('/') {
        let (v6, base) = ip_bits(ip.trim().parse().ok()?);
        let width: u32 = if v6 { 128 } else { 32 };
        let full = if v6 { u128::MAX } else { u32::MAX as u128 };
        let host_bits = match mask.trim().parse::<u32>() {
            Ok(prefix) if prefix <= width => width - prefix,
            _ => {
                let (mv6, m) = ip_bits(mask.trim().parse().ok()?);
                if mv6 != v6 {
                    return None;
                }
                (m & full).count_zeros() - (128 - width)
            }
        };
        let host = if host_bits >= 128 {
            u128::MAX
        } else {
            (1u128 << host_bits) - 1
        };
        let lo = base & full & !host;
        return Some((v6, lo, lo | host));
    }
    let (v6, bits) = ip_bits(entry.parse().ok()?);
    Some((v6, bits, bits))
}

/// Does a `RemoteAddresses` list reach the tailnet? `cover` (allow rules) needs
/// a whole tailnet range inside it; otherwise (block rules) any overlap counts.
fn remote_hits(spec: &str, cover: bool) -> bool {
    let spec = spec.trim();
    if spec.is_empty() || spec == "*" || spec.eq_ignore_ascii_case("any") {
        return true;
    }
    spec.split(',')
        .filter_map(parse_remote)
        .any(|(v6, lo, hi)| {
            TAILNET.iter().any(|&(tv6, tlo, thi)| {
                tv6 == v6
                    && if cover {
                        lo <= tlo && hi >= thi
                    } else {
                        lo <= thi && hi >= tlo
                    }
            })
        })
}

/// Reads the policy and decides, with a short cache. Cheap to share.
pub struct FirewallProbe {
    eval: Box<dyn Fn() -> TailnetInbound + Send + Sync>,
    cache: Mutex<Option<(Instant, TailnetInbound)>>,
}

impl FirewallProbe {
    pub fn new(eval: impl Fn() -> TailnetInbound + Send + Sync + 'static) -> Self {
        Self {
            eval: Box::new(eval),
            cache: Mutex::new(None),
        }
    }

    /// The real probe: tailnet addresses come from `binds` at evaluation time,
    /// `extra_ports` (e.g. the gRPC port) are checked alongside their ports.
    pub fn system(binds: Arc<Mutex<Vec<SocketAddr>>>, extra_ports: Vec<u16>) -> Self {
        Self::new(move || {
            let tailnet: Vec<SocketAddr> = binds
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .copied()
                .filter(|a| !crate::net_addrs::tailnet_addrs(&[a.ip()]).is_empty())
                .collect();
            if !cfg!(target_os = "windows") || tailnet.is_empty() {
                return TailnetInbound::NotApplicable;
            }
            let ports = probe_ports(&tailnet, &extra_ports);
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tze_hud.exe"));
            decide(true, &read_snapshot(), &exe, &ports)
        })
    }

    /// A fresh cached answer, if any. Never blocks.
    pub fn cached(&self) -> Option<TailnetInbound> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .as_ref()
            .filter(|(at, _)| at.elapsed() < CACHE_TTL)
            .map(|(_, v)| v.clone())
    }

    /// The cached answer or a fresh evaluation. Blocks on a cache miss (COM
    /// enumeration can take a while): call from a blocking context.
    pub fn current(&self) -> TailnetInbound {
        if let Some(v) = self.cached() {
            return v;
        }
        let v = (self.eval)();
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
        v
    }
}

#[cfg(not(target_os = "windows"))]
fn read_snapshot() -> Result<FirewallSnapshot, String> {
    Err("Windows Firewall is not available on this platform".to_owned())
}

#[cfg(target_os = "windows")]
fn read_snapshot() -> Result<FirewallSnapshot, String> {
    // COM init is per thread; use a private one so the caller's apartment
    // (winit, tokio workers) is never touched.
    std::thread::spawn(win::read_snapshot)
        .join()
        .unwrap_or_else(|_| Err("firewall reader panicked".to_owned()))
}

#[cfg(target_os = "windows")]
mod win {
    use super::{FirewallSnapshot, PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC, Rule};
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        INetFwPolicy2, INetFwRule, NET_FW_ACTION_ALLOW, NET_FW_ACTION_BLOCK, NET_FW_PROFILE_TYPE2,
        NET_FW_RULE_DIR_IN, NetFwPolicy2,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::System::Ole::IEnumVARIANT;
    use windows::Win32::System::Variant::VT_DISPATCH;
    use windows::core::{Interface, VARIANT};

    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: paired with the successful CoInitializeEx below.
            unsafe { CoUninitialize() }
        }
    }

    pub(super) fn read_snapshot() -> Result<FirewallSnapshot, String> {
        // SAFETY: plain COM calls on this thread; every interface is released
        // (dropped) before CoUninitialize because `_com` outlives them.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| format!("CoInitializeEx: {e}"))?;
            let _com = Com;
            read().map_err(|e| format!("INetFwPolicy2: {e}"))
        }
    }

    unsafe fn read() -> windows::core::Result<FirewallSnapshot> {
        let policy: INetFwPolicy2 =
            unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)? };
        let mut snap = FirewallSnapshot {
            current: unsafe { policy.CurrentProfileTypes()? } as u32,
            ..Default::default()
        };
        for bit in [PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC] {
            let profile = NET_FW_PROFILE_TYPE2(bit as i32);
            if unsafe { policy.get_FirewallEnabled(profile)? }.as_bool() {
                snap.enabled |= bit;
            }
            if unsafe { policy.get_DefaultInboundAction(profile)? } == NET_FW_ACTION_BLOCK {
                snap.default_block |= bit;
            }
        }
        let enumerator: IEnumVARIANT = unsafe { policy.Rules()?._NewEnum()?.cast()? };
        loop {
            let mut item = [VARIANT::default()];
            let mut fetched = 0u32;
            // SAFETY: one-element buffer; `fetched` is a live local.
            if unsafe { enumerator.Next(&mut item, &mut fetched) }.0 != 0 || fetched == 0 {
                break;
            }
            if let Some(rule) = unsafe { rule_of(&item[0]) } {
                if let Some(r) = unsafe { to_rule(&rule) } {
                    snap.rules.push(r);
                }
            }
        }
        Ok(snap)
    }

    unsafe fn rule_of(v: &VARIANT) -> Option<INetFwRule> {
        let raw = v.as_raw();
        // SAFETY: the union member is read only after checking `vt`; the pointer
        // is borrowed from the VARIANT, which outlives this call.
        unsafe {
            let inner = &raw.Anonymous.Anonymous;
            if inner.vt != VT_DISPATCH.0 {
                return None;
            }
            let punk = windows::core::IUnknown::from_raw_borrowed(&inner.Anonymous.pdispVal)?;
            punk.cast::<INetFwRule>().ok()
        }
    }

    unsafe fn to_rule(r: &INetFwRule) -> Option<Rule> {
        // A rule whose properties cannot be read is skipped, not fatal.
        // SAFETY: read-only property getters on a live interface.
        unsafe {
            let text = |b: windows::core::Result<windows::core::BSTR>| {
                b.map(|b| b.to_string()).unwrap_or_default()
            };
            Some(Rule {
                name: text(r.Name()),
                enabled: r.Enabled().ok()?.as_bool(),
                inbound: r.Direction().ok()? == NET_FW_RULE_DIR_IN,
                allow: r.Action().ok()? == NET_FW_ACTION_ALLOW,
                profiles: r.Profiles().ok()? as u32,
                application: Some(text(r.ApplicationName())).filter(|s| !s.is_empty()),
                protocol: r.Protocol().ok()?,
                local_ports: Some(text(r.LocalPorts())).filter(|s| !s.is_empty()),
                remote_addresses: text(r.RemoteAddresses()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NET_FW_PROFILE2_ALL`.
    const PROFILE_ALL: u32 = 0x7FFF_FFFF;
    const EXE: &str = r"C:\Program Files\tze_hud\tze_hud.exe";
    const PORTS: &[u16] = &[9090, 50051];

    fn rule(name: &str, allow: bool) -> Rule {
        Rule {
            name: name.into(),
            enabled: true,
            inbound: true,
            allow,
            profiles: PROFILE_ALL,
            application: None,
            protocol: PROTO_ANY,
            local_ports: None,
            remote_addresses: "*".into(),
        }
    }

    fn program(mut r: Rule) -> Rule {
        r.application = Some(EXE.to_ascii_uppercase());
        r
    }

    fn ports(mut r: Rule, spec: &str) -> Rule {
        r.local_ports = Some(spec.into());
        r.protocol = PROTO_TCP;
        r
    }

    /// Public profile active, firewall on, default inbound Block.
    fn snap(rules: Vec<Rule>) -> FirewallSnapshot {
        FirewallSnapshot {
            enabled: PROFILE_PUBLIC,
            default_block: PROFILE_PUBLIC,
            current: PROFILE_PUBLIC,
            rules,
        }
    }

    fn run(s: FirewallSnapshot) -> TailnetInbound {
        decide(true, &Ok(s), Path::new(EXE), PORTS)
    }

    fn blocked_by(name: &str) -> TailnetInbound {
        TailnetInbound::Blocked {
            reason: BlockReason::BlockRule,
            rule: Some(name.into()),
        }
    }

    #[test]
    fn block_rule_beats_allow_rule() {
        let s = snap(vec![
            program(rule("allow-exe", true)),
            program(rule("deny-exe", false)),
        ]);
        assert_eq!(run(s), blocked_by("deny-exe"));
    }

    #[test]
    fn program_allow_rule_allows_case_insensitively() {
        assert_eq!(
            run(snap(vec![program(rule("exe", true))])),
            TailnetInbound::Allowed
        );
    }

    #[test]
    fn port_allow_rule_allows_only_when_all_our_ports_are_covered() {
        let all = ports(rule("ports", true), "9090,50000-50100");
        assert_eq!(run(snap(vec![all])), TailnetInbound::Allowed);
        let some = ports(rule("one-port", true), "9090");
        assert!(run(snap(vec![some])).is_blocked());
    }

    #[test]
    fn allow_rule_for_other_remote_addresses_does_not_count() {
        let mut lan = program(rule("lan-only", true));
        lan.remote_addresses = "192.168.0.0/16,LocalSubnet".into();
        assert!(run(snap(vec![lan])).is_blocked());
        let mut tail = program(rule("tailnet", true));
        tail.remote_addresses = "100.64.0.0/255.192.0.0".into();
        assert_eq!(run(snap(vec![tail])), TailnetInbound::Allowed);
        let mut narrow = program(rule("one-peer", true));
        narrow.remote_addresses = "100.100.1.1".into();
        assert!(run(snap(vec![narrow])).is_blocked());
    }

    #[test]
    fn no_rule_with_default_block_is_blocked_and_default_allow_is_not() {
        assert_eq!(
            run(snap(vec![])),
            TailnetInbound::Blocked {
                reason: BlockReason::NoAllowRule,
                rule: None
            }
        );
        let mut open = snap(vec![]);
        open.default_block = 0;
        assert_eq!(run(open), TailnetInbound::Allowed);
    }

    #[test]
    fn disabled_firewall_allows_even_with_a_block_rule() {
        let mut s = snap(vec![program(rule("deny-exe", false))]);
        s.enabled = 0;
        assert_eq!(run(s), TailnetInbound::Allowed);
    }

    #[test]
    fn rules_on_inactive_profiles_and_disabled_rules_are_ignored() {
        let mut deny = program(rule("deny-private", false));
        deny.profiles = PROFILE_PRIVATE;
        let mut allow = program(rule("allow-private", true));
        allow.profiles = PROFILE_PRIVATE;
        let mut off = program(rule("deny-off", false));
        off.enabled = false;
        // Public is active: the private block rule and disabled rule do not
        // apply, and the private allow rule does not help either.
        let s = snap(vec![deny.clone(), allow.clone(), off]);
        assert_eq!(
            run(s),
            TailnetInbound::Blocked {
                reason: BlockReason::NoAllowRule,
                rule: None
            }
        );
        let mut private_active = snap(vec![deny]);
        private_active.current = PROFILE_PRIVATE;
        private_active.enabled = PROFILE_PRIVATE;
        assert_eq!(run(private_active), blocked_by("deny-private"));
    }

    #[test]
    fn one_allow_rule_per_port_covers_all_ports() {
        let split = vec![
            ports(rule("mcp", true), "9090"),
            ports(rule("grpc", true), "50051"),
        ];
        assert_eq!(run(snap(split)), TailnetInbound::Allowed);
        // Both ports must be covered by some rule.
        assert!(run(snap(vec![ports(rule("mcp", true), "9090")])).is_blocked());
    }

    #[test]
    fn rule_paths_expand_environment_variables() {
        let lookup = |k: &str| (k == "ProgramFiles").then(|| r"C:\Program Files".to_owned());
        assert_eq!(
            expand_env(r"%ProgramFiles%\tze_hud\tze_hud.exe", &lookup),
            EXE
        );
        assert_eq!(expand_env("100%", &lookup), "100%");
        assert_eq!(expand_env(r"%Nope%\x", &lookup), r"%Nope%\x");
    }

    #[test]
    fn probe_ports_are_the_bound_ones_without_zero_or_duplicates() {
        let binds: Vec<SocketAddr> = vec!["100.100.1.2:9290".parse().unwrap()];
        assert_eq!(probe_ports(&binds, &[50051, 0, 9290]), vec![9290, 50051]);
    }

    #[test]
    fn com_error_is_unknown() {
        let got = decide(true, &Err("E_ACCESSDENIED".into()), Path::new(EXE), PORTS);
        assert_eq!(
            got,
            TailnetInbound::Unknown {
                error: "E_ACCESSDENIED".into()
            }
        );
    }

    #[test]
    fn no_tailnet_bind_is_not_applicable_even_if_the_snapshot_failed() {
        assert_eq!(
            decide(false, &Err("x".into()), Path::new(EXE), PORTS),
            TailnetInbound::NotApplicable
        );
    }

    #[test]
    fn json_shape_and_probe_caching() {
        assert_eq!(
            TailnetInbound::Allowed.to_json(),
            json!({"state": "allowed"})
        );
        let j = blocked_by("deny-exe").to_json();
        assert_eq!(j["state"], "blocked");
        assert_eq!(j["reason"], "block_rule");
        assert_eq!(j["rule"], "deny-exe");
        assert_eq!(j["fix"], FIX_HINT);

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let probe = FirewallProbe::new(move || {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            TailnetInbound::Allowed
        });
        assert_eq!(probe.cached(), None);
        probe.current();
        probe.current();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn system_probe_off_windows_is_not_applicable() {
        let binds = Arc::new(Mutex::new(vec!["100.100.1.2:9090".parse().unwrap()]));
        assert_eq!(
            FirewallProbe::system(binds, vec![]).current(),
            TailnetInbound::NotApplicable
        );
    }
}
