//! `poc_demo`: drive a live tze_hud through zones, widgets, a resident tile and
//! a hung-agent override. See the crate docs and the README "Demo" section.

use std::time::Duration;

use poc_demo::{PORTAL_INSTRUCTIONS, Result, Target};

const USAGE: &str = "poc_demo: drive a live tze_hud (MCP and gRPC) through the POC stages

USAGE: poc_demo <STAGE> [OPTIONS]

STAGES:
  zones           notification TTL, delay_ms content, notification action -> hud_input
  widgets         typed parameter updates on the gauge and progress widgets
  tile            resident gRPC tile: ClaimTile, MutationBatch, Hold, Reclaimed
  override-hang   claim a tile, then stop reading its stream
  all             zones, widgets, tile, the portal instructions, override-hang

OPTIONS:
  --mcp <host:port>       MCP listener            [127.0.0.1:9090]
  --grpc <host:port>      gRPC listener           [127.0.0.1:50051]
  --agent <id>            paired agent id of the tile PSK [claude]
  --pace-ms <n>           pause between steps     [1500]
  --human-wait-s <n>      wait for a human action [10]
  -h, --help              this text

CREDENTIALS (never printed):
  TZE_HUD_PSK or TZE_HUD_PSK_FILE            MCP agent PSK
  TZE_HUD_TILE_PSK or TZE_HUD_TILE_PSK_FILE  resident tile PSK (default: the MCP PSK)
A file holds the PSK, or the JSON reply of POST /pair (its \"psk\" field).";

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut stage = None;
    let (mut mcp, mut grpc) = ("127.0.0.1:9090".to_string(), "127.0.0.1:50051".to_string());
    let mut tile_agent = "claude".to_string();
    let (mut pace_ms, mut human_wait_s) = (1_500, 10);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--mcp" => mcp = value("--mcp")?,
            "--grpc" => grpc = value("--grpc")?,
            "--agent" => tile_agent = value("--agent")?,
            "--pace-ms" => pace_ms = value("--pace-ms")?.parse()?,
            "--human-wait-s" => human_wait_s = value("--human-wait-s")?.parse()?,
            s if !s.starts_with('-') && stage.is_none() => stage = Some(arg),
            _ => return Err(format!("unknown argument {arg}\n\n{USAGE}").into()),
        }
    }
    let stage = stage.ok_or(format!("missing stage\n\n{USAGE}"))?;
    if !["zones", "widgets", "tile", "override-hang", "all"].contains(&stage.as_str()) {
        return Err(format!("unknown stage {stage}\n\n{USAGE}").into());
    }

    let mcp_psk = credential("TZE_HUD_PSK")?;
    let target = Target {
        mcp,
        grpc,
        tile_agent,
        tile_psk: if ["TZE_HUD_TILE_PSK", "TZE_HUD_TILE_PSK_FILE"]
            .iter()
            .any(|var| std::env::var_os(var).is_some())
        {
            credential("TZE_HUD_TILE_PSK")?
        } else {
            mcp_psk.clone()
        },
        mcp_psk,
        pace: Duration::from_millis(pace_ms),
        human_wait: Duration::from_secs(human_wait_s),
        held: None,
    };
    match stage.as_str() {
        "zones" => poc_demo::zones(&target).await,
        "widgets" => poc_demo::widgets(&target).await,
        "tile" => poc_demo::tile(&target).await,
        "override-hang" => poc_demo::override_hang(&target).await,
        _ => {
            poc_demo::zones(&target).await?;
            poc_demo::widgets(&target).await?;
            poc_demo::tile(&target).await?;
            println!("\n{PORTAL_INSTRUCTIONS}");
            poc_demo::override_hang(&target).await
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
