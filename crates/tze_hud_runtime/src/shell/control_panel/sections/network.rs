//! Network: the MCP and gRPC ports (saved now, applied by a restart) and
//! whether Windows Firewall lets tailnet agents in (changed now, through the
//! elevated helper).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tze_hud_compositor::{PanelBadge, PanelTone};
use tze_hud_config::RuntimeUpdate;

use super::super::row::{BoxFuture, Control, Edit, Row, Section};
use super::super::runtime::{Ports, RuntimeControl};
use crate::firewall::TailnetInbound;
use crate::firewall::remote::RemoteAction;
use crate::launch::Source;

pub(crate) struct NetworkSection {
    control: RuntimeControl,
    /// The last firewall verdict (`None` until the first check finishes).
    verdict: Arc<Mutex<Option<TailnetInbound>>>,
    probing: Arc<AtomicBool>,
    /// When the firewall was last read for the panel.
    checked: Mutex<Option<std::time::Instant>>,
}

impl NetworkSection {
    pub(crate) const MCP_PORT: &'static str = "network.mcp_port";
    pub(crate) const GRPC_PORT: &'static str = "network.grpc_port";
    pub(crate) const REMOTE: &'static str = "network.remote";

    pub(crate) fn new(control: RuntimeControl) -> Self {
        Self {
            control,
            verdict: Arc::default(),
            probing: Arc::default(),
            checked: Mutex::default(),
        }
    }

    fn port_row(&self, id: &str, label: &str, saved: u16, running: u16, source: Source) -> Row {
        let keywords: &'static [&'static str] = &["port", "listen", "address", "restart"];
        if source.is_pinned() || self.control.config_path.is_none() {
            let why = if source.is_pinned() {
                source.describe()
            } else {
                "No config file to save to".to_owned()
            };
            return Row::new(
                id,
                label,
                Control::ReadOnly {
                    text: running.to_string(),
                    mono: true,
                },
            )
            .detail(why)
            .keywords(keywords);
        }
        let pending = saved != running;
        let detail = match (pending, id == Self::MCP_PORT) {
            (true, true) => format!(
                "Saved to config · running {running} · paired agents must switch after the restart"
            ),
            (true, false) => format!("Saved to config · running {running}"),
            (false, _) => source.describe(),
        };
        Row::new(
            id,
            label,
            Control::Field {
                text: saved.to_string(),
            },
        )
        .detail(detail)
        .keywords(keywords)
        .badge(pending.then(|| PanelBadge {
            text: "Restart to apply".into(),
            tone: PanelTone::Caution,
        }))
    }

    fn remote_row(&self) -> Row {
        let verdict = self
            .verdict
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let keywords: &'static [&'static str] =
            &["firewall", "tailnet", "tailscale", "remote", "allow-remote"];
        let read_only = |text: &str, detail: &str| {
            Row::new(
                Self::REMOTE,
                "Remote agents",
                Control::ReadOnly {
                    text: text.to_owned(),
                    mono: false,
                },
            )
            .detail(detail)
            .keywords(keywords)
        };
        let (on, detail) = match verdict {
            None => return read_only("Checking…", "Reading Windows Firewall"),
            Some(TailnetInbound::NotApplicable) => {
                return read_only("This PC only", "Not listening on a tailnet address");
            }
            Some(TailnetInbound::Allowed) => {
                (true, "Windows Firewall lets tailnet agents in".to_owned())
            }
            Some(TailnetInbound::Blocked { .. }) => {
                (false, "Windows Firewall blocks tailnet agents".to_owned())
            }
            Some(TailnetInbound::Unknown { error }) => {
                (false, format!("Firewall state unknown: {error}"))
            }
        };
        Row::new(
            Self::REMOTE,
            "Remote agents",
            Control::Grant {
                on,
                confirm: "Let tailnet agents through Windows Firewall? Windows asks for \
                          administrator approval."
                    .to_owned(),
            },
        )
        .detail(detail)
        .keywords(keywords)
    }

    /// Read the firewall in the background unless the panel read it within
    /// [`super::RECHECK`] or a fresh answer is cached.
    fn check_firewall(&self) {
        let Some(probe) = self.control.firewall.clone() else {
            return;
        };
        if !super::due(&self.checked) {
            return;
        }
        if let Some(verdict) = probe.cached() {
            *self.verdict.lock().unwrap_or_else(|e| e.into_inner()) = Some(verdict);
            return;
        }
        if self.probing.swap(true, Ordering::AcqRel) {
            return;
        }
        let (verdict, probing) = (Arc::clone(&self.verdict), Arc::clone(&self.probing));
        tokio::task::spawn_blocking(move || {
            let found = probe.current();
            *verdict.lock().unwrap_or_else(|e| e.into_inner()) = Some(found);
            probing.store(false, Ordering::Release);
        });
    }

    fn set_port(&self, row: &str, text: &str) -> Result<(), String> {
        let port: u16 = text
            .trim()
            .parse()
            .map_err(|_| "Type a port from 0 to 65535 (0 turns it off)".to_owned())?;
        let mut ports = self.control.saved();
        let update = if row == Self::MCP_PORT {
            ports.mcp = port;
            RuntimeUpdate::McpPort(Some(port))
        } else {
            ports.grpc = port;
            RuntimeUpdate::GrpcPort(Some(port))
        };
        if port != 0 && ports.mcp == ports.grpc {
            return Err("MCP and gRPC need different ports".to_owned());
        }
        let is_mcp = row == Self::MCP_PORT;
        self.control.save(update, |s| {
            if is_mcp {
                &mut s.mcp_port
            } else {
                &mut s.grpc_port
            }
        })?;
        *self.control.saved.lock().unwrap_or_else(|e| e.into_inner()) = ports;
        Ok(())
    }

    async fn set_remote(&self, on: bool) -> Result<(), String> {
        let action = if on {
            RemoteAction::Allow
        } else {
            RemoteAction::Disallow
        };
        let Ports { mcp, grpc } = self.control.running;
        let remote = Arc::clone(&self.control.remote);
        let result = tokio::task::spawn_blocking(move || remote(action, [mcp, grpc]))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        if let Some(probe) = &self.control.firewall {
            probe.forget();
        }
        *self.verdict.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.checked.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.check_firewall();
        result.map(|outcome| tracing::info!(?action, %outcome, "control panel: firewall changed"))
    }
}

impl Section for NetworkSection {
    fn id(&self) -> &'static str {
        "network"
    }

    fn title(&self) -> &'static str {
        "Network"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            let (saved, running, sources) = (
                self.control.saved(),
                self.control.running,
                self.control.sources(),
            );
            let mut rows = vec![
                self.port_row(
                    Self::MCP_PORT,
                    "MCP port",
                    saved.mcp,
                    running.mcp,
                    sources.mcp_port,
                ),
                self.port_row(
                    Self::GRPC_PORT,
                    "gRPC port",
                    saved.grpc,
                    running.grpc,
                    sources.grpc_port,
                ),
            ];
            if self.control.firewall.is_some() {
                rows.push(self.remote_row());
            }
            rows
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match (row, edit) {
                (Self::MCP_PORT | Self::GRPC_PORT, Edit::Text(text)) => self.set_port(row, &text),
                // Enter without typing: nothing to save.
                (Self::MCP_PORT | Self::GRPC_PORT, Edit::Activate) => Ok(()),
                (Self::REMOTE, Edit::Set(on)) => self.set_remote(on).await,
                (_, edit) => Err(format!("{row} does not support {edit:?}")),
            }
        })
    }

    fn opened(&self) {
        *self.checked.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn refresh(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move { self.check_firewall() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firewall::FirewallProbe;
    use crate::shell::control_panel::runtime::tests::fake;

    fn row<'a>(rows: &'a [Row], id: &str) -> &'a Row {
        rows.iter().find(|r| r.id == id).unwrap()
    }

    /// Remote agents shows the firewall verdict, and switching it runs the
    /// elevated helper for the running ports, then reads the firewall again.
    #[tokio::test]
    async fn remote_agents_follows_the_firewall_and_runs_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let effects = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(AtomicBool::new(false));
        let verdict = Arc::clone(&allowed);
        let probe = Arc::new(FirewallProbe::new(move || {
            if verdict.load(Ordering::SeqCst) {
                TailnetInbound::Allowed
            } else {
                TailnetInbound::Blocked {
                    reason: crate::firewall::BlockReason::NoAllowRule,
                    rule: None,
                }
            }
        }));
        let control = fake(
            &dir.path().join("c.toml"),
            Arc::clone(&effects),
            Some(probe),
        );
        let section = NetworkSection::new(control);
        let remote = |rows: Vec<Row>| row(&rows, NetworkSection::REMOTE).clone();
        assert_eq!(
            remote(section.rows().await).control,
            Control::ReadOnly {
                text: "Checking…".into(),
                mono: false
            }
        );
        let checked = || async {
            for _ in 0..200 {
                section.refresh().await;
                let r = remote(section.rows().await);
                if matches!(r.control, Control::Grant { .. }) {
                    return r;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("the firewall check never finished")
        };
        let blocked = checked().await;
        assert!(matches!(blocked.control, Control::Grant { on: false, .. }));
        assert_eq!(
            blocked.detail.as_deref(),
            Some("Windows Firewall blocks tailnet agents")
        );

        allowed.store(true, Ordering::SeqCst);
        section
            .apply(NetworkSection::REMOTE, Edit::Set(true))
            .await
            .unwrap();
        assert_eq!(*effects.lock().unwrap(), ["Allow [9090, 50051]"]);
        let after = checked().await;
        assert!(
            matches!(after.control, Control::Grant { on: true, .. }),
            "re-read, not cached"
        );
    }

    /// A port is saved typed, refused when it is not a port or equals the
    /// other one, and a pinned port is read-only naming its flag.
    #[tokio::test]
    async fn ports_save_validate_and_respect_pins() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("c.toml");
        let control = fake(&config, Arc::default(), None);
        control.sources.lock().unwrap().mcp_port = Source::Flag("--mcp-port");
        let section = NetworkSection::new(control.clone());

        let rows = section.rows().await;
        let mcp = row(&rows, NetworkSection::MCP_PORT);
        assert!(matches!(mcp.control, Control::ReadOnly { .. }));
        assert_eq!(mcp.detail.as_deref(), Some("Set by --mcp-port"));
        assert_eq!(row(&rows, NetworkSection::GRPC_PORT).badge, None);

        for (text, why) in [("x", "Type a port"), ("9090", "different ports")] {
            let err = section
                .apply(NetworkSection::GRPC_PORT, Edit::Text(text.into()))
                .await
                .unwrap_err();
            assert!(err.contains(why), "{text}: {err}");
        }
        section
            .apply(NetworkSection::GRPC_PORT, Edit::Text(" 50061 ".into()))
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains("grpc_port = 50061")
        );
        assert_eq!(control.pending(), ["gRPC port"]);
        let rows = section.rows().await;
        let grpc = row(&rows, NetworkSection::GRPC_PORT);
        assert_eq!(
            grpc.control,
            Control::Field {
                text: "50061".into()
            }
        );
        assert_eq!(
            grpc.detail.as_deref(),
            Some("Saved to config · running 50051")
        );
        assert_eq!(grpc.badge.as_ref().unwrap().text, "Restart to apply");

        // Back to the running port: nothing pending.
        section
            .apply(NetworkSection::GRPC_PORT, Edit::Text("50051".into()))
            .await
            .unwrap();
        assert!(control.pending().is_empty());
        assert_eq!(
            row(&section.rows().await, NetworkSection::GRPC_PORT)
                .detail
                .as_deref(),
            Some("Saved to config")
        );
    }
}
