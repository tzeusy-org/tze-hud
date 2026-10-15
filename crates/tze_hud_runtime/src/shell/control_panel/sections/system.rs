//! System: which build is running.

use super::super::row::{BoxFuture, Control, Edit, Row, Section};
use crate::operator::status::build_label;

pub(crate) struct SystemSection;

impl Section for SystemSection {
    fn id(&self) -> &'static str {
        "system"
    }

    fn title(&self) -> &'static str {
        "System"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            vec![
                Row::new(
                    "system.version",
                    "Version",
                    Control::ReadOnly {
                        text: build_label(),
                        mono: true,
                    },
                )
                .keywords(&["build", "channel", "sha", "release", "update"]),
            ]
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { Err(format!("{row} is read-only ({edit:?})")) })
    }
}
