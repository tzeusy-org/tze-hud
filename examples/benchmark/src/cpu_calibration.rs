//! CPU scene-graph calibration for the benchmark's hardware-factor vector.
//!
//! Runs a fixed scene-mutation workload plus a synthetic hash loop and reports
//! how much slower this machine is than the reference. The Windows and
//! constrained-envelope budget gates divide raw observations by this factor.

// Only the headless benchmark runs the workload; the result type is always used.
#![cfg_attr(not(feature = "headless"), allow(dead_code))]

use serde::{Deserialize, Serialize};
use std::time::Instant;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::mutation::{MutationBatch, SceneMutation};
use tze_hud_scene::types::{
    FontFamily, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode, TextAlign, TextMarkdownNode,
    TextOverflow,
};

/// Result of the CPU calibration workload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CpuCalibrationResult {
    /// How many times slower this machine is than the reference (1.0 = reference).
    pub speed_factor: f64,
    /// Scene-graph operations per second achieved.
    pub scene_ops_per_sec: f64,
    /// Synthetic hash throughput in MB/s.
    pub hash_throughput_mbps: f64,
    /// Unix timestamp (seconds) when calibration ran.
    pub timestamp: u64,
    /// Duration of the calibration run in microseconds.
    pub calibration_duration_us: u64,
}

// ─── Reference baselines ─────────────────────────────────────────────────────

/// Reference baseline: scene-graph ops/sec on target hardware.
///
/// This was measured on a reference machine (modern x86-64, ~4 GHz, no
/// software rendering overhead). The calibration workload produces this
/// many operations per second on that machine.
///
/// The workload: create a 50-tile scene, then apply 100 mutation batches
/// (each with 5 mutations: 2 UpdateTileBounds + 2 SetTileRoot + 1 DeleteTile/CreateTile).
/// Total: ~550 scene-graph operations.
const REFERENCE_SCENE_OPS_PER_SEC: f64 = 550_000.0;

/// Reference baseline: synthetic hash throughput in MB/s.
const REFERENCE_HASH_THROUGHPUT_MBPS: f64 = 800.0;

/// Number of tiles to create in the reference scene.
const CALIBRATION_TILES: usize = 50;

/// Number of mutation batches to apply.
const CALIBRATION_BATCHES: usize = 100;

/// Mutations per batch in the calibration workload.
const MUTATIONS_PER_BATCH: usize = 5;

/// Minimum speed factor (prevents unreasonably tight budgets).
const MIN_SPEED_FACTOR: f64 = 0.5;

/// Maximum speed factor (prevents unreasonably loose budgets on very slow machines).
const MAX_SPEED_FACTOR: f64 = 50.0;

/// Run the CPU calibration workload and compute the speed factor.
pub fn calibrate() -> CpuCalibrationResult {
    let overall_start = Instant::now();
    let scene_ops = run_scene_workload();
    let hash_mbps = run_hash_workload();

    // Scene ops dominate (80%) since that's what the budgets protect; hash
    // throughput (20%) catches general CPU speed differences.
    let scene_factor = REFERENCE_SCENE_OPS_PER_SEC / scene_ops.max(1.0);
    let hash_factor = REFERENCE_HASH_THROUGHPUT_MBPS / hash_mbps.max(0.01);
    let raw_factor = scene_factor * 0.8 + hash_factor * 0.2;
    let speed_factor = raw_factor.clamp(MIN_SPEED_FACTOR, MAX_SPEED_FACTOR);

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    CpuCalibrationResult {
        speed_factor,
        scene_ops_per_sec: scene_ops,
        hash_throughput_mbps: hash_mbps,
        timestamp: now_secs,
        calibration_duration_us: overall_start.elapsed().as_micros() as u64,
    }
}

/// Run the scene-graph mutation workload and return ops/sec.
fn run_scene_workload() -> f64 {
    let start = Instant::now();
    let mut total_ops: u64 = 0;

    // Create a scene with CALIBRATION_TILES tiles
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("calibration", 0).expect("create_tab");
    total_ops += 1;

    // Grant a lease with a high budget for the calibration workload
    let lease_id = scene.grant_lease("calibration", 300_000);
    // Override the budget to allow many tiles
    if let Some(lease) = scene.leases.get_mut(&lease_id) {
        lease.resource_budget.max_tiles = (CALIBRATION_TILES + 10) as u32;
    }
    total_ops += 1;

    // Create tiles
    let mut tile_ids = Vec::with_capacity(CALIBRATION_TILES);
    let cols = 10u32;
    for i in 0..CALIBRATION_TILES {
        let col = (i as u32) % cols;
        let row = (i as u32) / cols;
        let bounds = Rect::new(col as f32 * 190.0, row as f32 * 180.0, 180.0, 170.0);
        let tile_id = scene
            .create_tile(tab_id, "calibration", lease_id, bounds, (i + 1) as u32)
            .expect("create_tile during calibration");

        // Set a root node
        let node = if i % 2 == 0 {
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.5, 0.5, 0.5, 1.0),
                    bounds: Rect::new(0.0, 0.0, 180.0, 170.0),
                    radius: None,
                }),
            }
        } else {
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: format!("calibration tile {i}"),
                    bounds: Rect::new(0.0, 0.0, 180.0, 170.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemMonospace,
                    color: Rgba::WHITE,
                    background: None,
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Clip,
                    color_runs: Box::default(),
                }),
            }
        };
        scene
            .set_tile_root(tile_id, node)
            .expect("set_tile_root during calibration");
        tile_ids.push(tile_id);
        total_ops += 2; // create_tile + set_tile_root
    }

    // Apply CALIBRATION_BATCHES mutation batches
    for batch_idx in 0..CALIBRATION_BATCHES {
        let mut mutations = Vec::with_capacity(MUTATIONS_PER_BATCH);

        // Pick tiles to mutate (cycling through available tiles)
        let base = batch_idx % tile_ids.len();

        // Mutation 1-2: UpdateTileBounds on two tiles
        for offset in 0..2 {
            let idx = (base + offset) % tile_ids.len();
            let jitter = (batch_idx as f32) * 0.1;
            mutations.push(SceneMutation::UpdateTileBounds {
                tile_id: tile_ids[idx],
                bounds: Rect::new(
                    (idx as u32 % cols) as f32 * 190.0 + jitter,
                    (idx as u32 / cols) as f32 * 180.0,
                    180.0,
                    170.0,
                ),
            });
        }

        // Mutation 3-4: SetTileRoot on two tiles (replace node tree)
        for offset in 2..4 {
            let idx = (base + offset) % tile_ids.len();
            let node = Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(batch_idx as f32 / CALIBRATION_BATCHES as f32, 0.5, 0.5, 1.0),
                    bounds: Rect::new(0.0, 0.0, 180.0, 170.0),
                    radius: None,
                }),
            };
            mutations.push(SceneMutation::SetTileRoot {
                tile_id: tile_ids[idx],
                node,
                descendants: vec![],
            });
        }

        // Mutation 5: Delete a tile and recreate it (exercises full lifecycle)
        let recycle_idx = (base + 4) % tile_ids.len();
        mutations.push(SceneMutation::DeleteTile {
            tile_id: tile_ids[recycle_idx],
        });

        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "calibration".to_string(),
            mutations,
            timing_hints: None,
            lease_id: None,
        };

        let result = scene.apply_batch(&batch);
        total_ops += MUTATIONS_PER_BATCH as u64;

        // Recreate the deleted tile
        if result.applied {
            let col = (recycle_idx as u32) % cols;
            let row = (recycle_idx as u32) / cols;
            let bounds = Rect::new(col as f32 * 190.0, row as f32 * 180.0, 180.0, 170.0);
            if let Ok(new_tile_id) = scene.create_tile(
                tab_id,
                "calibration",
                lease_id,
                bounds,
                (recycle_idx + 1) as u32,
            ) {
                let node = Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(0.3, 0.3, 0.3, 1.0),
                        bounds: Rect::new(0.0, 0.0, 180.0, 170.0),
                        radius: None,
                    }),
                };
                let _ = scene.set_tile_root(new_tile_id, node);
                tile_ids[recycle_idx] = new_tile_id;
                total_ops += 2;
            }
        }
    }

    // Also exercise hit_test — it's on the hot path
    for i in 0..100 {
        let x = (i as f32 * 19.2) % 1920.0;
        let y = (i as f32 * 10.8) % 1080.0;
        let _ = scene.hit_test(x, y);
        total_ops += 1;
    }

    let elapsed_secs = start.elapsed().as_secs_f64();
    total_ops as f64 / elapsed_secs.max(1e-9)
}

/// Run a synthetic CPU load workload and return throughput in MB/s.
///
/// Uses a simple iterative hash (FNV-1a inspired) to measure raw CPU
/// throughput without depending on external crates.
fn run_hash_workload() -> f64 {
    let start = Instant::now();
    let iterations = 100_000;
    let block_size = 64; // bytes per iteration
    let total_bytes = iterations * block_size;

    let mut state: u64 = 0xcbf29ce484222325; // FNV offset basis
    for i in 0..iterations {
        // FNV-1a style mixing
        for byte in 0..block_size {
            state ^= ((i * block_size + byte) & 0xFF) as u64;
            state = state.wrapping_mul(0x100000001b3); // FNV prime
        }
    }

    // Prevent the compiler from optimizing away the computation
    std::hint::black_box(state);

    let elapsed_secs = start.elapsed().as_secs_f64();
    let total_mb = total_bytes as f64 / (1024.0 * 1024.0);
    total_mb / elapsed_secs.max(1e-9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibrate_reports_a_clamped_positive_speed_factor() {
        let result = calibrate();
        assert!((MIN_SPEED_FACTOR..=MAX_SPEED_FACTOR).contains(&result.speed_factor));
        assert!(result.scene_ops_per_sec > 0.0);
        assert!(result.hash_throughput_mbps > 0.0);
    }
}
