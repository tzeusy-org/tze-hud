//! Scene event naming: validation of agent event names, used by
//! `tab_switch_on_event`.

pub mod naming;

pub use naming::{NamingError, validate_bare_name};
