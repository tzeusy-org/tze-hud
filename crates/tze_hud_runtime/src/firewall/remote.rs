//! Explicit, reversible Windows Firewall changes. The diagnostic reader remains
//! separate: its lossy snapshot must never authorize a deletion or undo image.

use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod windows;

pub const CHILD_FLAG: &str = "--tze-hud-firewall-child";
const VERSION: u32 = 1;
const NAME: &str = "tze_hud (tailnet)";
const GROUP: &str = "tze_hud/firewall/v1";
const TAILNET: &str = "100.64.0.0/10,fd7a:115c:a1e0::/48";
const MAX_PAYLOAD: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAction {
    Allow,
    Disallow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildRequest {
    version: u32,
    action: RemoteAction,
    program: String,
    ports: [u16; 2],
    parent_pid: u32,
    parent_created: u64,
    nonce: String,
}

impl ChildRequest {
    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION
            || self.program.is_empty()
            || self.program.contains('\0')
            || self.parent_pid == 0
            || self.parent_created == 0
            || self.nonce.len() != 32
            || !self.nonce.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid internal firewall request".into());
        }
        if serde_json::to_string(self)
            .map_err(|_| "internal payload serialization failed")?
            .len()
            > MAX_PAYLOAD
        {
            return Err("internal firewall payload exceeds its size limit".into());
        }
        if self.action == RemoteAction::Allow && self.ports == [0, 0] {
            return Err("--allow-remote requires at least one nonzero listen port".into());
        }
        Ok(())
    }
}

/// Recognize the private child before reading the alternate administrator's
/// environment. A child token is independently checked by the Windows backend.
pub fn parse_private_child(args: &[String]) -> Result<Option<ChildRequest>, String> {
    if !args.iter().any(|a| a == CHILD_FLAG) {
        return Ok(None);
    }
    if args.len() != 2 || args[0] != CHILD_FLAG || args[1].len() > MAX_PAYLOAD {
        return Err("invalid internal firewall arguments".into());
    }
    let request: ChildRequest = serde_json::from_str(&args[1])
        .map_err(|_| "invalid internal firewall payload".to_string())?;
    request.validate()?;
    Ok(Some(request))
}

/// Only the app's explicit early action calls this; no runtime/startup hook does.
pub fn execute(action: RemoteAction, ports: [u16; 2]) -> Result<String, String> {
    if action == RemoteAction::Allow && ports == [0, 0] {
        return Err("--allow-remote requires at least one nonzero listen port".into());
    }
    #[cfg(windows)]
    {
        windows::execute(action, ports)
    }
    #[cfg(not(windows))]
    {
        let _ = (action, ports);
        Err("--allow-remote and --disallow-remote are Windows-only commands".into())
    }
}

pub fn execute_child(request: ChildRequest) -> Result<String, String> {
    request.validate()?;
    #[cfg(windows)]
    {
        windows::execute_child(request)
    }
    #[cfg(not(windows))]
    {
        Err("the firewall child is Windows-only".into())
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildResult {
    version: u32,
    request: ChildRequest,
    original_sid: String,
    exit: u32,
    message: String,
    actual_changes: Vec<String>,
    classification: String,
    recovery_state: String,
}

#[cfg_attr(not(windows), allow(dead_code))]
fn validate_child_result(
    report: ChildResult,
    request: &ChildRequest,
    original_sid: &str,
    actual_exit: u32,
) -> Result<String, String> {
    if report.version != VERSION
        || report.request != *request
        || report.original_sid != original_sid
        || report.exit != actual_exit
    {
        return Err(
            "child result/exit identity mismatch; inspect the protected recovery journal".into(),
        );
    }
    if actual_exit == 0 {
        Ok(report.message)
    } else {
        Err(report.message)
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
trait Launcher {
    fn elevated(&mut self) -> Result<bool, String>;
    fn run_local(&mut self, request: &ChildRequest) -> Result<String, String>;
    /// Exactly one runas launch, a real process-handle wait, and validated result.
    fn launch_and_wait(&mut self, request: &ChildRequest) -> Result<String, String>;
}

#[cfg_attr(not(windows), allow(dead_code))]
fn control(
    launcher: &mut impl Launcher,
    request: &ChildRequest,
    private_child: bool,
) -> Result<String, String> {
    request.validate()?;
    if launcher.elevated()? {
        launcher.run_local(request)
    } else if private_child {
        Err("the internal firewall child is not elevated; no policy changes".into())
    } else {
        launcher.launch_and_wait(request)
    }
}

// Everything below also compiles in the pure Linux fixtures. None of the store
// operations is reachable from a non-Windows public invocation.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct RuleSpec {
    program: String,
    ports: Vec<u16>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl RuleSpec {
    fn new(program: String, ports: [u16; 2]) -> Result<Self, String> {
        let mut ports: Vec<_> = ports.into_iter().filter(|p| *p != 0).collect();
        ports.sort_unstable();
        ports.dedup();
        if program.is_empty() || program.contains('\0') || ports.is_empty() {
            return Err("an exact executable and nonzero ports are required".into());
        }
        Ok(Self { program, ports })
    }

    fn rule(&self, program_key: &str) -> RuleImage {
        RuleImage {
            name: NAME.into(),
            description: format!("{GROUP}/{program_key}"),
            application: self.program.clone(),
            service: String::new(),
            protocol: 6,
            local_ports: Some(
                self.ports
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            remote_ports: Some("*".into()),
            icmp: None,
            local_addresses: "*".into(),
            remote_addresses: TAILNET.into(),
            direction: 1,
            interfaces: None,
            interface_types: "All".into(),
            enabled: true,
            grouping: GROUP.into(),
            profiles: 0x7fff_ffff,
            edge_traversal: false,
            action: 1,
            edge_options: 0,
            package_id: String::new(),
            user_owner: String::new(),
            local_users: String::new(),
            remote_users: String::new(),
            remote_machines: String::new(),
            secure_flags: 0,
        }
    }
}

/// Accept native formatting only when it preserves the two exact remote ranges
/// and at most two exact ports. Every other property still compares completely.
#[cfg_attr(not(windows), allow(dead_code))]
fn equivalent_allow(observed: &RuleImage, expected: &RuleImage) -> bool {
    let ports = |text: &str| -> Option<Vec<u16>> {
        let mut values = Vec::new();
        for token in text.split(',') {
            if let Some((low, high)) = token.trim().split_once('-') {
                let low = low.trim().parse::<u16>().ok()?;
                let high = high.trim().parse::<u16>().ok()?;
                if high < low || u32::from(high) - u32::from(low) > 1 {
                    return None;
                }
                values.extend(low..=high);
            } else {
                values.push(token.trim().parse().ok()?);
            }
            values.sort_unstable();
            values.dedup();
            if values.len() > 2 {
                return None;
            }
        }
        Some(values)
    };
    let ranges = |text: &str| -> Option<Vec<(bool, u128, u128)>> {
        let mut values: Vec<_> = text
            .split(',')
            .map(super::parse_remote)
            .collect::<Option<_>>()?;
        values.sort_unstable();
        values.dedup();
        Some(values)
    };
    if observed.local_ports.as_deref().and_then(ports)
        != expected.local_ports.as_deref().and_then(ports)
        || ranges(&observed.remote_addresses) != ranges(&expected.remote_addresses)
    {
        return false;
    }
    let mut normalized = observed.clone();
    normalized.local_ports.clone_from(&expected.local_ports);
    normalized
        .remote_addresses
        .clone_from(&expected.remote_addresses);
    let any = |value: &str| value.is_empty() || value == "*" || value.eq_ignore_ascii_case("any");
    if observed.remote_ports.as_deref().is_some_and(any) {
        normalized.remote_ports.clone_from(&expected.remote_ports);
    }
    if any(&observed.local_addresses) {
        normalized
            .local_addresses
            .clone_from(&expected.local_addresses);
    }
    if observed
        .interface_types
        .eq_ignore_ascii_case(&expected.interface_types)
    {
        normalized
            .interface_types
            .clone_from(&expected.interface_types);
    }
    normalized == *expected
}

/// Complete exposed rule state, never the diagnostic `Rule` subset. Applicable
/// protocol attributes and typed interface arrays are preserved without raw
/// VARIANT/pointer serialization. Native roundtrip preflight rejects anything
/// this representation cannot recreate exactly.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleImage {
    name: String,
    description: String,
    application: String,
    service: String,
    protocol: i32,
    local_ports: Option<String>,
    remote_ports: Option<String>,
    icmp: Option<String>,
    local_addresses: String,
    remote_addresses: String,
    direction: i32,
    interfaces: Option<Vec<String>>,
    interface_types: String,
    enabled: bool,
    grouping: String,
    profiles: i32,
    edge_traversal: bool,
    action: i32,
    edge_options: i32,
    package_id: String,
    user_owner: String,
    local_users: String,
    remote_users: String,
    remote_machines: String,
    secure_flags: i32,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl RuleImage {
    fn owned_by(&self, key: &str) -> bool {
        self.name == NAME && self.grouping == GROUP && self.description == format!("{GROUP}/{key}")
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    before: Option<RuleImage>,
    after: Option<RuleImage>,
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedBlock {
    original: RuleImage,
    alias: String,
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    action: RemoteAction,
    changes: Vec<Change>,
    /// Intents are flushed before a write. This cursor includes the next write,
    /// which may have happened if the process crashed before the completion save.
    attempted: usize,
    completed: usize,
    prior_allow: Option<RuleImage>,
    prior_blocks: Vec<SavedBlock>,
    prior_active: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    program: String,
    original_sid: String,
    baseline_allow: Option<RuleImage>,
    current_allow: Option<RuleImage>,
    blocks: Vec<SavedBlock>,
    active: bool,
    pending: Option<Pending>,
}

#[cfg_attr(not(windows), allow(dead_code))]
trait Store {
    fn rules(&mut self) -> Result<Vec<RuleImage>, String>;
    fn load(&mut self) -> Result<Option<Journal>, String>;
    /// Protected atomic replacement + FlushFileBuffers, before any policy write.
    fn save(&mut self, journal: &Journal) -> Result<(), String>;
    fn remove_journal(&mut self) -> Result<(), String>;
    fn preflight_rule(&mut self, rule: &RuleImage) -> Result<(), String>;
    fn prepare_allow(&mut self, spec: &RuleSpec, key: &str) -> Result<RuleImage, String> {
        let rule = spec.rule(key);
        self.preflight_rule(&rule)?;
        Ok(rule)
    }
    fn same_program(&mut self, application: &str, program: &str) -> Result<bool, String>;
    /// Check exact full images and uniqueness again immediately before mutation.
    fn apply(&mut self, change: &Change) -> Result<(), String>;
    fn allowed(&mut self, spec: &RuleSpec) -> Result<bool, String>;
}

#[cfg_attr(not(windows), allow(dead_code))]
fn count(rules: &[RuleImage], image: &RuleImage) -> usize {
    rules.iter().filter(|r| *r == image).count()
}

#[cfg_attr(not(windows), allow(dead_code))]
fn observed_after(rules: &[RuleImage], change: &Change) -> Result<bool, String> {
    match (&change.before, &change.after) {
        (Some(before), Some(after)) => match (count(rules, before), count(rules, after)) {
            (1, 0) => Ok(false),
            (0, 1) => Ok(true),
            _ => Err("rule rename state changed externally; recovery journal retained".into()),
        },
        (Some(before), None) => match count(rules, before) {
            1 => Ok(false),
            0 if !rules.iter().any(|r| r.name == before.name) => Ok(true),
            _ => Err("rule removal state changed externally; recovery journal retained".into()),
        },
        (None, Some(after)) => match count(rules, after) {
            1 => Ok(true),
            0 if !rules.iter().any(|r| r.name == after.name) => Ok(false),
            _ => Err("rule addition state changed externally; recovery journal retained".into()),
        },
        (None, None) => Err("empty firewall change".into()),
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn recover(store: &mut impl Store, journal: &mut Journal) -> Result<(), String> {
    let Some(mut pending) = journal.pending.clone() else {
        return Ok(());
    };
    // Compensate only this attempt, preserving the first baseline and earlier
    // successful allow state. BLOCKs are restored before the owned allow is undone.
    while pending.attempted > 0 {
        let i = pending.attempted - 1;
        let change = &pending.changes[i];
        if observed_after(&store.rules()?, change)? {
            store.apply(&Change {
                before: change.after.clone(),
                after: change.before.clone(),
            })?;
        }
        pending.attempted = i;
        pending.completed = pending.completed.min(i);
        journal.pending = Some(pending.clone());
        store.save(journal)?;
    }
    journal.current_allow = pending.prior_allow;
    journal.blocks = pending.prior_blocks;
    journal.active = pending.prior_active;
    journal.pending = None;
    store.save(journal)
}

/// The journal is undo authority for this exact operation, never a policy import.
#[cfg_attr(not(windows), allow(dead_code))]
fn validate_journal(store: &mut impl Store, journal: &Journal, key: &str) -> Result<(), String> {
    if journal.active != journal.current_allow.is_some() {
        return Err("inconsistent journal active/owned-rule state".into());
    }
    let prefix = format!("tze_hud-undo-{key}-");
    let valid_alias = |name: &str| {
        name.strip_prefix(&prefix)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    let validate_image = |store: &mut dyn Store, rule: &RuleImage| -> Result<(), String> {
        if !store.same_program(&rule.application, &journal.program)?
            || rule.direction != 1
            || !(rule.action == 0 || (rule.action == 1 && rule.owned_by(key)))
            || (rule.action == 0
                && rule.name.starts_with("tze_hud-undo-")
                && !valid_alias(&rule.name))
        {
            return Err("journal contains an out-of-scope rule; no policy changes".into());
        }
        store.preflight_rule(rule)
    };
    for allow in [&journal.baseline_allow, &journal.current_allow]
        .into_iter()
        .flatten()
    {
        if !allow.owned_by(key) || allow.action != 1 {
            return Err("invalid owned ALLOW journal image".into());
        }
        validate_image(store, allow)?;
    }
    for block in &journal.blocks {
        if block.original.action != 0 || !valid_alias(&block.alias) {
            return Err("invalid BLOCK undo journal image".into());
        }
        validate_image(store, &block.original)?;
    }
    if let Some(pending) = &journal.pending {
        if pending.completed > pending.attempted || pending.attempted > pending.changes.len() {
            return Err("invalid journal transaction cursor".into());
        }
        for change in &pending.changes {
            if change.before.is_none() && change.after.is_none() {
                return Err("empty journal change".into());
            }
            for rule in [&change.before, &change.after].into_iter().flatten() {
                validate_image(store, rule)?;
            }
            if let (Some(before), Some(after)) = (&change.before, &change.after) {
                let mut renamed = before.clone();
                renamed.name.clone_from(&after.name);
                if renamed != *after {
                    return Err("journal change is not an exact rename".into());
                }
            }
        }
        for block in &pending.prior_blocks {
            if block.original.action != 0 || !valid_alias(&block.alias) {
                return Err("invalid prior BLOCK image".into());
            }
            validate_image(store, &block.original)?;
        }
        if let Some(allow) = &pending.prior_allow {
            validate_image(store, allow)?;
        }
    }
    Ok(())
}

#[cfg_attr(not(windows), allow(dead_code))]
fn transact(
    store: &mut impl Store,
    action: RemoteAction,
    program: &str,
    sid: &str,
    key: &str,
    spec: Option<&RuleSpec>,
) -> Result<Vec<String>, String> {
    let loaded = store.load()?;
    let mut journal = loaded.clone().unwrap_or(Journal {
        version: VERSION,
        program: program.into(),
        original_sid: sid.into(),
        baseline_allow: None,
        current_allow: None,
        blocks: Vec::new(),
        active: false,
        pending: None,
    });
    if journal.version != VERSION || journal.program != program || journal.original_sid != sid {
        return Err("journal version/program/original owner mismatch; no policy changes".into());
    }
    validate_journal(store, &journal, key)?;
    recover(store, &mut journal)?;
    let rules = store.rules()?;
    let named: Vec<_> = rules.iter().filter(|r| r.name == NAME).collect();
    if action == RemoteAction::Disallow
        && loaded.is_none()
        && !named.iter().any(|r| r.owned_by(key))
    {
        return Ok(vec![
            "No helper journal or owned rule; nothing to undo.".into(),
        ]);
    }
    if named.len() > 1 || named.first().is_some_and(|r| !r.owned_by(key)) {
        return Err("tze_hud (tailnet) name collision; foreign rules were not changed".into());
    }
    if named.first().copied() != journal.current_allow.as_ref() {
        return Err("owned rule/journal fingerprint mismatch; no policy changes".into());
    }
    if action == RemoteAction::Disallow && loaded.is_none() {
        return Ok(vec![
            "No helper journal or owned rule; nothing to undo.".into(),
        ]);
    }
    let prior_allow = journal.current_allow.clone();
    let prior_blocks = journal.blocks.clone();
    let prior_active = journal.active;
    let mut changes = Vec::new();
    let new_allow = match action {
        RemoteAction::Allow => {
            let spec = spec.ok_or("missing validated allow specification")?;
            let allow = store.prepare_allow(spec, key)?;
            if journal.current_allow.as_ref() != Some(&allow) {
                if let Some(old) = &journal.current_allow {
                    changes.push(Change {
                        before: Some(old.clone()),
                        after: None,
                    });
                }
                changes.push(Change {
                    before: None,
                    after: Some(allow.clone()),
                });
            }
            for rule in &rules {
                if rule.direction != 1
                    || rule.action != 0
                    || !store.same_program(&rule.application, program)?
                {
                    continue;
                }
                store.preflight_rule(rule)?;
                if count(&rules, rule) != 1 {
                    return Err(
                        "indistinguishable duplicate BLOCK rules cannot be backed up safely".into(),
                    );
                }
                if journal.blocks.iter().any(|b| b.original == *rule) {
                    return Err(
                        "a saved BLOCK was recreated/edited externally; journal retained".into(),
                    );
                }
                let alias = format!("tze_hud-undo-{key}-{}", journal.blocks.len());
                if rules.iter().any(|r| r.name == alias) {
                    return Err("temporary rule alias collision; no policy changes".into());
                }
                let mut renamed = rule.clone();
                renamed.name.clone_from(&alias);
                changes.push(Change {
                    before: Some(rule.clone()),
                    after: Some(renamed.clone()),
                });
                changes.push(Change {
                    before: Some(renamed),
                    after: None,
                });
                journal.blocks.push(SavedBlock {
                    original: rule.clone(),
                    alias,
                });
            }
            Some(allow)
        }
        RemoteAction::Disallow => {
            for block in &journal.blocks {
                store.preflight_rule(&block.original)?;
                if count(&rules, &block.original) != 0
                    || rules.iter().any(|r| r.name == block.alias)
                {
                    return Err(
                        "BLOCK undo conflicts with an external rule edit; journal retained".into(),
                    );
                }
                let mut renamed = block.original.clone();
                renamed.name.clone_from(&block.alias);
                changes.push(Change {
                    before: None,
                    after: Some(renamed.clone()),
                });
                changes.push(Change {
                    before: Some(renamed),
                    after: Some(block.original.clone()),
                });
            }
            if let Some(old) = &journal.current_allow {
                changes.push(Change {
                    before: Some(old.clone()),
                    after: None,
                });
            }
            if let Some(baseline) = &journal.baseline_allow {
                store.preflight_rule(baseline)?;
                changes.push(Change {
                    before: None,
                    after: Some(baseline.clone()),
                });
            }
            journal.baseline_allow.clone()
        }
    };
    journal.pending = Some(Pending {
        action,
        changes: changes.clone(),
        attempted: 0,
        completed: 0,
        prior_allow,
        prior_blocks,
        prior_active,
    });
    store.save(&journal)?;
    let operation: Result<Vec<String>, String> = (|| {
        let mut messages = Vec::new();
        for (i, change) in changes.iter().enumerate() {
            journal
                .pending
                .as_mut()
                .expect("prepared transaction")
                .attempted = i + 1;
            store.save(&journal)?;
            if observed_after(&store.rules()?, change)? {
                return Err("firewall changed externally before the write".into());
            }
            store.apply(change)?;
            if !observed_after(&store.rules()?, change)? {
                return Err("firewall write did not produce its complete expected image".into());
            }
            journal
                .pending
                .as_mut()
                .expect("prepared transaction")
                .completed = i + 1;
            store.save(&journal)?;
            messages.push(match (&change.before, &change.after) {
                (Some(before), Some(after)) => format!("Renamed {} -> {}", before.name, after.name),
                (Some(before), None) => format!("Removed {} (program {}, protocol {}, ports {:?}, remotes {}, profiles {}, enabled {})", before.name, before.application, before.protocol, before.local_ports, before.remote_addresses, before.profiles, before.enabled),
                (None, Some(after)) => format!("Added {}", after.name),
                _ => unreachable!(),
            });
        }
        if action == RemoteAction::Allow && !store.allowed(spec.expect("allow has spec"))? {
            return Err("the fresh detector still reports blocked/unknown; unrelated policy was not changed".into());
        }
        Ok(messages)
    })();
    let mut messages = match operation {
        Ok(messages) => messages,
        Err(error) => {
            return match recover(store, &mut journal) {
                Ok(()) => Err(format!("{error}; this attempt was rolled back")),
                Err(undo) => Err(format!(
                    "{error}; incomplete rollback: {undo}; protected recovery journal retained"
                )),
            };
        }
    };
    // Keep the last flushed intent available until final durability succeeds.
    // A failed final save must compensate just like a failed policy write.
    let finalization = (|| {
        if action == RemoteAction::Disallow {
            let current = store.rules()?;
            if journal
                .blocks
                .iter()
                .any(|b| count(&current, &b.original) != 1)
                || current
                    .iter()
                    .any(|r| journal.blocks.iter().any(|b| r.name == b.alias))
            {
                return Err("full BLOCK restoration verification failed".into());
            }
            store.remove_journal()?;
        } else {
            let mut committed = journal.clone();
            committed.current_allow = new_allow;
            committed.active = true;
            committed.pending = None;
            store.save(&committed)?;
        }
        Ok::<_, String>(())
    })();
    if let Err(error) = finalization {
        return match recover(store, &mut journal) {
            Ok(()) => Err(format!("{error}; this attempt was rolled back")),
            Err(undo) => Err(format!(
                "{error}; incomplete rollback: {undo}; protected recovery journal retained"
            )),
        };
    }
    messages.push(if action == RemoteAction::Disallow {
        "Original helper/BLOCK state restored; undo journal removed.".into()
    } else {
        "Fresh policy classification: allowed. Undo is --disallow-remote on this executable. Removing prior BLOCKs can uncover existing broader ALLOW rules; review the removed scopes above.".into()
    });
    Ok(messages)
}

/// Windows CommandLineToArgvW quoting, without a shell or interpolated options.
#[cfg_attr(not(windows), allow(dead_code))]
fn quote_argument(arg: &str) -> Result<String, String> {
    if arg.contains('\0') || arg.encode_utf16().count() > MAX_PAYLOAD {
        return Err("invalid Windows argument".into());
    }
    let mut result = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        result.push(c);
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROGRAM: &str = r"C:\HUD Dev\tze_hud.exe";
    const SID: &str = "S-1-5-21-123";
    const KEY: &str = "fixture";
    fn spec() -> RuleSpec {
        RuleSpec::new(PROGRAM.into(), [9090, 50051]).unwrap()
    }
    fn block(name: &str) -> RuleImage {
        let mut rule = spec().rule(KEY);
        rule.name = name.into();
        rule.grouping = "original group".into();
        rule.description = "original description".into();
        rule.action = 0;
        rule.protocol = 256;
        rule.local_ports = None;
        rule.remote_ports = None;
        rule.remote_addresses = "*".into();
        rule.enabled = false;
        rule.profiles = 2;
        rule.interface_types = "Wireless".into();
        rule.interfaces = Some(vec!["adapter one".into()]);
        rule.edge_options = 2;
        rule.service = "service".into();
        rule.secure_flags = 1;
        rule.local_users = "D:(A;;CC;;;WD)".into();
        rule
    }
    #[derive(Clone, Default)]
    struct Memory {
        rules: Vec<RuleImage>,
        journal: Option<Journal>,
        calls: usize,
        fail: Option<usize>,
        crash_after_apply: Option<usize>,
        writes: usize,
        deny: bool,
    }
    impl Memory {
        fn tick(&mut self) -> Result<(), String> {
            self.calls += 1;
            if self.fail == Some(self.calls) {
                self.fail = None;
                return Err("injected adapter failure".into());
            }
            Ok(())
        }
    }
    impl Store for Memory {
        fn rules(&mut self) -> Result<Vec<RuleImage>, String> {
            self.tick()?;
            Ok(self.rules.clone())
        }
        fn load(&mut self) -> Result<Option<Journal>, String> {
            self.tick()?;
            Ok(self.journal.clone())
        }
        fn save(&mut self, journal: &Journal) -> Result<(), String> {
            self.tick()?;
            self.journal = Some(journal.clone());
            Ok(())
        }
        fn remove_journal(&mut self) -> Result<(), String> {
            self.tick()?;
            self.journal = None;
            Ok(())
        }
        fn preflight_rule(&mut self, rule: &RuleImage) -> Result<(), String> {
            self.tick()?;
            if !rule.package_id.is_empty() {
                return Err("unsupported package rule".into());
            }
            Ok(())
        }
        fn same_program(&mut self, application: &str, program: &str) -> Result<bool, String> {
            self.tick()?;
            Ok(application == program || application == r"%ORIGINAL_OWNER%\tze_hud.exe")
        }
        fn apply(&mut self, change: &Change) -> Result<(), String> {
            self.tick()?;
            if let Some(before) = &change.before {
                let positions: Vec<_> = self
                    .rules
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| *r == before)
                    .map(|(i, _)| i)
                    .collect();
                if positions.len() != 1 {
                    return Err("changed full fingerprint".into());
                }
                if change.after.is_none()
                    && self.rules.iter().filter(|r| r.name == before.name).count() != 1
                {
                    return Err("unsafe name removal".into());
                }
                self.rules.remove(positions[0]);
            }
            if let Some(after) = &change.after {
                self.rules.push(after.clone());
            }
            self.writes += 1;
            if self.crash_after_apply == Some(self.writes) {
                self.crash_after_apply = None;
                panic!("simulated process loss after policy write");
            }
            Ok(())
        }
        fn allowed(&mut self, _: &RuleSpec) -> Result<bool, String> {
            self.tick()?;
            Ok(!self.deny)
        }
    }
    fn run(
        store: &mut Memory,
        action: RemoteAction,
        rule: Option<&RuleSpec>,
    ) -> Result<Vec<String>, String> {
        transact(store, action, PROGRAM, SID, KEY, rule)
    }
    fn multiset(rules: &[RuleImage]) -> Vec<String> {
        let mut result: Vec<_> = rules
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        result.sort();
        result
    }

    #[test]
    fn rule_spec_uses_resolved_program_and_ports() {
        for (program, ports, expected) in [
            (
                r"C:\Users\owner\AppData\Local\tze_hud\tze_hud.exe",
                [9090, 50051],
                vec![9090, 50051],
            ),
            (PROGRAM, [9290, 9290], vec![9290]),
            (PROGRAM, [0, 50052], vec![50052]),
        ] {
            let spec = RuleSpec::new(program.into(), ports).unwrap();
            let rule = spec.rule(KEY);
            assert_eq!(spec.ports, expected);
            assert_eq!(rule.application, program);
            assert_eq!(rule.remote_addresses, TAILNET);
            assert_eq!(rule.protocol, 6);
            assert_eq!(rule.profiles, 0x7fff_ffff);
            assert!(rule.enabled);
            assert!(!rule.edge_traversal);
            assert_eq!((rule.direction, rule.action), (1, 1));
        }
        let expected = spec().rule(KEY);
        let mut formatted = expected.clone();
        formatted.remote_addresses =
            "fd7a:115c:a1e0::/ffff:ffff:ffff::,100.64.0.0/255.192.0.0".into();
        formatted.local_ports = Some("50051,9090".into());
        assert!(equivalent_allow(&formatted, &expected));
        formatted.remote_addresses = "100.64.0.0/10,fd7a:115c:a1e0::/47".into();
        assert!(!equivalent_allow(&formatted, &expected));
        formatted = expected.clone();
        formatted.local_ports = Some("9090,50051,50052".into());
        assert!(!equivalent_allow(&formatted, &expected));
        assert!(RuleSpec::new(PROGRAM.into(), [0, 0]).is_err());
        assert!(RuleSpec::new("bad\0path".into(), [1, 0]).is_err());
    }

    #[test]
    fn program_block_filter_is_exact_and_name_collisions_are_safe() {
        let selected = block("shared name");
        let mut peer = selected.clone();
        peer.application = r"C:\other\tze_hud.exe".into();
        let mut outbound = selected.clone();
        outbound.name = "outbound".into();
        outbound.direction = 2;
        let mut generic = selected.clone();
        generic.name = "generic".into();
        generic.application.clear();
        let initial = vec![selected.clone(), peer.clone(), outbound, generic];
        let mut store = Memory {
            rules: initial.clone(),
            ..Default::default()
        };
        run(&mut store, RemoteAction::Allow, Some(&spec())).unwrap();
        assert!(!store.rules.contains(&selected));
        assert!(store.rules.contains(&peer));
        run(&mut store, RemoteAction::Disallow, None).unwrap();
        assert_eq!(multiset(&store.rules), multiset(&initial));
        for collision in [spec().rule("foreign"), {
            let mut r = spec().rule(KEY);
            r.application = "other".into();
            r
        }] {
            let mut store = Memory {
                rules: vec![collision.clone()],
                ..Default::default()
            };
            assert!(run(&mut store, RemoteAction::Allow, Some(&spec())).is_err());
            assert_eq!(store.rules, vec![collision]);
            assert_eq!(store.writes, 0);
        }
        let mut unsupported = block("package");
        unsupported.package_id = "package identity".into();
        let mut store = Memory {
            rules: vec![unsupported.clone()],
            ..Default::default()
        };
        assert!(run(&mut store, RemoteAction::Allow, Some(&spec())).is_err());
        assert_eq!(store.rules, vec![unsupported]);
        assert_eq!(store.writes, 0);
    }

    #[test]
    fn allow_and_disallow_restore_the_original_policy() {
        let mut expanded = block("expanded original");
        expanded.application = r"%ORIGINAL_OWNER%\tze_hud.exe".into();
        let initial = vec![block("first"), expanded];
        let mut store = Memory {
            rules: initial.clone(),
            ..Default::default()
        };
        run(&mut store, RemoteAction::Allow, Some(&spec())).unwrap();
        let first = store.journal.clone().unwrap();
        run(&mut store, RemoteAction::Allow, Some(&spec())).unwrap();
        assert_eq!(store.journal.as_ref().unwrap().blocks, first.blocks);
        let changed = RuleSpec::new(PROGRAM.into(), [9290, 0]).unwrap();
        run(&mut store, RemoteAction::Allow, Some(&changed)).unwrap();
        assert_eq!(store.journal.as_ref().unwrap().blocks, first.blocks);
        run(&mut store, RemoteAction::Disallow, None).unwrap();
        assert_eq!(multiset(&store.rules), multiset(&initial));
        assert!(store.journal.is_none());
        run(&mut store, RemoteAction::Disallow, None).unwrap();

        // Exercise the production transaction at every adapter boundary, not a
        // second algorithm. One-shot faults also let compensation itself finish.
        for action in [RemoteAction::Allow, RemoteAction::Disallow] {
            let mut baseline = Memory {
                rules: initial.clone(),
                ..Default::default()
            };
            if action == RemoteAction::Disallow {
                run(&mut baseline, RemoteAction::Allow, Some(&spec())).unwrap();
            }
            let before = baseline.rules.clone();
            baseline.calls = 0;
            let mut success = baseline.clone();
            run(&mut success, action, Some(&spec())).unwrap();
            for point in 1..=success.calls {
                let mut failed = baseline.clone();
                failed.fail = Some(point);
                assert!(
                    run(&mut failed, action, Some(&spec())).is_err(),
                    "{action:?} fault {point}"
                );
                assert_eq!(
                    multiset(&failed.rules),
                    multiset(&before),
                    "{action:?} compensation at {point}"
                );
                run(&mut failed, action, Some(&spec())).unwrap();
            }
            for point in 1..=(success.writes - baseline.writes) {
                let mut crashed = baseline.clone();
                crashed.crash_after_apply = Some(baseline.writes + point);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&mut crashed, action, Some(&spec()))
                }));
                assert!(result.is_err());
                run(&mut crashed, action, Some(&spec())).unwrap();
                assert_eq!(multiset(&crashed.rules), multiset(&success.rules));
            }
        }
        let mut denied = Memory {
            rules: initial.clone(),
            deny: true,
            ..Default::default()
        };
        assert!(run(&mut denied, RemoteAction::Allow, Some(&spec())).is_err());
        assert_eq!(multiset(&denied.rules), multiset(&initial));
        let mut edited = Memory {
            rules: initial,
            ..Default::default()
        };
        run(&mut edited, RemoteAction::Allow, Some(&spec())).unwrap();
        edited.rules[0].description.push_str(" externally edited");
        let before = edited.rules.clone();
        assert!(run(&mut edited, RemoteAction::Disallow, None).is_err());
        assert_eq!(edited.rules, before);
        let mut malformed = edited.journal.clone().unwrap();
        malformed.program = "another executable".into();
        edited.journal = Some(malformed);
        assert!(run(&mut edited, RemoteAction::Allow, Some(&spec())).is_err());
        assert_eq!(edited.rules, before);
    }

    #[test]
    fn explicit_remote_action_controls_elevation_and_child_result() {
        struct Launch {
            elevated: bool,
            local: usize,
            prompts: usize,
            outcome: Result<String, String>,
        }
        impl Launcher for Launch {
            fn elevated(&mut self) -> Result<bool, String> {
                Ok(self.elevated)
            }
            fn run_local(&mut self, _: &ChildRequest) -> Result<String, String> {
                self.local += 1;
                self.outcome.clone()
            }
            fn launch_and_wait(&mut self, _: &ChildRequest) -> Result<String, String> {
                self.prompts += 1;
                self.outcome.clone()
            }
        }
        let request = ChildRequest {
            version: VERSION,
            action: RemoteAction::Allow,
            program: PROGRAM.into(),
            ports: [9090, 50051],
            parent_pid: 123,
            parent_created: 456,
            nonce: "1234567890abcdef1234567890abcdef".into(),
        };
        for (elevated, child, outcome) in [
            (true, false, Ok("direct".into())),
            (true, true, Ok("child".into())),
            (false, false, Ok("validated child result".into())),
            (false, false, Err("UAC cancelled".into())),
            (false, false, Err("no process handle".into())),
            (false, false, Err("child exit/result mismatch".into())),
            (false, true, Ok("must not launch".into())),
        ] {
            let mut launch = Launch {
                elevated,
                local: 0,
                prompts: 0,
                outcome: outcome.clone(),
            };
            let result = control(&mut launch, &request, child);
            assert_eq!(launch.prompts, usize::from(!elevated && !child));
            assert_eq!(launch.local, usize::from(elevated));
            if !elevated && child {
                assert!(result.is_err());
            } else {
                assert_eq!(result, outcome);
            }
        }
        let report = ChildResult {
            version: VERSION,
            request: request.clone(),
            original_sid: SID.into(),
            exit: 0,
            message: "actual result".into(),
            actual_changes: vec!["Added fixture rule".into()],
            classification: "allowed".into(),
            recovery_state: "undo_journal_retained".into(),
        };
        assert_eq!(
            validate_child_result(report.clone(), &request, SID, 0).unwrap(),
            "actual result"
        );
        assert!(
            validate_child_result(report.clone(), &request, "alternate administrator SID", 0)
                .is_err()
        );
        assert!(validate_child_result(report.clone(), &request, SID, 1).is_err());
        let mut stale = report.clone();
        stale.request.nonce = "ffffffffffffffffffffffffffffffff".into();
        assert!(validate_child_result(stale, &request, SID, 0).is_err());
        let mut failed = report;
        failed.exit = 1;
        failed.message = "actual child failure".into();
        assert_eq!(
            validate_child_result(failed, &request, SID, 1),
            Err("actual child failure".into())
        );
        let payload = serde_json::to_string(&request).unwrap();
        assert_eq!(
            parse_private_child(&[CHILD_FLAG.into(), payload.clone()]).unwrap(),
            Some(request.clone())
        );
        assert!(
            parse_private_child(&["--allow-remote".into(), CHILD_FLAG.into(), payload.clone()])
                .is_err()
        );
        assert!(
            parse_private_child(&[
                CHILD_FLAG.into(),
                payload.replace("\"version\":1", "\"version\":99")
            ])
            .is_err()
        );
        assert!(parse_private_child(&[CHILD_FLAG.into(), "{}".into()]).is_err());
        for argument in ["", PROGRAM, "a\\\"b", "尾\\", "quote\" and space", &payload] {
            assert!(quote_argument(argument).unwrap().starts_with('"'));
        }
        assert!(quote_argument("bad\0value").is_err());
    }
}
