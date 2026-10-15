//! Agents: pause and resume every agent.

use super::super::pause::PauseControl;
use super::super::row::{BoxFuture, Control, Edit, Row, Section};

pub(crate) struct AgentsSection {
    pause: PauseControl,
}

impl AgentsSection {
    pub(crate) const PAUSE: &'static str = "agents.pause";

    pub(crate) fn new(pause: PauseControl) -> Self {
        Self { pause }
    }
}

impl Section for AgentsSection {
    fn id(&self) -> &'static str {
        "agents"
    }

    fn title(&self) -> &'static str {
        "Agents"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            let paused = self.pause.is_paused();
            vec![
                Row::new(Self::PAUSE, "Pause agents", Control::Toggle(paused))
                    .detail(if paused {
                        "Paused · leases held, publishes rejected"
                    } else {
                        "Running"
                    })
                    .keywords(&["safe mode", "resume", "suspend", "stop", "override"])
                    .caution(paused),
            ]
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match (row, edit) {
                (Self::PAUSE, Edit::Set(paused)) => {
                    self.pause.set(paused).await;
                    Ok(())
                }
                _ => Err(format!("{row} does not support {edit:?}")),
            }
        })
    }
}
