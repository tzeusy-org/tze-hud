//! The panel's sections, in display order. Each reads live facts into rows
//! and applies edits; see [`super::row::Section`] for the contract.

mod agents;
mod input;
mod system;

pub(crate) use agents::AgentsSection;
pub(crate) use input::InputSection;
pub(crate) use system::SystemSection;
