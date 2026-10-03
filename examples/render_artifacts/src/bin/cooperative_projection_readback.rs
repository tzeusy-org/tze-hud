//! Runtime-native readback artifact for cooperative HUD projection proof.
//!
//! This binary renders a fixed cooperative projection scene through
//! `HeadlessRuntime`, reads back the compositor frame pixels, and writes a
//! portable pixmap plus JSON metadata. It is intentionally narrow evidence
//! tooling for cases where OS desktop capture cannot observe the HUD overlay.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use tze_hud_runtime::headless::{HeadlessConfig, HeadlessRuntime};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    FontFamily, Node, NodeData, Rect, Rgba, SceneId, TextAlign, TextMarkdownNode, TextOverflow,
};

const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const TILE_X: u32 = 64;
const TILE_Y: u32 = 120;
const TILE_WIDTH: u32 = 720;
const TILE_HEIGHT: u32 = 360;
const SAMPLE_MARGIN: u32 = 10;

#[derive(Debug)]
struct Args {
    output_dir: PathBuf,
    width: u32,
    height: u32,
}

fn parse_args() -> Args {
    let mut output_dir = PathBuf::from("test_results/cooperative_projection_readback");
    let mut width = DEFAULT_WIDTH;
    let mut height = DEFAULT_HEIGHT;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--output" | "-o" => {
                i += 1;
                output_dir = PathBuf::from(args.get(i).unwrap_or_else(|| usage_and_exit()));
            }
            "--width" => {
                i += 1;
                width = args
                    .get(i)
                    .unwrap_or_else(|| usage_and_exit())
                    .parse()
                    .unwrap_or_else(|_| usage_and_exit());
            }
            "--height" => {
                i += 1;
                height = args
                    .get(i)
                    .unwrap_or_else(|| usage_and_exit())
                    .parse()
                    .unwrap_or_else(|_| usage_and_exit());
            }
            "--help" | "-h" => usage_and_exit(),
            _ => usage_and_exit(),
        }
        i += 1;
    }

    Args {
        output_dir,
        width,
        height,
    }
}

fn usage_and_exit() -> ! {
    eprintln!("Usage: cooperative-projection-readback [--output DIR] [--width PX] [--height PX]");
    std::process::exit(2);
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();
    validate_args(&args)?;
    fs::create_dir_all(&args.output_dir)?;

    let mut runtime = HeadlessRuntime::new(HeadlessConfig {
        width: args.width,
        height: args.height,
        grpc_port: 0,
        agents: Default::default(),
        config_toml: Some(readback_config_toml()),
    })
    .await?;

    let scene = build_projection_scene(args.width as f32, args.height as f32)?;
    {
        let state = runtime.shared_state().lock().await;
        *state.scene.lock().await = scene;
    }

    let telemetry = runtime.render_frame().await;
    let pixels = runtime.read_pixels();

    let ppm_path = args.output_dir.join("cooperative-projection-readback.ppm");
    write_ppm(&ppm_path, args.width, args.height, &pixels)?;

    let samples = sample_points(args.width, args.height, &pixels);
    let metadata = serde_json::json!({
        "artifact": "cooperative-projection-readback",
        "description": "Runtime-native headless compositor pixel readback for cooperative HUD projection proof",
        "width": args.width,
        "height": args.height,
        "pixel_format": "RGBA8 readback, PPM artifact stores RGB",
        "frame_telemetry": {
            "frame_number": telemetry.frame_number,
            "frame_time_us": telemetry.frame_time_us,
            "stage6_render_encode_us": telemetry.stage6_render_encode_us,
            "stage7_gpu_submit_us": telemetry.stage7_gpu_submit_us,
            "tile_count": telemetry.tile_count,
            "node_count": telemetry.node_count,
            "active_leases": telemetry.active_leases,
        },
        "sampled_pixels": samples,
        "expected": {
            "projection_tile": format!(
                "large dark text tile at x={TILE_X} y={TILE_Y} width={TILE_WIDTH} height={TILE_HEIGHT}"
            ),
            "background": "runtime clear color outside tile"
        },
        "files": {
            "ppm": "cooperative-projection-readback.ppm"
        }
    });
    fs::write(
        args.output_dir.join("cooperative-projection-readback.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )?;

    println!("{}", serde_json::to_string_pretty(&metadata)?);
    Ok(())
}

fn validate_args(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let min_width = TILE_X + TILE_WIDTH + SAMPLE_MARGIN;
    let min_height = TILE_Y + TILE_HEIGHT + SAMPLE_MARGIN;
    if args.width < min_width || args.height < min_height {
        return Err(artifact_error(format!(
            "readback dimensions too small: got {}x{}, expected at least {}x{} so tile and sample points fit without clamping",
            args.width, args.height, min_width, min_height
        ))
        .into());
    }
    Ok(())
}

fn readback_config_toml() -> String {
    r#"
[runtime]
profile = "headless"

[[tabs]]
name = "Cooperative Projection Proof"
default_tab = true
"#
    .to_string()
}

fn build_projection_scene(
    width: f32,
    height: f32,
) -> Result<SceneGraph, Box<dyn std::error::Error>> {
    let mut scene = SceneGraph::new(width, height);
    scene.create_tab("Cooperative Projection Proof", 0)?;
    let tab = scene
        .active_tab
        .ok_or_else(|| artifact_error("scene has no active tab after create_tab"))?;
    let lease = scene.grant_lease("agent-alpha", 120_000);
    let tile = scene.create_tile(
        tab,
        "agent-alpha",
        lease,
        Rect::new(
            TILE_X as f32,
            TILE_Y as f32,
            TILE_WIDTH as f32,
            TILE_HEIGHT as f32,
        ),
        160,
    )?;
    scene.set_tile_root(
        tile,
        Node {
            layout: Default::default(),
            id: SceneId::new(),
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: [
                    "**Cooperative HUD Projection Proof**",
                    "`hud-ggntn.12` | Expanded | AttentionLow",
                    "status: runtime-native readback artifact",
                    "note: OS desktop capture was unavailable from SSH",
                    "",
                    "Transcript:",
                    "Agent output is rendered through the governed HUD surface.",
                    "Viewer input remains bounded by lease cleanup and runtime sovereignty.",
                    "",
                    "composer: ready",
                    "pending HUD input: 0",
                ]
                .join("\n"),
                bounds: Rect::new(0.0, 0.0, TILE_WIDTH as f32, TILE_HEIGHT as f32),
                font_size_px: 22.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba::new(0.94, 0.97, 1.0, 1.0),
                background: Some(Rgba::new(0.04, 0.06, 0.10, 0.94)),
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::default(),
            }),
            children: vec![],
        },
    )?;
    Ok(scene)
}

fn write_ppm(
    path: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    if rgba.len() != (width * height * 4) as usize {
        return Err(format!(
            "pixel buffer size mismatch: got {}, expected {}",
            rgba.len(),
            width * height * 4
        )
        .into());
    }

    let file = fs::File::create(path)?;
    let mut writer = BufWriter::new(file);
    write!(writer, "P6\n{width} {height}\n255\n")?;
    for px in rgba.chunks_exact(4) {
        writer.write_all(&px[..3])?;
    }
    writer.flush()?;
    Ok(())
}

fn sample_points(width: u32, height: u32, rgba: &[u8]) -> Vec<serde_json::Value> {
    let points = [
        ("background_top_left", 10, 10),
        (
            "projection_tile_center",
            TILE_X + TILE_WIDTH / 2,
            TILE_Y + TILE_HEIGHT / 2,
        ),
        ("projection_tile_text_region", TILE_X + 32, TILE_Y + 32),
        (
            "background_bottom_right",
            width.saturating_sub(10),
            height.saturating_sub(10),
        ),
    ];
    points
        .into_iter()
        .map(|(name, x, y)| {
            let x = x.min(width.saturating_sub(1));
            let y = y.min(height.saturating_sub(1));
            let offset = ((y * width + x) * 4) as usize;
            let rgba = &rgba[offset..offset + 4];
            serde_json::json!({
                "name": name,
                "x": x,
                "y": y,
                "rgba": rgba,
            })
        })
        .collect()
}

fn artifact_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}
