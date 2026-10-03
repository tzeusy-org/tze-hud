//! # Scene Snapshot Performance Benchmark — [rig-bav0]
//!
//! Measures `SceneGraph::take_snapshot` latency against the v1 spec requirement:
//!
//! > Serialization MUST complete in < 1ms for 100 tiles with 1000 nodes.
//! > Source: scene-graph/spec.md line 285, RFC 0001 §10.
//!
//! ## Benchmark groups
//!
//! - `snapshot/100_tiles_1000_nodes` — primary spec requirement case.
//!   100 tiles (10 nodes each), with zone registry populated.
//!
//! - `snapshot/empty_scene` — baseline with no tiles or nodes.
//!
//! - `snapshot/max_tabs_stress` — 25 test-scene `max_tiles_stress` build.
//!
//! ## Interpretation
//!
//! Criterion reports median and p99 wall-clock time. The < 1ms requirement
//! applies to single-core 3GHz-equivalent reference hardware. On faster
//! machines the threshold will be proportionally lower. The benchmark is
//! informational — it does not fail the build. Use it to detect regressions.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use tze_hud_scene::{
    Node, NodeData, Rect, Rgba, SceneId, SolidColorNode,
    graph::SceneGraph,
    test_scenes::{ClockMs, TestSceneRegistry},
    types::{
        ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetDefinition, WidgetInstance,
        WidgetParamConstraints, WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue,
        WidgetSvgLayer,
    },
};

const WALL_US: u64 = 1_735_689_600_000_000;
const MONO_US: u64 = 12_345_678;

// ─── Scene builders ───────────────────────────────────────────────────────────

/// Build a scene with `tile_count` tiles, each with `nodes_per_tile` SolidColor nodes.
///
/// Node layout: the first node is the tile root; subsequent nodes are added as
/// children of the root via `add_node_to_tile`. This produces a flat (1-level-deep)
/// tree per tile.
fn build_dense_scene(tile_count: usize, nodes_per_tile: usize) -> SceneGraph {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Bench", 0).unwrap();
    let lease_id = scene.grant_lease("agent.bench", 600_000);

    let cols = 10usize;
    for i in 0..tile_count {
        let col = (i % cols) as f32;
        let row = (i / cols) as f32;
        let tile_bounds = Rect::new(col * 192.0, row * 108.0, 190.0, 106.0);

        let tile_id = scene
            .create_tile(tab_id, "agent.bench", lease_id, tile_bounds, i as u32)
            .unwrap();

        let node_size = 190.0 / nodes_per_tile.max(1) as f32;

        // Root node (occupies full tile)
        let root_id = SceneId::new();
        let root_node = Node {
            layout: Default::default(),
            id: root_id,
            children: vec![],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::new(0.5, 0.5, 0.5, 1.0),
                bounds: Rect::new(0.0, 0.0, 190.0, 106.0),
                radius: None,
            }),
        };
        scene.set_tile_root(tile_id, root_node).unwrap();

        // Additional child nodes added to the tile (up to MAX_NODES_PER_TILE)
        let extra_nodes = nodes_per_tile.saturating_sub(1).min(63);
        for j in 0..extra_nodes {
            let child = Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(j as f32 / nodes_per_tile as f32, 0.3, 0.7, 1.0),
                    bounds: Rect::new(j as f32 * node_size, 0.0, node_size - 1.0, 50.0),
                    radius: None,
                }),
            };
            // Add as child of root
            scene
                .add_node_to_tile(tile_id, Some(root_id), child)
                .unwrap();
        }
    }

    scene
}

// ─── Benchmarks ───────────────────────────────────────────────────────────────

fn bench_snapshot_100_tiles_1000_nodes(c: &mut Criterion) {
    // Primary spec requirement: 100 tiles with 1000 nodes total (10 nodes/tile)
    let scene = build_dense_scene(100, 10);
    assert!(scene.tile_count() == 100, "expected 100 tiles");
    assert!(
        scene.node_count() >= 100,
        "expected at least 100 nodes (root nodes)"
    );

    let mut group = c.benchmark_group("snapshot");
    group.bench_function(
        BenchmarkId::new("100_tiles_1000_nodes", "take_snapshot"),
        |b| {
            b.iter(|| black_box(scene.take_snapshot(black_box(WALL_US), black_box(MONO_US))));
        },
    );
    group.finish();
}

fn bench_snapshot_empty_scene(c: &mut Criterion) {
    let scene = SceneGraph::new(1920.0, 1080.0);

    let mut group = c.benchmark_group("snapshot");
    group.bench_function(BenchmarkId::new("empty_scene", "take_snapshot"), |b| {
        b.iter(|| black_box(scene.take_snapshot(black_box(WALL_US), black_box(MONO_US))));
    });
    group.finish();
}

fn bench_snapshot_max_tiles_stress(c: &mut Criterion) {
    let registry = TestSceneRegistry::new();
    let (scene, _) = registry.build("max_tiles_stress", ClockMs::FIXED).unwrap();

    let mut group = c.benchmark_group("snapshot");
    group.bench_function(BenchmarkId::new("max_tiles_stress", "take_snapshot"), |b| {
        b.iter(|| black_box(scene.take_snapshot(black_box(WALL_US), black_box(MONO_US))));
    });
    group.finish();
}

fn bench_snapshot_with_zones(c: &mut Criterion) {
    let registry = TestSceneRegistry::new();
    let (scene, _) = registry
        .build("zone_publish_subtitle", ClockMs::FIXED)
        .unwrap();

    let mut group = c.benchmark_group("snapshot");
    group.bench_function(
        BenchmarkId::new("zone_publish_subtitle", "take_snapshot"),
        |b| {
            b.iter(|| black_box(scene.take_snapshot(black_box(WALL_US), black_box(MONO_US))));
        },
    );
    group.finish();
}

/// Build a scene populated with widget definitions and instances.
///
/// Registers `widget_count` gauge widget instances each with an active publication.
/// Used to benchmark SceneSnapshot < 1ms with widgets (hud-mim2.7 acceptance criterion 10).
fn build_scene_with_widgets(tile_count: usize, widget_count: usize) -> SceneGraph {
    let mut scene = build_dense_scene(tile_count, 1);
    let tab_id = *scene.tabs.keys().next().expect("scene has a tab");

    // Register gauge definition once.
    let def = WidgetDefinition {
        id: "gauge".to_string(),
        name: "gauge".to_string(),
        description: "bench gauge".to_string(),
        parameter_schema: vec![
            WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: Some(WidgetParamConstraints {
                    f32_min: Some(0.0),
                    f32_max: Some(1.0),
                    ..Default::default()
                }),
            },
            WidgetParameterDeclaration {
                name: "label".to_string(),
                param_type: WidgetParamType::String,
                default_value: WidgetParameterValue::String(String::new()),
                constraints: None,
            },
        ],
        layers: vec![WidgetSvgLayer {
            svg_file: "fill.svg".to_string(),
            bindings: vec![],
        }],
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.1,
            height_pct: 0.1,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: false,
        hover_behavior: None,
    };
    scene.widget_registry.register_definition(def);

    // Register `widget_count` instances and publish one record to each.
    for i in 0..widget_count {
        let instance_name = format!("gauge-{i}");
        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: instance_name.clone(),
            current_params: std::collections::HashMap::from([
                (
                    "level".to_string(),
                    WidgetParameterValue::F32(i as f32 / widget_count as f32),
                ),
                (
                    "label".to_string(),
                    WidgetParameterValue::String(format!("Widget {i}")),
                ),
            ]),
        });

        // Directly insert an active publish record so the snapshot includes widget publications.
        let record = tze_hud_scene::types::WidgetPublishRecord {
            lease_id: None,
            widget_name: instance_name.clone(),
            publisher_namespace: "bench.agent".to_string(),
            params: std::collections::HashMap::from([
                (
                    "level".to_string(),
                    WidgetParameterValue::F32(i as f32 / widget_count as f32),
                ),
                (
                    "label".to_string(),
                    WidgetParameterValue::String(format!("W{i}")),
                ),
            ]),
            published_at_wall_us: WALL_US,
            merge_key: None,
            expires_at_wall_us: None,
            transition_ms: 0,
        };
        scene
            .widget_registry
            .active_publishes
            .entry(instance_name)
            .or_default()
            .push(record);
    }

    scene
}

/// Benchmark: SceneSnapshot < 1ms with widget registry populated.
///
/// Spec: hud-mim2.7 acceptance criterion 10 — "SceneSnapshot < 1ms with widgets".
/// This exercises the same < 1ms budget as the zone snapshot benchmark but with
/// 10 widget instances and 10 active publications included in the snapshot payload.
fn bench_snapshot_with_widgets(c: &mut Criterion) {
    // 50 tiles + 10 widget instances with active publications.
    let scene = build_scene_with_widgets(50, 10);

    let mut group = c.benchmark_group("snapshot");
    group.bench_function(
        BenchmarkId::new("50_tiles_10_widgets", "take_snapshot"),
        |b| {
            b.iter(|| black_box(scene.take_snapshot(black_box(WALL_US), black_box(MONO_US))));
        },
    );
    group.finish();
}

criterion_group!(
    benches,
    bench_snapshot_100_tiles_1000_nodes,
    bench_snapshot_empty_scene,
    bench_snapshot_max_tiles_stress,
    bench_snapshot_with_zones,
    bench_snapshot_with_widgets,
);
criterion_main!(benches);
