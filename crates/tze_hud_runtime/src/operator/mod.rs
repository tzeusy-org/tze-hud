//! Operator endpoints (`/admin/*`): outside the model surface, reachable only
//! with an agent PSK whose allow list names `admin` (see
//! `docs/operations/windows-install.md`).

pub mod handoff;
pub mod install;
pub mod logs;
pub mod screenshot;
pub mod status;
