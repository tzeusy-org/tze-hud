//! `poc_demo`: drive a live tze_hud through zones and widgets. See the crate docs and the README "Demo" section.

use std::time::Duration;

use poc_demo::{PORTAL_INSTRUCTIONS, Result, Target};

const USAGE: &str = "poc_demo: drive a live tze_hud (MCP) through the POC stages

USAGE: poc_demo <STAGE> [OPTIONS]

STAGES:
  zones           notification TTL, delay_ms content, notification action -> hud_input
  widgets         typed parameter updates on the gauge and progress widgets
  all             zones, widgets and portal instructions

OPTIONS:
  --mcp <host:port>       MCP listener            [127.0.0.1:9090]
  --pace-ms <n>           pause between steps     [1500]
  --human-wait-s <n>      wait for a human action [10]
  -h, --help              this text

CREDENTIALS (never printed):
  TZE_HUD_PSK or TZE_HUD_PSK_FILE            MCP agent PSK
A file holds the PSK, or the JSON reply of POST /pair (its \"psk\" field).";

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut stage = None;
    let mut mcp = "127.0.0.1:9090".to_string();
    let (mut pace_ms, mut human_wait_s) = (1_500, 10);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--mcp" => mcp = value("--mcp")?,
            "--pace-ms" => pace_ms = value("--pace-ms")?.parse()?,
            "--human-wait-s" => human_wait_s = value("--human-wait-s")?.parse()?,
            s if !s.starts_with('-') && stage.is_none() => stage = Some(arg),
            _ => return Err(format!("unknown argument {arg}\n\n{USAGE}").into()),
        }
    }
    let stage = stage.ok_or(format!("missing stage\n\n{USAGE}"))?;
    if !["zones", "widgets", "all"].contains(&stage.as_str()) {
        return Err(format!("unknown stage {stage}\n\n{USAGE}").into());
    }

    let mcp_psk = credential("TZE_HUD_PSK")?;
    let target = Target {
        mcp,
        mcp_psk,
        pace: Duration::from_millis(pace_ms),
        human_wait: Duration::from_secs(human_wait_s),
    };
    match stage.as_str() {
        "zones" => poc_demo::zones(&target).await,
        "widgets" => poc_demo::widgets(&target).await,
        _ => {
            poc_demo::zones(&target).await?;
            poc_demo::widgets(&target).await?;
            println!("\n{PORTAL_INSTRUCTIONS}");
            Ok(())
        }
    }
}

/// A PSK from `<var>` or the file named by `<var>_FILE`.
fn credential(var: &str) -> Result<String> {
    if let Ok(psk) = std::env::var(var) {
        return poc_demo::psk_from_file_text(&psk).map_err(|e| format!("{var}: {e}").into());
    }
    let file = std::env::var(format!("{var}_FILE"))
        .map_err(|_| format!("set {var} or {var}_FILE (see --help)"))?;
    let text = std::fs::read_to_string(&file).map_err(|e| format!("read {var}_FILE: {e}"))?;
    poc_demo::psk_from_file_text(&text).map_err(|e| format!("{var}_FILE: {e}").into())
}
