//! # Hit-Test Performance Benchmark — [rig-xlr9]
//!
//! Measures `SceneGraph::hit_test` latency against the v1 spec requirement:
//!
//! > Hit-testing MUST complete in < 100µs for a single point query against
//! > 50 tiles, measured as pure Rust execution with no GPU involvement.
//! > Source: scene-graph/spec.md line 267, RFC 0001 §10.
//!
//! ## Benchmark groups
//!
//! - `hit_test/50_tiles_worst_case` — 50 tiles, all non-passthrough with
//!   HitRegionNodes.  Point is in the top tile (requires traversing the full
//!   z-order list).  Representative of worst-case production load.
//!
//! - `hit_test/50_tiles_miss` — 50 tiles, point misses all bounds.  Tests
//!   the fast-reject path.
//!
//! - `hit_test/50_tiles_passthrough` — 50 passthrough tiles.  Tests the
//!   passthrough-skip loop.
//!
//! ## Interpretation
//!
//! Criterion reports median and p99 wall-clock time.  The < 100µs requirement
//! applies to single-core 3GHz-equivalent reference hardware.  On faster
//! machines the threshold will be proportionally lower; on slower machines it
//! may exceed 100µs and the test will still pass (CI is not reference hardware).
//!
//! The benchmark is informational — it does not fail the build if it exceeds
//! the threshold.  Use it to detect regressions and to validate optimisations.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use tze_hud_scene::{HitRegionNode, InputMode, Node, NodeData, Rect, SceneId, graph::SceneGraph};

// ─── Scene builders ───────────────────────────────────────────────────────────

/// Build a scene with `tile_count` tiles, each with a HitRegionNode filling
/// the tile bounds.  Tiles are laid out in a grid (10 columns).
///
/// `point_in_top_tile` — the display coordinate used for the "hit" benchmark.
fn build_scene_grid(tile_count: usize) -> (SceneGraph, f32, f32) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Bench", 0).unwrap();
    let lease_id = scene.grant_lease("agent.bench", 86_400_000);

    let cols = 10usize;
    let tile_w = 192.0f32;
    let tile_h = 108.0f32;

    let mut top_tile_center = (tile_w / 2.0, tile_h / 2.0);

    for i in 0..tile_count {
        let col = i % cols;
        let row = i / cols;
        let x = col as f32 * tile_w;
        let y = row as f32 * tile_h;
        let bounds = Rect::new(x, y, tile_w, tile_h);
        let z_order = (i + 1) as u32;

        let tile_id = scene
            .create_tile(tab_id, "agent.bench", lease_id, bounds, z_order)
            .unwrap();

        let node_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
                        interaction_id: format!("tile-{i}"),
                        accepts_focus: false,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        // The last tile (highest z-order) is the front-most.
        if i == tile_count - 1 {
            top_tile_center = (x + tile_w / 2.0, y + tile_h / 2.0);
        }
    }

    (scene, top_tile_center.0, top_tile_center.1)
}

/// Build a scene with `tile_count` passthrough tiles.
fn build_passthrough_scene(tile_count: usize) -> SceneGraph {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Bench", 0).unwrap();
    let lease_id = scene.grant_lease("agent.bench", 86_400_000);

    for i in 0..tile_count {
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent.bench",
                lease_id,
                Rect::new(0.0, 0.0, 1920.0, 1080.0),
                (i + 1) as u32,
            )
            .unwrap();
        scene.tiles.get_mut(&tile_id).unwrap().input_mode = InputMode::Passthrough;
    }
    scene
}

// ─── Benchmarks ───────────────────────────────────────────────────────────────

fn bench_hit_test(c: &mut Criterion) {
    let mut group = c.benchmark_group("hit_test");

    // ── 50-tile worst case (point hits the top tile, full traversal) ──────
    let (scene_50, px, py) = build_scene_grid(50);
    group.bench_with_input(
        BenchmarkId::new("50_tiles_worst_case", ""),
        &(scene_50, px, py),
        |b, (scene, x, y)| {
            b.iter(|| black_box(scene.hit_test(black_box(*x), black_box(*y))));
        },
    );

    // ── 50-tile miss (point outside all tile bounds) ───────────────────────
    let (scene_miss, _, _) = build_scene_grid(50);
    group.bench_with_input(
        BenchmarkId::new("50_tiles_miss", ""),
        &scene_miss,
        |b, scene| {
            b.iter(|| {
                // Use a point clearly outside the grid (bottom-right corner).
                black_box(scene.hit_test(black_box(1900.0), black_box(1050.0)))
            });
        },
    );

    // ── 50-tile all-passthrough ────────────────────────────────────────────
    let passthrough_scene = build_passthrough_scene(50);
    group.bench_with_input(
        BenchmarkId::new("50_tiles_passthrough", ""),
        &passthrough_scene,
        |b, scene| {
            b.iter(|| black_box(scene.hit_test(black_box(960.0), black_box(540.0))));
        },
    );

    group.finish();
}

criterion_group!(benches, bench_hit_test);
criterion_main!(benches);
