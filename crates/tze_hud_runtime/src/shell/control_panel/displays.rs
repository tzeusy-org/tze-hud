//! Owner edits cross from the panel task to the main thread once per change.
//! Monitor choices come from the same topology publication as rendering/status.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MonitorChoice {
    pub key: String,
    /// Model+serial alias only when the all-connected matcher resolves it to
    /// this monitor, even if other connected hardware has no usable overlay.
    pub base: Option<String>,
    pub primary: bool,
}

impl MonitorChoice {
    pub(crate) fn label(&self, index: usize) -> String {
        if self.primary {
            format!("Primary · {}", self.key)
        } else {
            format!("Display {} · {}", index + 1, self.key)
        }
    }
}

#[derive(Default)]
struct Assignments {
    configured: HashMap<String, String>,
    monitors: Vec<MonitorChoice>,
    topology: u64,
    pending: bool,
}

#[derive(Clone, Default)]
pub(crate) struct DisplayControl(Arc<Mutex<Assignments>>);

impl DisplayControl {
    pub(crate) fn new(configured: HashMap<String, String>) -> Self {
        Self(Arc::new(Mutex::new(Assignments {
            configured,
            ..Assignments::default()
        })))
    }

    pub(crate) fn publish_monitors(&self, monitors: Vec<MonitorChoice>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.monitors != monitors {
            state.topology += 1;
            state.monitors = monitors;
        }
    }

    pub(crate) fn snapshot(&self) -> (HashMap<String, String>, Vec<MonitorChoice>, u64) {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (
            state.configured.clone(),
            state.monitors.clone(),
            state.topology,
        )
    }

    /// Validate the exact key the rendered row offered, rather than interpreting
    /// its index in a new topology. An unplug cannot redirect the choice.
    /// Returns whether live placement changed; an unchanged choice may still
    /// retry a failed config save.
    pub(crate) fn assign(
        &self,
        zone: &str,
        key: Option<&str>,
        topology: u64,
    ) -> Result<bool, String> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.topology != topology {
            return Err("Display choices changed; choose again".into());
        }
        if state.configured.get(zone).map(String::as_str) == key {
            return Ok(false);
        }
        if let Some(key) = key {
            if !state.monitors.iter().any(|monitor| monitor.key == key) {
                return Err(
                    "That display is no longer available; choose a connected display".into(),
                );
            }
            state.configured.insert(zone.to_owned(), key.to_owned());
        } else {
            state.configured.remove(zone);
        }
        state.pending = true;
        Ok(true)
    }

    /// Coalesce edits made before the next main-thread turn without a timer.
    pub(crate) fn take_pending(&self) -> Option<HashMap<String, String>> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut state.pending).then(|| state.configured.clone())
    }
}
