//! The panel's sections, in display order. Each reads live facts into rows
//! and applies edits; see [`super::row::Section`] for the contract.

mod agents;
mod diagnostics;
mod input;
mod network;
mod system;
mod visibility;

pub(crate) use agents::AgentsSection;
pub(crate) use diagnostics::DiagnosticsSection;
pub(crate) use input::InputSection;
pub(crate) use network::NetworkSection;
pub(crate) use system::SystemSection;
pub(crate) use visibility::VisibilitySection;

/// How often an open panel re-reads outside state that is costly to read
/// (Windows Firewall rules, the registry). A change made from the panel
/// re-reads at once; one made elsewhere shows within this.
const RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether `checked` is older than [`RECHECK`]; if so, stamps it now.
fn due(checked: &std::sync::Mutex<Option<std::time::Instant>>) -> bool {
    let mut checked = checked.lock().unwrap_or_else(|e| e.into_inner());
    let due = checked.is_none_or(|at| at.elapsed() >= RECHECK);
    if due {
        *checked = Some(std::time::Instant::now());
    }
    due
}
