//! Test scene registry — deterministic named scene configurations for scene, render, and pixel tests.
//!
//! # Design
//!
//! [`TestSceneRegistry`] is the single entry point. Call [`TestSceneRegistry::build`] with a
//! scene name to receive a fully-assembled [`SceneGraph`] and the matching [`SceneSpec`] that
//! describes what Layer 0 invariants must hold.
//!
//! All randomness is absent by design. Every scene is a pure function of its name and the
//! injected clock. The same name always produces the same graph structure (modulo UUIDs, which
//! are assigned freshly on every call but are not inspected by the invariant checks).
//!
//! ## Injectable clock
//!
//! Scene construction calls that need a timestamp (`grant_lease`, `create_tab`, …) ultimately
//! call the internal `now_millis()` helper in `graph.rs`. That helper reads the real wall clock.
//! For tests that want to reason about expiry, the registry accepts a [`ClockMs`] value that is
//! used to derive TTLs and `present_at`/`expires_at` offsets — no wall-clock-sensitive assertions
//! are made directly on those fields.
//!
//! ## Layer 0 assertions
//!
//! [`assert_layer0_invariants`] runs the full invariant suite on any [`SceneGraph`] slice and
//! returns a [`Vec<InvariantViolation>`]. An empty vec means all invariants hold. Individual
//! named checks are also exported so callers can compose narrower assertion sets.

use crate::graph::SceneGraph;
use crate::types::{
    Capability, ContentionPolicy, DisplayEdge, FontFamily, GeometryPolicy, HitRegionNode,
    InputMode, LayerAttachment, Node, NodeData, Rect, RenderingPolicy, Rgba, SceneId,
    SolidColorNode, TextAlign, TextMarkdownNode, TextOverflow, ZoneDefinition, ZoneMediaType,
};

// ─── Clock injection ─────────────────────────────────────────────────────────

/// A timestamp in milliseconds since the Unix epoch, used as the "current time"
/// when constructing test scenes. Inject a fixed value for deterministic expiry tests.
///
/// If you don't care about timing semantics, use [`ClockMs::FIXED`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockMs(pub u64);

impl ClockMs {
    /// A fixed timestamp used when timing doesn't matter (1 January 2025 00:00:00 UTC).
    pub const FIXED: ClockMs = ClockMs(1_735_689_600_000);

    /// Offset this clock by `delta_ms` milliseconds.
    pub fn offset(self, delta_ms: u64) -> Self {
        ClockMs(self.0 + delta_ms)
    }
}

impl Default for ClockMs {
    fn default() -> Self {
        Self::FIXED
    }
}

// ─── Scene specification ──────────────────────────────────────────────────────

/// Metadata describing what a test scene contains.
///
/// Used by higher validation layers (Layer 1–4) to know what to render and check.
/// Layer 0 assertions are derived programmatically from the [`SceneGraph`]; this struct
/// carries only the human-readable description and structural expectations that the
/// graph itself cannot express.
#[derive(Clone, Debug)]
pub struct SceneSpec {
    /// Canonical name (the key used with [`TestSceneRegistry::build`]).
    pub name: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Expected number of tabs.
    pub expected_tab_count: usize,
    /// Expected number of tiles (across all tabs).
    pub expected_tile_count: usize,
    /// Whether any tiles contain hit regions.
    pub has_hit_regions: bool,
    /// Whether the scene registers any zones.
    pub has_zones: bool,
}

// ─── Invariant violation ─────────────────────────────────────────────────────

// InvariantViolation is defined in `invariants` (its canonical home) and
// re-exported here for backward compatibility with existing test_scenes consumers.
pub use crate::invariants::InvariantViolation;

// ─── Test scene registry ──────────────────────────────────────────────────────

/// Registry of named, deterministic test scenes.
///
/// # Usage
///
/// ```rust
/// use tze_hud_scene::test_scenes::{TestSceneRegistry, ClockMs};
///
/// let registry = TestSceneRegistry::new();
/// let (graph, spec) = registry.build("single_tile_solid", ClockMs::FIXED).unwrap();
/// // graph is ready for Layer 0 assertions
/// ```
pub struct TestSceneRegistry {
    /// Display dimensions used for all scenes.
    pub display_width: f32,
    pub display_height: f32,
}

impl Default for TestSceneRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TestSceneRegistry {
    /// Create a registry using the standard 1920×1080 display area.
    pub fn new() -> Self {
        Self {
            display_width: 1920.0,
            display_height: 1080.0,
        }
    }

    /// Create a registry with a custom display area (e.g. the 800×600 pixel-readback surface).
    pub fn with_display(width: f32, height: f32) -> Self {
        Self {
            display_width: width,
            display_height: height,
        }
    }

    /// Build a named scene, returning `(graph, spec)`.
    ///
    /// Returns `None` if the name is not known.
    pub fn build(&self, name: &str, clock: ClockMs) -> Option<(SceneGraph, SceneSpec)> {
        match name {
            "empty_scene" => Some(self.build_empty_scene(clock)),
            "single_tile_solid" => Some(self.build_single_tile_solid(clock)),
            "three_tiles_no_overlap" => Some(self.build_three_tiles_no_overlap(clock)),
            "max_tiles_stress" => Some(self.build_max_tiles_stress(clock)),
            "overlapping_tiles_zorder" => Some(self.build_overlapping_tiles_zorder(clock)),
            "overlay_transparency" => Some(self.build_overlay_transparency(clock)),
            "tab_switch" => Some(self.build_tab_switch(clock)),
            "lease_expiry" => Some(self.build_lease_expiry(clock)),
            "input_highlight" => Some(self.build_input_highlight(clock)),
            "coalesced_dashboard" => Some(self.build_coalesced_dashboard(clock)),
            "three_agents_contention" => Some(self.build_three_agents_contention(clock)),
            "overlay_passthrough_regions" => Some(self.build_overlay_passthrough_regions(clock)),
            "disconnect_reclaim_multiagent" => {
                Some(self.build_disconnect_reclaim_multiagent(clock))
            }
            "chatty_dashboard_touch" => Some(self.build_chatty_dashboard_touch(clock)),
            "zone_publish_subtitle" => Some(self.build_zone_publish_subtitle(clock)),
            "zone_reject_wrong_type" => Some(self.build_zone_reject_wrong_type(clock)),
            "zone_conflict_two_publishers" => Some(self.build_zone_conflict_two_publishers(clock)),
            "zone_orchestrate_then_publish" => {
                Some(self.build_zone_orchestrate_then_publish(clock))
            }
            "zone_disconnect_cleanup" => Some(self.build_zone_disconnect_cleanup(clock)),
            _ => None,
        }
    }

    /// All known scene names.
    pub fn scene_names() -> &'static [&'static str] {
        &[
            "empty_scene",
            "single_tile_solid",
            "three_tiles_no_overlap",
            "max_tiles_stress",
            "overlapping_tiles_zorder",
            "overlay_transparency",
            "tab_switch",
            "lease_expiry",
            "input_highlight",
            "coalesced_dashboard",
            "three_agents_contention",
            "overlay_passthrough_regions",
            "disconnect_reclaim_multiagent",
            "chatty_dashboard_touch",
            "zone_publish_subtitle",
            "zone_reject_wrong_type",
            "zone_conflict_two_publishers",
            "zone_orchestrate_then_publish",
            "zone_disconnect_cleanup",
        ]
    }

    // ─── Scene builders ───────────────────────────────────────────────────

    /// `empty_scene` — no tabs, no tiles. Validates clean initialisation.
    fn build_empty_scene(&self, _clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let graph = SceneGraph::new(self.display_width, self.display_height);

        let spec = SceneSpec {
            name: "empty_scene",
            description: "No tabs, no tiles. Validates clean startup state.",
            expected_tab_count: 0,
            expected_tile_count: 0,
            has_hit_regions: false,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `single_tile_solid` — one tab, one tile with a text content node.
    fn build_single_tile_solid(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Main", 0).expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.single",
            clock.0,
            300_000, // 5-minute TTL
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Tile starts at 10% inset from each edge, occupying 80% of display width and
        // 67% of display height — scales to any display size without exceeding bounds.
        let tile_w = self.display_width * 0.8;
        let tile_h = self.display_height * 0.67;
        let tile_bounds = Rect::new(
            self.display_width * 0.1,
            self.display_height * 0.1,
            tile_w,
            tile_h,
        );
        let tile_id = graph
            .create_tile(tab_id, "agent.single", lease_id, tile_bounds, 1)
            .expect("create_tile failed");

        let text_node = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: "# Hello from tze_hud\n\nThis is a single-tile test scene.".to_string(),
                bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
                font_size_px: 18.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba::WHITE,
                background: Some(Rgba::new(0.08, 0.08, 0.15, 1.0)),
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::default(),
            }),
        };
        graph
            .set_tile_root(tile_id, text_node)
            .expect("set_tile_root failed");

        let spec = SceneSpec {
            name: "single_tile_solid",
            description: "One tab, one tile with markdown text content.",
            expected_tab_count: 1,
            expected_tile_count: 1,
            has_hit_regions: false,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `three_tiles_no_overlap` — one tab, three non-overlapping tiles (text + hit_region + solid).
    fn build_three_tiles_no_overlap(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Dashboard", 0).expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.two",
            clock.0,
            300_000,
            vec![
                Capability::CreateTiles,
                Capability::ModifyOwnTiles,
                Capability::AccessInputEvents,
            ],
        );

        // Layout: left half | right half | status bar at bottom
        // All coordinates are relative to display dimensions so the scene
        // works at any resolution (e.g. 800×600 in pixel-readback tests).
        let half_w = (self.display_width / 2.0) - 15.0;
        let content_h = self.display_height * 0.8;
        let status_h = (self.display_height * 0.1).max(20.0);
        let status_y = self.display_height - status_h;

        // Tile 1 — text content, left half of screen
        let text_tile_bounds = Rect::new(10.0, 10.0, half_w, content_h);
        let text_tile_id = graph
            .create_tile(tab_id, "agent.two", lease_id, text_tile_bounds, 1)
            .expect("create_tile failed");

        let text_node = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: "## two_tiles scene\n\nText tile on the left.".to_string(),
                bounds: Rect::new(0.0, 0.0, half_w, content_h),
                font_size_px: 16.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba::WHITE,
                background: Some(Rgba::new(0.1, 0.1, 0.2, 1.0)),
                alignment: TextAlign::Start,
                overflow: TextOverflow::Ellipsis,
                color_runs: Box::default(),
            }),
        };
        graph
            .set_tile_root(text_tile_id, text_node)
            .expect("set_tile_root failed");

        // Tile 2 — hit region tile, right half of screen
        let hit_x = self.display_width / 2.0 + 5.0;
        let hit_tile_bounds = Rect::new(hit_x, 10.0, half_w, content_h);
        let hit_tile_id = graph
            .create_tile(tab_id, "agent.two", lease_id, hit_tile_bounds, 2)
            .expect("create_tile failed");

        // Hit region inset within the tile
        let hr_x = (half_w * 0.3).min(hit_tile_bounds.width - 10.0);
        let hr_y = (content_h * 0.3).min(hit_tile_bounds.height - 10.0);
        let hr_w = (half_w * 0.4).min(hit_tile_bounds.width - hr_x);
        let hr_h = (content_h * 0.15).min(hit_tile_bounds.height - hr_y);
        let hit_node = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(hr_x, hr_y, hr_w, hr_h),
                interaction_id: "btn-primary".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                ..Default::default()
            }),
        };
        graph
            .set_tile_root(hit_tile_id, hit_node)
            .expect("set_tile_root failed");

        // Tile 3 — solid color status bar at the bottom, no overlap with tiles 1 or 2
        let status_tile_bounds = Rect::new(0.0, status_y, self.display_width, status_h);
        let status_tile_id = graph
            .create_tile(tab_id, "agent.two", lease_id, status_tile_bounds, 3)
            .expect("create_tile failed");

        let status_node = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::new(0.05, 0.05, 0.1, 1.0),
                bounds: Rect::new(0.0, 0.0, self.display_width, status_h),
                radius: None,
            }),
        };
        graph
            .set_tile_root(status_tile_id, status_node)
            .expect("set_tile_root failed");

        let spec = SceneSpec {
            name: "three_tiles_no_overlap",
            description: "One tab, three non-overlapping tiles: text (left), hit-region (right), \
                          and solid-color status bar (bottom). All bounds are disjoint.",
            expected_tab_count: 1,
            expected_tile_count: 3,
            has_hit_regions: true,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `max_tiles_stress` — stress test with many tiles, approaching the default `max_nodes` budget.
    ///
    /// Creates 60 tiles on a single tab (default budget is 64). This exercises the scene graph
    /// under load and validates that bookkeeping remains consistent near capacity.
    fn build_max_tiles_stress(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Stress", 0).expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.stress",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // 10 columns × 6 rows = 60 tiles
        let cols = 10u32;
        let rows = 6u32;
        let tile_w = self.display_width / cols as f32;
        let tile_h = self.display_height / rows as f32;

        for row in 0..rows {
            for col in 0..cols {
                let z = row * cols + col + 1;
                let bounds = Rect::new(
                    col as f32 * tile_w,
                    row as f32 * tile_h,
                    tile_w - 2.0, // 2px gap
                    tile_h - 2.0,
                );
                let tile_id = graph
                    .create_tile(tab_id, "agent.stress", lease_id, bounds, z)
                    .expect("create_tile failed in max_tiles");

                // Alternate between SolidColor and TextMarkdown to exercise both node types
                let node = if z % 2 == 0 {
                    Node {
                        layout: Default::default(),
                        id: SceneId::new(),
                        children: vec![],
                        data: NodeData::SolidColor(SolidColorNode {
                            color: Rgba::new(
                                (col as f32) / cols as f32,
                                (row as f32) / rows as f32,
                                0.5,
                                1.0,
                            ),
                            bounds: Rect::new(0.0, 0.0, tile_w - 2.0, tile_h - 2.0),
                            radius: None,
                        }),
                    }
                } else {
                    Node {
                        layout: Default::default(),
                        id: SceneId::new(),
                        children: vec![],
                        data: NodeData::TextMarkdown(TextMarkdownNode {
                            content: format!("tile {z}"),
                            bounds: Rect::new(0.0, 0.0, tile_w - 2.0, tile_h - 2.0),
                            font_size_px: 12.0,
                            font_family: FontFamily::SystemMonospace,
                            color: Rgba::WHITE,
                            background: None,
                            alignment: TextAlign::Center,
                            overflow: TextOverflow::Clip,
                            color_runs: Box::default(),
                        }),
                    }
                };
                graph
                    .set_tile_root(tile_id, node)
                    .expect("set_tile_root failed in max_tiles_stress");
            }
        }

        let tile_count = (cols * rows) as usize;

        let spec = SceneSpec {
            name: "max_tiles_stress",
            description: "Stress test with 60 tiles (near the 64-node default budget) on a \
                          single tab. Exercises scene graph bookkeeping under load.",
            expected_tab_count: 1,
            expected_tile_count: tile_count,
            has_hit_regions: false,
            has_zones: false,
        };

        (graph, spec)
    }

    // ─── More scene builders ─────────────────────────────────────────────

    /// `overlapping_tiles_zorder` — 3 tiles with overlapping bounds and explicit z-orders.
    ///
    /// Validates z-order composition: the compositing layer must respect z_order even when
    /// tile bounds intersect. Layer 0 invariant: all z-orders are distinct per tab.
    fn build_overlapping_tiles_zorder(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Overlap", 0).expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.overlap",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Three overlapping tiles placed so that at (display_w/2, display_h*0.42)
        // all three overlap and z=3 (blue) wins. Tiles are display-relative so
        // they fit within any reasonable resolution (including 800×600).
        //
        // tile_w = 50% of display; tile_h = 67% of display.
        // Offsets: base starts at 10%×16%, mid at 20%×25%, top at 30%×33%.
        // At 800×600: base=(80,97,400,400), mid=(160,150,400,400), top=(240,200,400,400).
        // Overlap centre ≈ (400,300) — all three tiles cover that point.
        let tile_w = self.display_width * 0.5;
        let tile_h = self.display_height * 0.67;
        let base = Rect::new(
            self.display_width * 0.10,
            self.display_height * 0.16,
            tile_w,
            tile_h,
        );
        let mid = Rect::new(
            self.display_width * 0.20,
            self.display_height * 0.25,
            tile_w,
            tile_h,
        );
        let top = Rect::new(
            self.display_width * 0.30,
            self.display_height * 0.33,
            tile_w,
            tile_h,
        );

        for (bounds, z, color) in [
            (base, 1u32, Rgba::new(0.8, 0.2, 0.2, 1.0)),
            (mid, 2u32, Rgba::new(0.2, 0.8, 0.2, 1.0)),
            (top, 3u32, Rgba::new(0.2, 0.2, 0.8, 1.0)),
        ] {
            let tile_id = graph
                .create_tile(tab_id, "agent.overlap", lease_id, bounds, z)
                .expect("create_tile failed");
            let node = Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color,
                    bounds: Rect::new(0.0, 0.0, bounds.width, bounds.height),
                    radius: None,
                }),
            };
            graph
                .set_tile_root(tile_id, node)
                .expect("set_tile_root failed");
        }

        let spec = SceneSpec {
            name: "overlapping_tiles_zorder",
            description: "Three tiles with deliberately overlapping bounds and distinct \
                          z-orders (1, 2, 3). Validates z-order composition: higher z-order \
                          must occlude lower z-order tiles per scene-graph/spec.md §3.",
            expected_tab_count: 1,
            expected_tile_count: 3,
            has_hit_regions: false,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `overlay_transparency` — chrome overlay with alpha < 1.0 over an agent tile.
    ///
    /// Validates the alpha blending path. Layer 0: opacity is in [0.0, 1.0].
    /// Layer 1 pixel expectation: ±2/channel blending tolerance.
    fn build_overlay_transparency(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Overlay", 0).expect("create_tab failed");

        let agent_lease = graph.grant_lease_at(
            "agent.base",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        let chrome_lease = graph.grant_lease_at(
            "chrome.overlay",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Base agent tile — full background
        let base_bounds = Rect::new(0.0, 0.0, self.display_width, self.display_height);
        let base_tile = graph
            .create_tile(tab_id, "agent.base", agent_lease, base_bounds, 1)
            .expect("create_tile failed");
        graph
            .set_tile_root(
                base_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(0.1, 0.1, 0.5, 1.0),
                        bounds: Rect::new(0.0, 0.0, self.display_width, self.display_height),
                        radius: None,
                    }),
                },
            )
            .expect("set_tile_root failed");

        // Chrome overlay tile with semi-transparent opacity
        let overlay_bounds = Rect::new(200.0, 200.0, 400.0, 200.0);
        let overlay_tile = graph
            .create_tile(tab_id, "chrome.overlay", chrome_lease, overlay_bounds, 10)
            .expect("create_tile failed");
        graph
            .set_tile_root(
                overlay_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(1.0, 1.0, 1.0, 0.5),
                        bounds: Rect::new(0.0, 0.0, overlay_bounds.width, overlay_bounds.height),
                        radius: None,
                    }),
                },
            )
            .expect("set_tile_root failed");

        // Set tile-level opacity to 0.75 to exercise the tile opacity path
        graph
            .tiles
            .get_mut(&overlay_tile)
            .expect("overlay tile missing")
            .opacity = 0.75;

        let spec = SceneSpec {
            name: "overlay_transparency",
            description: "Chrome overlay tile (opacity=0.75, color alpha=0.5) over a solid \
                          agent tile. Validates alpha blending path with ±2/channel tolerance \
                          per heart-and-soul/validation.md line 117.",
            expected_tab_count: 1,
            expected_tile_count: 2,
            has_hit_regions: false,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `tab_switch` — 2 tabs with different tile layouts; validates tab isolation.
    ///
    /// Layer 0: each tab's tiles are independent; z_orders are unique per tab (not globally).
    fn build_tab_switch(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_a = graph.create_tab("TabA", 0).expect("create_tab failed");
        let tab_b = graph.create_tab("TabB", 1).expect("create_tab failed");

        let lease_a = graph.grant_lease_at(
            "agent.tabA",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let lease_b = graph.grant_lease_at(
            "agent.tabB",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Tab A: 1 tile — 5% inset, 90% wide, 67% tall (display-relative)
        let tab_a_tile_w = self.display_width * 0.90;
        let tab_a_tile_h = self.display_height * 0.67;
        let tab_a_tile_bounds = Rect::new(
            self.display_width * 0.05,
            self.display_height * 0.05,
            tab_a_tile_w,
            tab_a_tile_h,
        );
        let tile_a = graph
            .create_tile(tab_a, "agent.tabA", lease_a, tab_a_tile_bounds, 1)
            .expect("create_tile failed");
        graph
            .set_tile_root(
                tile_a,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "Tab A content".to_string(),
                        bounds: Rect::new(0.0, 0.0, tab_a_tile_w, tab_a_tile_h),
                        font_size_px: 18.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: Some(Rgba::new(0.1, 0.2, 0.4, 1.0)),
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .expect("set_tile_root failed");

        // Tab B: 2 tiles using the same z_orders as Tab A — valid because z_order is per-tab.
        // Tiles are side-by-side, each occupying ~44% of display width (with 5% gaps).
        let tab_b_tile_w = (self.display_width - self.display_width * 0.15) / 2.0;
        let tab_b_tile_h = self.display_height * 0.5;
        let tab_b_tile_y = self.display_height * 0.1;
        for (i, (z, label)) in [(1u32, "Tab B tile 1"), (2u32, "Tab B tile 2")]
            .iter()
            .enumerate()
        {
            let x =
                self.display_width * 0.05 + i as f32 * (tab_b_tile_w + self.display_width * 0.05);
            let tile = graph
                .create_tile(
                    tab_b,
                    "agent.tabB",
                    lease_b,
                    Rect::new(x, tab_b_tile_y, tab_b_tile_w, tab_b_tile_h),
                    *z,
                )
                .expect("create_tile failed");
            graph
                .set_tile_root(
                    tile,
                    Node {
                        layout: Default::default(),
                        id: SceneId::new(),
                        children: vec![],
                        data: NodeData::TextMarkdown(TextMarkdownNode {
                            content: label.to_string(),
                            bounds: Rect::new(0.0, 0.0, tab_b_tile_w, tab_b_tile_h),
                            font_size_px: 16.0,
                            font_family: FontFamily::SystemSansSerif,
                            color: Rgba::WHITE,
                            background: Some(Rgba::new(0.2, 0.1, 0.3, 1.0)),
                            alignment: TextAlign::Start,
                            overflow: TextOverflow::Clip,
                            color_runs: Box::default(),
                        }),
                    },
                )
                .expect("set_tile_root failed");
        }

        // Switch to tab B so active_tab != tab_a (tests tab switching logic)
        graph
            .switch_active_tab(tab_b)
            .expect("switch_active_tab failed");

        let spec = SceneSpec {
            name: "tab_switch",
            description: "Two tabs: Tab A has 1 tile, Tab B has 2 tiles. Active tab is B. \
                          Validates tab isolation: z_orders are per-tab, not global. \
                          Per scene-graph/spec.md §2 (Tab[0-256]).",
            expected_tab_count: 2,
            expected_tile_count: 3,
            has_hit_regions: false,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `lease_expiry` — tile with a very short TTL; validates ACTIVE→EXPIRED transition.
    ///
    /// The lease is granted with TTL = 1ms relative to `clock`.  The scene as-built has
    /// state = ACTIVE.  To test expiry, callers must call `expire_leases(now_ms)` with a
    /// `now_ms` value past the TTL.  Note: this scene uses `SceneGraph::new()` (system
    /// clock), so the injected `clock` only sets `granted_at_ms`; time advancement for
    /// expiry testing requires passing `now_ms` directly to `expire_leases(now_ms)`.
    fn build_lease_expiry(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Expiring", 0).expect("create_tab failed");

        // Short-lived lease: expires 1 ms after `clock`
        let lease_id = graph.grant_lease_at(
            "agent.expiry",
            clock.0,
            1, // TTL = 1 ms — already logically past if now > clock+1
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        let tile_id = graph
            .create_tile(
                tab_id,
                "agent.expiry",
                lease_id,
                Rect::new(100.0, 100.0, 600.0, 400.0),
                1,
            )
            .expect("create_tile failed");

        graph
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "This tile will expire (TTL = 1ms)".to_string(),
                        bounds: Rect::new(0.0, 0.0, 600.0, 400.0),
                        font_size_px: 16.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: Some(Rgba::new(0.5, 0.1, 0.1, 1.0)),
                        alignment: TextAlign::Center,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .expect("set_tile_root failed");

        // Leave lease in ACTIVE state — test callers drive the ACTIVE→EXPIRED transition
        // by calling expire_leases(now_ms) with now_ms > granted_at_ms + 1
        // (lease-governance/spec.md lines 10-25).

        let spec = SceneSpec {
            name: "lease_expiry",
            description: "One tile with a 1ms TTL lease (ACTIVE state at build time). \
                          Call expire_leases(now_ms) with now_ms past the TTL to drive the \
                          ACTIVE→EXPIRED transition and remove the tile. Validates the \
                          ACTIVE→EXPIRED state machine per lease-governance/spec.md §1.",
            expected_tab_count: 1,
            expected_tile_count: 1,
            has_hit_regions: false,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `input_highlight` — tile with a HitRegionNode accepting focus and pointer events.
    ///
    /// Validates the focus tree (per-tab, at most one focus owner) and focus cycling
    /// per input-model/spec.md lines 11-22 and 78-89.
    fn build_input_highlight(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Input", 0).expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.input",
            clock.0,
            300_000,
            vec![
                Capability::CreateTiles,
                Capability::ModifyOwnTiles,
                Capability::AccessInputEvents,
            ],
        );

        // Background tile
        let bg_tile = graph
            .create_tile(
                tab_id,
                "agent.input",
                lease_id,
                Rect::new(0.0, 0.0, self.display_width, self.display_height),
                1,
            )
            .expect("create_tile failed");
        graph
            .set_tile_root(
                bg_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(0.05, 0.05, 0.15, 1.0),
                        bounds: Rect::new(0.0, 0.0, self.display_width, self.display_height),
                        radius: None,
                    }),
                },
            )
            .expect("set_tile_root failed");

        // Interactive tile with hit region (accepts focus + pointer)
        let btn_tile = graph
            .create_tile(
                tab_id,
                "agent.input",
                lease_id,
                Rect::new(400.0, 300.0, 400.0, 100.0),
                5,
            )
            .expect("create_tile failed");
        graph
            .set_tile_root(
                btn_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 100.0),
                        interaction_id: "primary-button".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .expect("set_tile_root failed");

        let spec = SceneSpec {
            name: "input_highlight",
            description: "Background tile plus an interactive tile with a HitRegionNode \
                          (accepts_focus=true, accepts_pointer=true). Validates the focus \
                          tree (per-tab, ≤1 owner) and focus cycling per \
                          input-model/spec.md lines 11-22 and 78-89.",
            expected_tab_count: 1,
            expected_tile_count: 2,
            has_hit_regions: true,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `coalesced_dashboard` — 12 tiles with sequential mutations demonstrating state-stream
    /// coalescing. Validates atomic batch semantics per scene-graph/spec.md lines 142-157.
    fn build_coalesced_dashboard(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Dashboard", 0).expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.dashboard",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // 4 columns × 3 rows = 12 tiles representing a live dashboard layout
        let cols = 4u32;
        let rows = 3u32;
        let pad = 10.0_f32;
        let tile_w = (self.display_width - pad * (cols as f32 + 1.0)) / cols as f32;
        let tile_h = (self.display_height - pad * (rows as f32 + 1.0)) / rows as f32;

        let metrics = [
            "CPU",
            "Memory",
            "Network In",
            "Network Out",
            "Disk Read",
            "Disk Write",
            "Latency",
            "Throughput",
            "Error Rate",
            "Queue Depth",
            "Active Conns",
            "Uptime",
        ];

        for row in 0..rows {
            for col in 0..cols {
                let idx = (row * cols + col) as usize;
                let z = idx as u32 + 1;
                let x = pad + col as f32 * (tile_w + pad);
                let y = pad + row as f32 * (tile_h + pad);
                let tile_id = graph
                    .create_tile(
                        tab_id,
                        "agent.dashboard",
                        lease_id,
                        Rect::new(x, y, tile_w, tile_h),
                        z,
                    )
                    .expect("create_tile failed in coalesced_dashboard");

                graph
                    .set_tile_root(
                        tile_id,
                        Node {
                            layout: Default::default(),
                            id: SceneId::new(),
                            children: vec![],
                            data: NodeData::TextMarkdown(TextMarkdownNode {
                                content: format!(
                                    "**{}**\n\n`{:.1}%`",
                                    metrics[idx],
                                    (idx as f32 * 7.3) % 100.0
                                ),
                                bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
                                font_size_px: 13.0,
                                font_family: FontFamily::SystemMonospace,
                                color: Rgba::WHITE,
                                background: Some(Rgba::new(
                                    0.08 + (col as f32 * 0.04),
                                    0.1,
                                    0.18,
                                    1.0,
                                )),
                                alignment: TextAlign::Start,
                                overflow: TextOverflow::Ellipsis,
                                color_runs: Box::default(),
                            }),
                        },
                    )
                    .expect("set_tile_root failed in coalesced_dashboard");
            }
        }

        let spec = SceneSpec {
            name: "coalesced_dashboard",
            description: "12-tile dashboard (4 cols × 3 rows) representing live metrics. \
                          Demonstrates the state-stream coalescing path: rapid sequential \
                          set_tile_root calls on many tiles. Per scene-graph/spec.md §5 \
                          (atomic batch, lines 142-157).",
            expected_tab_count: 1,
            expected_tile_count: 12,
            has_hit_regions: false,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `three_agents_contention` — 3 agents with different lease priorities and overlapping
    /// z-order requests.
    ///
    /// Validates priority sort: lease_priority ASC, z_order DESC per
    /// lease-governance/spec.md lines 62-69.
    fn build_three_agents_contention(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph
            .create_tab("Contention", 0)
            .expect("create_tab failed");

        // Three agents at different priorities (lower number = higher priority)
        let agents = [
            ("agent.high_prio", 1u8),
            ("agent.normal_prio", 2u8),
            ("agent.low_prio", 3u8),
        ];

        let leases: Vec<SceneId> = agents
            .iter()
            .map(|(ns, priority)| {
                use crate::types::{Lease, LeaseState, RenewalPolicy, ResourceBudget};
                let id = SceneId::new();
                graph.leases.insert(
                    id,
                    Lease {
                        id,
                        namespace: ns.to_string(),
                        session_id: SceneId::nil(),
                        state: LeaseState::Active,
                        priority: *priority,
                        granted_at_ms: clock.0,
                        ttl_ms: 300_000,
                        renewal_policy: RenewalPolicy::default(),
                        capabilities: vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
                        resource_budget: ResourceBudget::default(),
                        spatial_budget: Default::default(),
                        suspended_at_ms: None,
                        ttl_remaining_at_suspend_ms: None,
                        disconnected_at_ms: None,
                        grace_period_ms: SceneGraph::DEFAULT_GRACE_PERIOD_MS,
                    },
                );
                graph.version += 1;
                id
            })
            .collect();

        // Each agent places a tile that partially overlaps the others.
        // Tiles are display-relative and stay within bounds at any resolution.
        // tile_w = 55% of display; tile_h = 55% of display.
        // Offsets: 10%×16%, 20%×25%, 30%×33% — all right/bottom edges within display.
        let contention_tile_w = self.display_width * 0.55;
        let contention_tile_h = self.display_height * 0.55;
        let positions = [
            Rect::new(
                self.display_width * 0.10,
                self.display_height * 0.16,
                contention_tile_w,
                contention_tile_h,
            ),
            Rect::new(
                self.display_width * 0.20,
                self.display_height * 0.25,
                contention_tile_w,
                contention_tile_h,
            ),
            Rect::new(
                self.display_width * 0.30,
                self.display_height * 0.33,
                contention_tile_w,
                contention_tile_h,
            ),
        ];
        let colors = [
            Rgba::new(0.8, 0.2, 0.2, 1.0),
            Rgba::new(0.2, 0.8, 0.2, 1.0),
            Rgba::new(0.2, 0.2, 0.8, 1.0),
        ];

        for ((ns, _), (lease_id, (bounds, color))) in agents
            .iter()
            .zip(leases.iter().zip(positions.iter().zip(colors.iter())))
        {
            let z = match *ns {
                "agent.high_prio" => 10u32,
                "agent.normal_prio" => 5u32,
                _ => 1u32,
            };
            let tile_id = graph
                .create_tile(tab_id, ns, *lease_id, *bounds, z)
                .expect("create_tile failed");
            graph
                .set_tile_root(
                    tile_id,
                    Node {
                        layout: Default::default(),
                        id: SceneId::new(),
                        children: vec![],
                        data: NodeData::SolidColor(SolidColorNode {
                            color: *color,
                            bounds: Rect::new(0.0, 0.0, bounds.width, bounds.height),
                            radius: None,
                        }),
                    },
                )
                .expect("set_tile_root failed");
        }

        let spec = SceneSpec {
            name: "three_agents_contention",
            description: "Three agents with lease priorities 1 (high), 2 (normal), 3 (low) \
                          each placing overlapping tiles at z-orders 10, 5, 1. Validates \
                          priority-sort contention resolution: lease_priority ASC, \
                          z_order DESC per lease-governance/spec.md lines 62-69.",
            expected_tab_count: 1,
            expected_tile_count: 3,
            has_hit_regions: false,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `overlay_passthrough_regions` — chrome overlay with mixed passthrough / capture regions.
    ///
    /// Validates the hit-test pipeline: chrome-first, z-descending, reverse tree order per
    /// input-model/spec.md line 264. Passthrough tiles let pointer events fall through.
    fn build_overlay_passthrough_regions(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph
            .create_tab("Passthrough", 0)
            .expect("create_tab failed");

        let agent_lease = graph.grant_lease_at(
            "agent.content",
            clock.0,
            300_000,
            vec![
                Capability::CreateTiles,
                Capability::ModifyOwnTiles,
                Capability::AccessInputEvents,
            ],
        );
        let chrome_lease = graph.grant_lease_at(
            "chrome.ui",
            clock.0,
            300_000,
            vec![
                Capability::CreateTiles,
                Capability::ModifyOwnTiles,
                Capability::AccessInputEvents,
            ],
        );

        // Content tile — below the overlay, accepts input in its own region
        let content_tile = graph
            .create_tile(
                tab_id,
                "agent.content",
                agent_lease,
                Rect::new(0.0, 0.0, self.display_width, self.display_height),
                1,
            )
            .expect("create_tile failed");
        graph
            .set_tile_root(
                content_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, self.display_width, self.display_height),
                        interaction_id: "content-area".to_string(),
                        accepts_focus: false,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .expect("set_tile_root failed");

        // Chrome overlay — PASSTHROUGH input mode (pointer events fall through)
        let overlay_tile = graph
            .create_tile(
                tab_id,
                "chrome.ui",
                chrome_lease,
                Rect::new(0.0, 0.0, self.display_width, self.display_height),
                20,
            )
            .expect("create_tile failed");
        graph
            .tiles
            .get_mut(&overlay_tile)
            .expect("overlay tile missing")
            .input_mode = InputMode::Passthrough;
        graph
            .set_tile_root(
                overlay_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(0.0, 0.0, 0.0, 0.15),
                        bounds: Rect::new(0.0, 0.0, self.display_width, self.display_height),
                        radius: None,
                    }),
                },
            )
            .expect("set_tile_root failed");

        // Interactive chrome widget on top — CAPTURE (blocks input)
        let widget_tile = graph
            .create_tile(
                tab_id,
                "chrome.ui",
                chrome_lease,
                Rect::new(self.display_width - 200.0, 20.0, 180.0, 60.0),
                30,
            )
            .expect("create_tile failed");
        graph
            .set_tile_root(
                widget_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 180.0, 60.0),
                        interaction_id: "chrome-menu-button".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .expect("set_tile_root failed");

        let spec = SceneSpec {
            name: "overlay_passthrough_regions",
            description: "Content tile (z=1, Capture) beneath a full-screen passthrough \
                          overlay (z=20, Passthrough) with a small capture widget (z=30). \
                          Validates hit-test pipeline: chrome-first, z-descending, \
                          per input-model/spec.md line 264.",
            expected_tab_count: 1,
            expected_tile_count: 3,
            has_hit_regions: true,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `disconnect_reclaim_multiagent` — three agents hold tiles simultaneously,
    /// all starting in Active state.
    ///
    /// This scene provides the initial state for disconnect/reconnect reclaim tests.
    /// All three agents start Active so tests can exercise the full lifecycle:
    /// disconnect one agent, verify others are unaffected, reconnect within grace.
    ///
    /// - `agent.one` holds two tiles (left third of screen)
    /// - `agent.two` holds one tile (middle third)
    /// - `agent.three` holds one tile (right third)
    ///
    /// Validates:
    /// - V1 Success Criterion: Live Multi-Agent Presence
    /// - Thesis 2: The lease model works (lease reclaim on reconnect)
    /// - Thesis 3: Multiple agents coexist (disconnect/reconnect does not affect others)
    /// - validation-framework spec §Test Scene Registry lines 160-172
    fn build_disconnect_reclaim_multiagent(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        // Use a SimulatedClock fixed at `clock.0` so that lease-expiry checks
        // compare against the scene's construction timestamp rather than the real system
        // clock. This avoids false `LeaseExpired` errors when session_lifecycle tests run
        // years after the lease `granted_at_ms` (ClockMs::FIXED = Jan 2025).
        use crate::clock::SimulatedClock;
        use std::sync::Arc;
        // SimulatedClock::new takes microseconds; ClockMs stores milliseconds.
        let sim_clock = Arc::new(SimulatedClock::new(clock.0 * 1_000));
        let mut graph =
            SceneGraph::new_with_clock(self.display_width, self.display_height, sim_clock);

        let tab_id = graph
            .create_tab("MultiAgent", 0)
            .expect("create_tab failed");

        // Three agents — all start Active
        let lease_one = graph.grant_lease_at(
            "agent.one",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let lease_two = graph.grant_lease_at(
            "agent.two",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let lease_three = graph.grant_lease_at(
            "agent.three",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Layout: left third (agent.one × 2 tiles) | middle third (agent.two) | right third (agent.three)
        // All coordinates are display-relative so the scene works at any resolution.
        let third_w = (self.display_width - 30.0) / 3.0;
        let pad = 10.0_f32;
        let top_h = self.display_height * 0.77;
        let bot_h = self.display_height - top_h - pad * 3.0;

        // Agent One: two tiles on the left third of the screen
        let one_bounds_a = Rect::new(pad, pad, third_w, top_h);
        let tile_one_a = graph
            .create_tile(tab_id, "agent.one", lease_one, one_bounds_a, 1)
            .expect("create_tile agent.one tile_a failed");
        graph
            .set_tile_root(
                tile_one_a,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(0.8, 0.2, 0.2, 1.0),
                        bounds: Rect::new(0.0, 0.0, one_bounds_a.width, one_bounds_a.height),
                        radius: None,
                    }),
                },
            )
            .expect("set_tile_root agent.one tile_a failed");

        let one_bounds_b = Rect::new(pad, top_h + pad * 2.0, third_w, bot_h);
        let tile_one_b = graph
            .create_tile(tab_id, "agent.one", lease_one, one_bounds_b, 2)
            .expect("create_tile agent.one tile_b failed");
        graph
            .set_tile_root(
                tile_one_b,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "agent.one — second tile".to_string(),
                        bounds: Rect::new(0.0, 0.0, one_bounds_b.width, one_bounds_b.height),
                        font_size_px: 14.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: Some(Rgba::new(0.4, 0.1, 0.1, 1.0)),
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .expect("set_tile_root agent.one tile_b failed");

        // Agent Two: one tile in the middle third
        let two_x = pad * 2.0 + third_w;
        let two_h = self.display_height - pad * 2.0;
        let two_bounds = Rect::new(two_x, pad, third_w, two_h);
        let tile_two = graph
            .create_tile(tab_id, "agent.two", lease_two, two_bounds, 3)
            .expect("create_tile agent.two failed");
        graph
            .set_tile_root(
                tile_two,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(0.2, 0.7, 0.2, 1.0),
                        bounds: Rect::new(0.0, 0.0, two_bounds.width, two_bounds.height),
                        radius: None,
                    }),
                },
            )
            .expect("set_tile_root agent.two failed");

        // Agent Three: one tile on the right third
        let three_x = pad * 3.0 + third_w * 2.0;
        let three_w = self.display_width - three_x - pad;
        let three_bounds = Rect::new(three_x, pad, three_w, two_h);
        let tile_three = graph
            .create_tile(tab_id, "agent.three", lease_three, three_bounds, 4)
            .expect("create_tile agent.three failed");
        graph
            .set_tile_root(
                tile_three,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(0.2, 0.4, 0.9, 1.0),
                        bounds: Rect::new(0.0, 0.0, three_bounds.width, three_bounds.height),
                        radius: None,
                    }),
                },
            )
            .expect("set_tile_root agent.three failed");

        let spec = SceneSpec {
            name: "disconnect_reclaim_multiagent",
            description: "Three agents (agent.one, agent.two, agent.three) each hold tiles simultaneously. \
                 agent.one has two tiles; agent.two and agent.three each have one. \
                 All start Active. Used to test disconnect/reconnect reclaim without disrupting \
                 other agents. Validates V1 thesis: multi-agent coexistence and lease reclaim. \
                 Per validation-framework spec §Test Scene Registry lines 160-172.",
            expected_tab_count: 1,
            expected_tile_count: 4, // 2 + 1 + 1
            has_hit_regions: false,
            has_zones: false,
        };

        // Suppress unused variable warnings — tile IDs are not needed in the spec
        let _ = (tile_one_a, tile_one_b, tile_two, tile_three);

        (graph, spec)
    }

    /// `chatty_dashboard_touch` — dashboard layout with HitRegionNode tiles ready for
    /// high-frequency input injection (<100µs hit-test for 50 tiles).
    fn build_chatty_dashboard_touch(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Chatty", 0).expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.chatty",
            clock.0,
            300_000,
            vec![
                Capability::CreateTiles,
                Capability::ModifyOwnTiles,
                Capability::AccessInputEvents,
            ],
        );

        // 5 columns × 10 rows = 50 hit-region tiles (one per cell)
        let cols = 5u32;
        let rows = 10u32;
        let tile_w = self.display_width / cols as f32;
        let tile_h = self.display_height / rows as f32;

        for row in 0..rows {
            for col in 0..cols {
                let z = row * cols + col + 1;
                let x = col as f32 * tile_w;
                let y = row as f32 * tile_h;
                let tile_id = graph
                    .create_tile(
                        tab_id,
                        "agent.chatty",
                        lease_id,
                        Rect::new(x, y, tile_w - 1.0, tile_h - 1.0),
                        z,
                    )
                    .expect("create_tile failed in chatty_dashboard_touch");
                graph
                    .set_tile_root(
                        tile_id,
                        Node {
                            layout: Default::default(),
                            id: SceneId::new(),
                            children: vec![],
                            data: NodeData::HitRegion(HitRegionNode {
                                bounds: Rect::new(0.0, 0.0, tile_w - 1.0, tile_h - 1.0),
                                interaction_id: format!("cell-{row}-{col}"),
                                accepts_focus: false,
                                accepts_pointer: true,
                                ..Default::default()
                            }),
                        },
                    )
                    .expect("set_tile_root failed in chatty_dashboard_touch");
            }
        }

        let spec = SceneSpec {
            name: "chatty_dashboard_touch",
            description: "50 hit-region tiles in a 5×10 grid, each ready for high-frequency \
                          touch/pointer input injection. Validates the input drain budget: \
                          hit-test for 50 tiles must complete in <100µs per \
                          input-model/spec.md line 264.",
            expected_tab_count: 1,
            expected_tile_count: 50,
            has_hit_regions: true,
            has_zones: false,
        };

        (graph, spec)
    }

    /// `zone_publish_subtitle` — tile publishing to the subtitle zone.
    ///
    /// Renamed from `zone_test` in the canonical scene list. Validates zone registry
    /// operations and tile-to-zone mapping per scene-graph/spec.md lines 198-200.
    fn build_zone_publish_subtitle(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Subtitle", 0).expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.subtitle",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Register subtitle zone
        graph.zone_registry.zones.insert(
            "subtitle".to_string(),
            ZoneDefinition {
                id: SceneId::new(),
                name: "subtitle".to_string(),
                description: "Centered subtitle overlay at the bottom of the screen.".to_string(),
                geometry_policy: GeometryPolicy::EdgeAnchored {
                    edge: DisplayEdge::Bottom,
                    height_pct: 0.10,
                    width_pct: 0.80,
                    margin_px: 48.0,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 1,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );

        // Tile publishing to the subtitle zone
        let sub_bounds = Rect::new(
            self.display_width * 0.1,
            self.display_height * 0.88,
            self.display_width * 0.8,
            self.display_height * 0.08,
        );
        let tile_id = graph
            .create_tile(tab_id, "agent.subtitle", lease_id, sub_bounds, 10)
            .expect("create_tile failed");
        graph
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "Subtitle zone: StreamText content".to_string(),
                        bounds: Rect::new(0.0, 0.0, sub_bounds.width, sub_bounds.height),
                        font_size_px: 20.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: Some(Rgba::new(0.0, 0.0, 0.0, 0.75)),
                        alignment: TextAlign::Center,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .expect("set_tile_root failed");

        let spec = SceneSpec {
            name: "zone_publish_subtitle",
            description: "One tile publishing StreamText to the subtitle zone \
                          (EdgeAnchored bottom, 80% width, LatestWins contention, \
                          max_publishers=1). Validates zone registry + tile-to-zone \
                          mapping per scene-graph/spec.md lines 198-200.",
            expected_tab_count: 1,
            expected_tile_count: 1,
            has_hit_regions: false,
            has_zones: true,
        };

        (graph, spec)
    }

    /// `zone_reject_wrong_type` — zone configured for StreamText; scene encodes the
    /// expectation that a wrong content type would be rejected.
    ///
    /// The scene itself is structurally valid (it builds without error). The rejection
    /// semantic is documented in the SceneSpec description so higher validation layers
    /// can inject wrong-type publishes and assert the error.
    fn build_zone_reject_wrong_type(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("TypedZone", 0).expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.typed",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Zone accepts ONLY StreamText
        graph.zone_registry.zones.insert(
            "typed_zone".to_string(),
            ZoneDefinition {
                id: SceneId::new(),
                name: "typed_zone".to_string(),
                description: "Zone that accepts only StreamText (used to validate type rejection)."
                    .to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.2,
                    y_pct: 0.6,
                    width_pct: 0.6,
                    height_pct: 0.2,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 2,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );

        let tile_bounds = Rect::new(
            self.display_width * 0.2,
            self.display_height * 0.6,
            self.display_width * 0.6,
            self.display_height * 0.2,
        );
        let tile_id = graph
            .create_tile(tab_id, "agent.typed", lease_id, tile_bounds, 1)
            .expect("create_tile failed");
        graph
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "typed_zone accepts StreamText only".to_string(),
                        bounds: Rect::new(0.0, 0.0, tile_bounds.width, tile_bounds.height),
                        font_size_px: 16.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: Some(Rgba::new(0.15, 0.1, 0.25, 1.0)),
                        alignment: TextAlign::Center,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .expect("set_tile_root failed");

        let spec = SceneSpec {
            name: "zone_reject_wrong_type",
            description: "Zone 'typed_zone' accepts only ZoneMediaType::StreamText. \
                          Injecting a KeyValuePairs or Notification payload must be \
                          rejected with a type-mismatch error. Per scene-graph/spec.md \
                          lines 198-200 (type validation).",
            expected_tab_count: 1,
            expected_tile_count: 1,
            has_hit_regions: false,
            has_zones: true,
        };

        (graph, spec)
    }

    /// `zone_conflict_two_publishers` — 2 agents publishing to the same zone with
    /// LatestWins contention policy.
    ///
    /// Per scene-graph/spec.md lines 185-196.
    fn build_zone_conflict_two_publishers(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph.create_tab("Conflict", 0).expect("create_tab failed");

        let lease_a = graph.grant_lease_at(
            "agent.pub_a",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let lease_b = graph.grant_lease_at(
            "agent.pub_b",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Shared zone with LatestWins — second publish replaces first
        graph.zone_registry.zones.insert(
            "shared_banner".to_string(),
            ZoneDefinition {
                id: SceneId::new(),
                name: "shared_banner".to_string(),
                description: "Shared zone for contention testing (LatestWins).".to_string(),
                geometry_policy: GeometryPolicy::EdgeAnchored {
                    edge: DisplayEdge::Top,
                    height_pct: 0.06,
                    width_pct: 1.0,
                    margin_px: 0.0,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 2,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Chrome,
            },
        );

        // Both agents place a tile targeting the shared_banner zone
        let banner_bounds = Rect::new(0.0, 0.0, self.display_width, self.display_height * 0.06);

        for (ns, lease_id, z, content) in [
            (
                "agent.pub_a",
                lease_a,
                1u32,
                "Publisher A — will be evicted",
            ),
            ("agent.pub_b", lease_b, 2u32, "Publisher B — LatestWins"),
        ] {
            let tile_id = graph
                .create_tile(tab_id, ns, lease_id, banner_bounds, z)
                .expect("create_tile failed");
            graph
                .set_tile_root(
                    tile_id,
                    Node {
                        layout: Default::default(),
                        id: SceneId::new(),
                        children: vec![],
                        data: NodeData::TextMarkdown(TextMarkdownNode {
                            content: content.to_string(),
                            bounds: Rect::new(0.0, 0.0, banner_bounds.width, banner_bounds.height),
                            font_size_px: 14.0,
                            font_family: FontFamily::SystemSansSerif,
                            color: Rgba::WHITE,
                            background: Some(Rgba::new(0.2, 0.1, 0.1, 0.9)),
                            alignment: TextAlign::Center,
                            overflow: TextOverflow::Ellipsis,
                            color_runs: Box::default(),
                        }),
                    },
                )
                .expect("set_tile_root failed");
        }

        let spec = SceneSpec {
            name: "zone_conflict_two_publishers",
            description: "Two agents (pub_a at z=1, pub_b at z=2) each publishing to \
                          'shared_banner' zone with LatestWins contention. \
                          pub_b's content wins; pub_a's publish is evicted. \
                          Per scene-graph/spec.md lines 185-196.",
            expected_tab_count: 1,
            expected_tile_count: 2,
            has_hit_regions: false,
            has_zones: true,
        };

        (graph, spec)
    }

    /// `zone_orchestrate_then_publish` — orchestrated zone publish sequence:
    /// zone is registered, then content is published to it in order.
    ///
    /// Validates the full zone publish lifecycle per scene-graph/spec.md lines 185-200.
    fn build_zone_orchestrate_then_publish(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph
            .create_tab("Orchestrate", 0)
            .expect("create_tab failed");

        let lease_id = graph.grant_lease_at(
            "agent.orchestrate",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Three zones registered in orchestration order
        let zone_defs = [
            (
                "alert_banner",
                "Alert banner at the top of the display.",
                GeometryPolicy::EdgeAnchored {
                    edge: DisplayEdge::Top,
                    height_pct: 0.05,
                    width_pct: 1.0,
                    margin_px: 0.0,
                },
                ZoneMediaType::ShortTextWithIcon,
                ContentionPolicy::Replace,
            ),
            (
                "notification_area",
                "Notification stack in the top-right corner.",
                GeometryPolicy::Relative {
                    x_pct: 0.75,
                    y_pct: 0.02,
                    width_pct: 0.24,
                    height_pct: 0.30,
                },
                ZoneMediaType::ShortTextWithIcon,
                ContentionPolicy::Stack { max_depth: 5 },
            ),
            (
                "status_bar",
                "Status bar at the bottom edge.",
                GeometryPolicy::EdgeAnchored {
                    edge: DisplayEdge::Bottom,
                    height_pct: 0.04,
                    width_pct: 1.0,
                    margin_px: 0.0,
                },
                ZoneMediaType::KeyValuePairs,
                ContentionPolicy::MergeByKey { max_keys: 16 },
            ),
        ];

        for (name, desc, geom, media_type, contention) in &zone_defs {
            graph.zone_registry.zones.insert(
                name.to_string(),
                ZoneDefinition {
                    id: SceneId::new(),
                    name: name.to_string(),
                    description: desc.to_string(),
                    geometry_policy: *geom,
                    accepted_media_types: vec![*media_type],
                    rendering_policy: RenderingPolicy::default(),
                    contention_policy: *contention,
                    max_publishers: 4,
                    transport_constraint: None,
                    auto_clear_ms: None,
                    ephemeral: false,
                    layer_attachment: LayerAttachment::Chrome,
                },
            );
        }

        // One tile per zone demonstrating the publish sequence
        let zone_tile_configs = [
            (
                "alert_banner",
                Rect::new(0.0, 0.0, self.display_width, self.display_height * 0.05),
                10u32,
            ),
            (
                "notification_area",
                Rect::new(
                    self.display_width * 0.75,
                    self.display_height * 0.02,
                    self.display_width * 0.24,
                    self.display_height * 0.30,
                ),
                20u32,
            ),
            (
                "status_bar",
                Rect::new(
                    0.0,
                    self.display_height * 0.96,
                    self.display_width,
                    self.display_height * 0.04,
                ),
                30u32,
            ),
        ];

        for (zone_name, bounds, z) in &zone_tile_configs {
            let tile_id = graph
                .create_tile(tab_id, "agent.orchestrate", lease_id, *bounds, *z)
                .expect("create_tile failed");
            graph
                .set_tile_root(
                    tile_id,
                    Node {
                        layout: Default::default(),
                        id: SceneId::new(),
                        children: vec![],
                        data: NodeData::TextMarkdown(TextMarkdownNode {
                            content: format!("→ {zone_name}"),
                            bounds: Rect::new(0.0, 0.0, bounds.width, bounds.height),
                            font_size_px: 12.0,
                            font_family: FontFamily::SystemSansSerif,
                            color: Rgba::WHITE,
                            background: Some(Rgba::new(0.1, 0.1, 0.3, 0.85)),
                            alignment: TextAlign::Center,
                            overflow: TextOverflow::Clip,
                            color_runs: Box::default(),
                        }),
                    },
                )
                .expect("set_tile_root failed");
        }

        let spec = SceneSpec {
            name: "zone_orchestrate_then_publish",
            description: "Three zones registered in orchestration order (alert_banner, \
                          notification_area, status_bar) each with a tile publishing to it. \
                          Validates the full zone publish lifecycle per \
                          scene-graph/spec.md lines 185-200.",
            expected_tab_count: 1,
            expected_tile_count: 3,
            has_hit_regions: false,
            has_zones: true,
        };

        (graph, spec)
    }

    /// `zone_disconnect_cleanup` — zone publisher agent disconnects; validates cleanup.
    ///
    /// One agent registers as a zone publisher; then its lease enters Disconnected state.
    /// After the grace period the lease is cleaned up, removing the tile from the zone's
    /// visual footprint. Per lease-governance/spec.md lines 132-155.
    fn build_zone_disconnect_cleanup(&self, clock: ClockMs) -> (SceneGraph, SceneSpec) {
        let mut graph = SceneGraph::new(self.display_width, self.display_height);

        let tab_id = graph
            .create_tab("ZoneCleanup", 0)
            .expect("create_tab failed");

        let pub_lease = graph.grant_lease_at(
            "agent.zone_pub",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let stable_lease = graph.grant_lease_at(
            "agent.stable",
            clock.0,
            300_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Subtitle zone
        graph.zone_registry.zones.insert(
            "subtitle".to_string(),
            ZoneDefinition {
                id: SceneId::new(),
                name: "subtitle".to_string(),
                description: "Subtitle zone for disconnect cleanup test.".to_string(),
                geometry_policy: GeometryPolicy::EdgeAnchored {
                    edge: DisplayEdge::Bottom,
                    height_pct: 0.08,
                    width_pct: 0.80,
                    margin_px: 40.0,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 1,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );

        // Stable tile — unaffected by the publisher disconnect
        let stable_tile = graph
            .create_tile(
                tab_id,
                "agent.stable",
                stable_lease,
                Rect::new(0.0, 0.0, self.display_width, self.display_height * 0.85),
                1,
            )
            .expect("create_tile failed");
        graph
            .set_tile_root(
                stable_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(0.08, 0.1, 0.18, 1.0),
                        bounds: Rect::new(0.0, 0.0, self.display_width, self.display_height * 0.85),
                        radius: None,
                    }),
                },
            )
            .expect("set_tile_root failed");

        // Publisher tile — this agent will disconnect
        let pub_bounds = Rect::new(
            self.display_width * 0.1,
            self.display_height * 0.88,
            self.display_width * 0.8,
            self.display_height * 0.08,
        );
        let pub_tile = graph
            .create_tile(tab_id, "agent.zone_pub", pub_lease, pub_bounds, 10)
            .expect("create_tile failed");
        graph
            .set_tile_root(
                pub_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "Zone publisher — will disconnect".to_string(),
                        bounds: Rect::new(0.0, 0.0, pub_bounds.width, pub_bounds.height),
                        font_size_px: 18.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: Some(Rgba::new(0.0, 0.0, 0.0, 0.75)),
                        alignment: TextAlign::Center,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .expect("set_tile_root failed");

        // Publisher agent disconnects (enters 30,000ms grace period)
        graph
            .disconnect_lease(&pub_lease, clock.0)
            .expect("disconnect_lease failed");

        let spec = SceneSpec {
            name: "zone_disconnect_cleanup",
            description: "Zone publisher (agent.zone_pub) disconnects at clock.0, entering \
                          the 30,000ms grace period. After grace period, the lease + tile \
                          are cleaned up, clearing the zone's visual footprint. \
                          Per lease-governance/spec.md lines 132-155.",
            expected_tab_count: 1,
            expected_tile_count: 2,
            has_hit_regions: false,
            has_zones: true,
        };

        (graph, spec)
    }
}

// ─── Graph extension: grant_lease_at ─────────────────────────────────────────

/// Extension trait adding a clock-injectable variant of `grant_lease` to [`SceneGraph`].
///
/// The core `grant_lease` always calls the real wall clock. For test scenes we need to
/// control the `granted_at_ms` so that expiry behaviour is deterministic.
pub trait SceneGraphTestExt {
    /// Grant a lease using the provided `granted_at_ms` timestamp instead of the wall clock.
    fn grant_lease_at(
        &mut self,
        namespace: &str,
        granted_at_ms: u64,
        ttl_ms: u64,
        capabilities: Vec<Capability>,
    ) -> SceneId;
}

impl SceneGraphTestExt for SceneGraph {
    fn grant_lease_at(
        &mut self,
        namespace: &str,
        granted_at_ms: u64,
        ttl_ms: u64,
        capabilities: Vec<Capability>,
    ) -> SceneId {
        use crate::graph::SceneGraph;
        use crate::types::{Lease, LeaseState, RenewalPolicy, ResourceBudget};

        let id = SceneId::new();
        self.leases.insert(
            id,
            Lease {
                id,
                namespace: namespace.to_string(),
                session_id: SceneId::nil(),
                state: LeaseState::Active,
                priority: 2,
                granted_at_ms,
                ttl_ms,
                renewal_policy: RenewalPolicy::default(),
                capabilities,
                resource_budget: ResourceBudget::default(),
                spatial_budget: Default::default(),
                suspended_at_ms: None,
                ttl_remaining_at_suspend_ms: None,
                disconnected_at_ms: None,
                grace_period_ms: SceneGraph::DEFAULT_GRACE_PERIOD_MS,
            },
        );
        self.version += 1;
        id
    }
}

// ─── Layer 0 invariant checks ─────────────────────────────────────────────────

/// Run all Layer 0 invariants against `graph`. Returns all violations found.
///
/// An empty vec means all invariants pass. A non-empty vec contains diagnostics
/// with structured codes suitable for automated regression reporting.
pub fn assert_layer0_invariants(graph: &SceneGraph) -> Vec<InvariantViolation> {
    let mut violations = Vec::new();

    violations.extend(check_tile_tab_refs(graph));
    violations.extend(check_tile_lease_refs(graph));
    violations.extend(check_tile_bounds_positive(graph));
    violations.extend(check_tile_bounds_within_display(graph));
    violations.extend(check_tile_opacity_range(graph));
    violations.extend(check_node_tile_backlinks(graph));
    violations.extend(check_hit_region_state_consistency(graph));
    violations.extend(check_active_tab_exists(graph));
    violations.extend(check_z_order_unique_per_tab(graph));
    violations.extend(check_lease_namespace_nonempty(graph));
    violations.extend(check_zone_names_nonempty(graph));
    violations.extend(check_zone_name_key_consistency(graph));
    violations.extend(check_version_non_decreasing(graph));

    violations
}

// ─── Individual invariant functions ──────────────────────────────────────────

/// Every tile's `tab_id` must reference a tab that exists in the graph.
pub fn check_tile_tab_refs(graph: &SceneGraph) -> Vec<InvariantViolation> {
    graph
        .tiles
        .values()
        .filter(|t| !graph.tabs.contains_key(&t.tab_id))
        .map(|t| {
            InvariantViolation::new(
                "orphan_tile_tab",
                format!(
                    "tile {} references tab {} which does not exist",
                    t.id, t.tab_id
                ),
            )
        })
        .collect()
}

/// Every tile's `lease_id` must reference a lease that exists in the graph.
pub fn check_tile_lease_refs(graph: &SceneGraph) -> Vec<InvariantViolation> {
    graph
        .tiles
        .values()
        .filter(|t| !graph.leases.contains_key(&t.lease_id))
        .map(|t| {
            InvariantViolation::new(
                "orphan_tile_lease",
                format!(
                    "tile {} references lease {} which does not exist",
                    t.id, t.lease_id
                ),
            )
        })
        .collect()
}

/// Every tile must have positive width and height.
pub fn check_tile_bounds_positive(graph: &SceneGraph) -> Vec<InvariantViolation> {
    graph
        .tiles
        .values()
        .filter(|t| t.bounds.width <= 0.0 || t.bounds.height <= 0.0)
        .map(|t| {
            InvariantViolation::new(
                "tile_bounds_non_positive",
                format!(
                    "tile {} has non-positive bounds: {}×{}",
                    t.id, t.bounds.width, t.bounds.height
                ),
            )
        })
        .collect()
}

/// Every tile's bounds must be fully contained within the display area.
pub fn check_tile_bounds_within_display(graph: &SceneGraph) -> Vec<InvariantViolation> {
    let display = &graph.display_area;
    graph
        .tiles
        .values()
        .filter(|t| !t.bounds.is_within(display))
        .map(|t| {
            InvariantViolation::new(
                "tile_out_of_display",
                format!(
                    "tile {} bounds ({},{} {}×{}) exceed display area ({},{} {}×{})",
                    t.id,
                    t.bounds.x,
                    t.bounds.y,
                    t.bounds.width,
                    t.bounds.height,
                    display.x,
                    display.y,
                    display.width,
                    display.height,
                ),
            )
        })
        .collect()
}

/// Every tile's opacity must be in [0.0, 1.0].
pub fn check_tile_opacity_range(graph: &SceneGraph) -> Vec<InvariantViolation> {
    graph
        .tiles
        .values()
        .filter(|t| !(0.0..=1.0).contains(&t.opacity))
        .map(|t| {
            InvariantViolation::new(
                "tile_opacity_out_of_range",
                format!(
                    "tile {} has opacity {} (must be in [0.0, 1.0])",
                    t.id, t.opacity
                ),
            )
        })
        .collect()
}

/// Every tile's `root_node`, if set, must point to a node that exists in the graph.
/// Additionally, every node listed as a child of another node must exist.
pub fn check_node_tile_backlinks(graph: &SceneGraph) -> Vec<InvariantViolation> {
    let mut violations = Vec::new();

    // Root node backlinks
    for tile in graph.tiles.values() {
        if let Some(root_id) = tile.root_node {
            if !graph.nodes.contains_key(&root_id) {
                violations.push(InvariantViolation::new(
                    "missing_root_node",
                    format!(
                        "tile {} root_node {} does not exist in nodes map",
                        tile.id, root_id
                    ),
                ));
            }
        }
    }

    // Child node backlinks
    for node in graph.nodes.values() {
        for child_id in &node.children {
            if !graph.nodes.contains_key(child_id) {
                violations.push(InvariantViolation::new(
                    "missing_child_node",
                    format!(
                        "node {} child {} does not exist in nodes map",
                        node.id, child_id
                    ),
                ));
            }
        }
    }

    violations
}

/// Every [`HitRegionNode`] must have a corresponding entry in `hit_region_states`.
pub fn check_hit_region_state_consistency(graph: &SceneGraph) -> Vec<InvariantViolation> {
    let mut violations = Vec::new();

    for node in graph.nodes.values() {
        if matches!(node.data, NodeData::HitRegion(_))
            && !graph.hit_region_states.contains_key(&node.id)
        {
            violations.push(InvariantViolation::new(
                "missing_hit_region_state",
                format!(
                    "hit region node {} has no entry in hit_region_states",
                    node.id
                ),
            ));
        }
    }

    // Inverse: every entry in hit_region_states must point to an existing HitRegion node
    for node_id in graph.hit_region_states.keys() {
        match graph.nodes.get(node_id) {
            None => violations.push(InvariantViolation::new(
                "orphan_hit_region_state",
                format!("hit_region_states entry {node_id} has no corresponding node"),
            )),
            Some(node) if !matches!(node.data, NodeData::HitRegion(_)) => {
                violations.push(InvariantViolation::new(
                    "hit_region_state_type_mismatch",
                    format!("hit_region_states entry {node_id} points to a non-HitRegion node"),
                ));
            }
            _ => {}
        }
    }

    violations
}

/// If `active_tab` is `Some(id)`, that id must exist in the tabs map.
pub fn check_active_tab_exists(graph: &SceneGraph) -> Vec<InvariantViolation> {
    if let Some(active_id) = graph.active_tab {
        if !graph.tabs.contains_key(&active_id) {
            return vec![InvariantViolation::new(
                "missing_active_tab",
                format!("active_tab {active_id} does not exist in tabs map"),
            )];
        }
    }
    vec![]
}

/// No two tiles on the same tab may share the same `z_order`.
pub fn check_z_order_unique_per_tab(graph: &SceneGraph) -> Vec<InvariantViolation> {
    use std::collections::HashMap;

    // tab_id → (z_order → tile_id)
    let mut seen: HashMap<SceneId, HashMap<u32, SceneId>> = HashMap::new();
    let mut violations = Vec::new();

    for tile in graph.tiles.values() {
        let z_map = seen.entry(tile.tab_id).or_default();
        if let Some(existing_id) = z_map.insert(tile.z_order, tile.id) {
            violations.push(InvariantViolation::new(
                "duplicate_z_order",
                format!(
                    "tiles {} and {} on tab {} share z_order {}",
                    existing_id, tile.id, tile.tab_id, tile.z_order
                ),
            ));
        }
    }

    violations
}

/// Every lease must have a non-empty namespace.
pub fn check_lease_namespace_nonempty(graph: &SceneGraph) -> Vec<InvariantViolation> {
    graph
        .leases
        .values()
        .filter(|l| l.namespace.is_empty())
        .map(|l| {
            InvariantViolation::new(
                "empty_lease_namespace",
                format!("lease {} has an empty namespace", l.id),
            )
        })
        .collect()
}

/// Every zone definition must have a non-empty name.
pub fn check_zone_names_nonempty(graph: &SceneGraph) -> Vec<InvariantViolation> {
    graph
        .zone_registry
        .zones
        .values()
        .filter(|z| z.name.is_empty())
        .map(|z| {
            InvariantViolation::new(
                "empty_zone_name",
                format!("zone {} has an empty name", z.id),
            )
        })
        .collect()
}

/// The key of each entry in `zone_registry.zones` must match the `name` field of its
/// `ZoneDefinition`. The map is keyed by zone name for O(1) lookup, but the `ZoneDefinition`
/// also carries a `name` field. If they diverge the registry is silently inconsistent.
pub fn check_zone_name_key_consistency(graph: &SceneGraph) -> Vec<InvariantViolation> {
    graph
        .zone_registry
        .zones
        .iter()
        .filter(|(key, zone_def)| **key != zone_def.name)
        .map(|(key, zone_def)| {
            InvariantViolation::new(
                "zone_name_key_mismatch",
                format!(
                    "zone registry key '{}' does not match zone definition name '{}' for zone id {}",
                    key, zone_def.name, zone_def.id
                ),
            )
        })
        .collect()
}

/// The scene version must be ≥ 0 (a trivially always-true structural check included
/// to make the check suite exhaustive; catches accidental integer underflow if
/// version arithmetic changes in future).
pub fn check_version_non_decreasing(graph: &SceneGraph) -> Vec<InvariantViolation> {
    // u64 can never be negative, but we validate the version is reasonable.
    // A fresh graph starts at 0; a mutated one must be > 0.
    // We only flag this if the graph has content but version is still 0.
    let has_content = !graph.tabs.is_empty() || !graph.tiles.is_empty();
    if has_content && graph.version == 0 {
        vec![InvariantViolation::new(
            "version_not_incremented",
            "graph has content but version is still 0 — mutations must increment version",
        )]
    } else {
        vec![]
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scene: empty_scene ───────────────────────────────────────────────

    #[test]
    fn empty_scene_has_correct_structure() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry.build("empty_scene", ClockMs::FIXED).unwrap();

        assert_eq!(graph.tabs.len(), spec.expected_tab_count, "tab count");
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert!(
            graph.active_tab.is_none(),
            "empty scene must have no active tab"
        );
        assert!(graph.leases.is_empty(), "empty scene must have no leases");
        assert!(graph.nodes.is_empty(), "empty scene must have no nodes");
        assert_eq!(graph.version, 0, "empty graph version must be 0");
    }

    // ── Scene: single_tile_solid ──────────────────────────────────────────

    #[test]
    fn single_tile_scene_has_correct_structure() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry.build("single_tile_solid", ClockMs::FIXED).unwrap();

        assert_eq!(graph.tabs.len(), spec.expected_tab_count, "tab count");
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert!(
            graph.active_tab.is_some(),
            "single_tile_solid must have an active tab"
        );
        assert_eq!(
            graph.leases.len(),
            1,
            "single_tile_solid must have exactly one lease"
        );
        assert_eq!(
            graph.nodes.len(),
            1,
            "single_tile_solid must have exactly one node"
        );
    }

    #[test]
    fn single_tile_scene_tile_has_text_root() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry.build("single_tile_solid", ClockMs::FIXED).unwrap();

        let tile = graph.tiles.values().next().unwrap();
        assert!(tile.root_node.is_some(), "tile must have a root node");
        let node = graph.nodes.get(&tile.root_node.unwrap()).unwrap();
        assert!(
            matches!(node.data, NodeData::TextMarkdown(_)),
            "root node must be TextMarkdown"
        );
    }

    #[test]
    fn single_tile_scene_tile_within_display() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry.build("single_tile_solid", ClockMs::FIXED).unwrap();

        let tile = graph.tiles.values().next().unwrap();
        assert!(
            tile.bounds.is_within(&graph.display_area),
            "tile bounds must be within display area"
        );
    }

    // ── Scene: three_tiles_no_overlap ────────────────────────────────────

    #[test]
    fn two_tiles_scene_has_correct_structure() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("three_tiles_no_overlap", ClockMs::FIXED)
            .unwrap();

        assert_eq!(graph.tabs.len(), spec.expected_tab_count, "tab count");
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert_eq!(
            graph.nodes.len(),
            3,
            "three_tiles_no_overlap must have exactly three nodes"
        );
    }

    #[test]
    fn two_tiles_scene_has_one_hit_region() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("three_tiles_no_overlap", ClockMs::FIXED)
            .unwrap();

        let hit_region_count = graph
            .nodes
            .values()
            .filter(|n| matches!(n.data, NodeData::HitRegion(_)))
            .count();

        assert_eq!(
            hit_region_count, 1,
            "three_tiles_no_overlap must have exactly one hit region node"
        );
        assert_eq!(
            graph.hit_region_states.len(),
            1,
            "hit_region_states must have one entry"
        );
        assert!(
            spec.has_hit_regions,
            "spec must declare has_hit_regions = true"
        );
    }

    #[test]
    fn two_tiles_scene_tiles_do_not_overlap() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry
            .build("three_tiles_no_overlap", ClockMs::FIXED)
            .unwrap();

        let tiles: Vec<_> = graph.tiles.values().collect();
        assert_eq!(tiles.len(), 3, "expected exactly 3 tiles");
        // Verify all pairs of tiles are non-overlapping
        for i in 0..tiles.len() {
            for j in (i + 1)..tiles.len() {
                assert!(
                    !tiles[i].bounds.intersects(&tiles[j].bounds),
                    "tiles must not overlap: tile[{i}] {:?} vs tile[{j}] {:?}",
                    tiles[i].bounds,
                    tiles[j].bounds,
                );
            }
        }
    }

    #[test]
    fn two_tiles_scene_z_orders_are_unique() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry
            .build("three_tiles_no_overlap", ClockMs::FIXED)
            .unwrap();

        let mut z_orders: Vec<u32> = graph.tiles.values().map(|t| t.z_order).collect();
        z_orders.sort_unstable();
        let before = z_orders.len();
        z_orders.dedup();
        assert_eq!(z_orders.len(), before, "all z_orders must be unique");
    }

    // ── Scene: max_tiles_stress ───────────────────────────────────────────

    #[test]
    fn max_tiles_scene_has_correct_structure() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry.build("max_tiles_stress", ClockMs::FIXED).unwrap();

        assert_eq!(graph.tabs.len(), spec.expected_tab_count, "tab count");
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        // Each tile has exactly one root node
        assert_eq!(
            graph.nodes.len(),
            spec.expected_tile_count,
            "node count must equal tile count (one root per tile)"
        );
    }

    #[test]
    fn max_tiles_scene_all_tiles_within_display() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry.build("max_tiles_stress", ClockMs::FIXED).unwrap();

        let out_of_bounds: Vec<_> = graph
            .tiles
            .values()
            .filter(|t| !t.bounds.is_within(&graph.display_area))
            .collect();

        assert!(
            out_of_bounds.is_empty(),
            "{} tile(s) extend outside the display area",
            out_of_bounds.len()
        );
    }

    #[test]
    fn max_tiles_scene_z_orders_all_unique() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry.build("max_tiles_stress", ClockMs::FIXED).unwrap();

        let mut z_orders: Vec<u32> = graph.tiles.values().map(|t| t.z_order).collect();
        z_orders.sort_unstable();
        let before = z_orders.len();
        z_orders.dedup();
        assert_eq!(z_orders.len(), before, "all z_orders must be unique");
    }

    // ── Scene: overlapping_tiles_zorder ──────────────────────────────────

    #[test]
    fn overlapping_tiles_zorder_has_correct_structure() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("overlapping_tiles_zorder", ClockMs::FIXED)
            .unwrap();
        assert_eq!(graph.tabs.len(), spec.expected_tab_count, "tab count");
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert_eq!(spec.expected_tile_count, 3, "must have 3 tiles");
    }

    #[test]
    fn overlapping_tiles_zorder_z_orders_unique() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry
            .build("overlapping_tiles_zorder", ClockMs::FIXED)
            .unwrap();
        let mut z_orders: Vec<u32> = graph.tiles.values().map(|t| t.z_order).collect();
        z_orders.sort_unstable();
        let before = z_orders.len();
        z_orders.dedup();
        assert_eq!(z_orders.len(), before, "z_orders must be unique");
    }

    // ── Scene: overlay_transparency ───────────────────────────────────────

    #[test]
    fn overlay_transparency_has_correct_structure() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("overlay_transparency", ClockMs::FIXED)
            .unwrap();
        assert_eq!(graph.tabs.len(), spec.expected_tab_count, "tab count");
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert_eq!(spec.expected_tile_count, 2, "must have 2 tiles");
    }

    #[test]
    fn overlay_transparency_overlay_tile_has_sub_unit_opacity() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry
            .build("overlay_transparency", ClockMs::FIXED)
            .unwrap();
        let opacities: Vec<f32> = graph.tiles.values().map(|t| t.opacity).collect();
        assert!(
            opacities.iter().any(|&o| o < 1.0),
            "at least one tile must have opacity < 1.0 for transparency test"
        );
    }

    // ── Scene: tab_switch ─────────────────────────────────────────────────

    #[test]
    fn tab_switch_has_two_tabs() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry.build("tab_switch", ClockMs::FIXED).unwrap();
        assert_eq!(graph.tabs.len(), spec.expected_tab_count, "tab count");
        assert_eq!(spec.expected_tab_count, 2, "must have 2 tabs");
    }

    #[test]
    fn tab_switch_active_tab_is_tab_b() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry.build("tab_switch", ClockMs::FIXED).unwrap();
        // Active tab should be tab B (which has 2 tiles)
        let active_id = graph.active_tab.expect("must have active tab");
        let tiles_on_active: Vec<_> = graph
            .tiles
            .values()
            .filter(|t| t.tab_id == active_id)
            .collect();
        assert_eq!(tiles_on_active.len(), 2, "active tab (B) must have 2 tiles");
    }

    // ── Scene: lease_expiry ───────────────────────────────────────────────

    #[test]
    fn lease_expiry_lease_is_active_at_build_time() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry.build("lease_expiry", ClockMs::FIXED).unwrap();
        use crate::types::LeaseState;
        let lease = graph.leases.values().next().expect("must have a lease");
        assert_eq!(
            lease.state,
            LeaseState::Active,
            "lease must be ACTIVE at build time"
        );
        assert_eq!(lease.ttl_ms, 1, "TTL must be 1ms");
    }

    // ── Scene: input_highlight ────────────────────────────────────────────

    #[test]
    fn input_highlight_has_hit_region() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry.build("input_highlight", ClockMs::FIXED).unwrap();
        assert!(
            spec.has_hit_regions,
            "spec must declare has_hit_regions = true"
        );
        let hit_count = graph
            .nodes
            .values()
            .filter(|n| matches!(n.data, NodeData::HitRegion(_)))
            .count();
        assert_eq!(hit_count, 1, "must have exactly one hit region node");
    }

    #[test]
    fn input_highlight_hit_region_accepts_focus_and_pointer() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry.build("input_highlight", ClockMs::FIXED).unwrap();
        let hit_node = graph
            .nodes
            .values()
            .find(|n| matches!(n.data, NodeData::HitRegion(_)))
            .expect("must have a hit region node");
        if let NodeData::HitRegion(hr) = &hit_node.data {
            assert!(hr.accepts_focus, "hit region must accept focus");
            assert!(hr.accepts_pointer, "hit region must accept pointer");
        }
    }

    // ── Scene: coalesced_dashboard ────────────────────────────────────────

    #[test]
    fn coalesced_dashboard_has_twelve_tiles() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("coalesced_dashboard", ClockMs::FIXED)
            .unwrap();
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert_eq!(spec.expected_tile_count, 12, "must have 12 tiles");
    }

    #[test]
    fn coalesced_dashboard_all_tiles_within_display() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry
            .build("coalesced_dashboard", ClockMs::FIXED)
            .unwrap();
        let out_of_bounds: Vec<_> = graph
            .tiles
            .values()
            .filter(|t| !t.bounds.is_within(&graph.display_area))
            .collect();
        assert!(
            out_of_bounds.is_empty(),
            "{} tile(s) outside display area",
            out_of_bounds.len()
        );
    }

    // ── Scene: three_agents_contention ────────────────────────────────────

    #[test]
    fn three_agents_contention_has_three_distinct_namespaces() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("three_agents_contention", ClockMs::FIXED)
            .unwrap();
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        let mut namespaces: Vec<&str> =
            graph.tiles.values().map(|t| t.namespace.as_str()).collect();
        namespaces.sort_unstable();
        namespaces.dedup();
        assert_eq!(namespaces.len(), 3, "must have 3 distinct namespaces");
    }

    #[test]
    fn three_agents_contention_lease_priorities_are_distinct() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry
            .build("three_agents_contention", ClockMs::FIXED)
            .unwrap();
        let mut priorities: Vec<u8> = graph.leases.values().map(|l| l.priority).collect();
        priorities.sort_unstable();
        priorities.dedup();
        assert_eq!(priorities.len(), 3, "must have 3 distinct lease priorities");
    }

    // ── Scene: overlay_passthrough_regions ────────────────────────────────

    #[test]
    fn overlay_passthrough_regions_has_correct_structure() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("overlay_passthrough_regions", ClockMs::FIXED)
            .unwrap();
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert_eq!(spec.expected_tile_count, 3, "must have 3 tiles");
        assert!(
            spec.has_hit_regions,
            "spec must declare has_hit_regions = true"
        );
    }

    #[test]
    fn overlay_passthrough_regions_has_passthrough_tile() {
        use crate::types::InputMode;
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry
            .build("overlay_passthrough_regions", ClockMs::FIXED)
            .unwrap();
        let passthrough_tiles: Vec<_> = graph
            .tiles
            .values()
            .filter(|t| t.input_mode == InputMode::Passthrough)
            .collect();
        assert_eq!(
            passthrough_tiles.len(),
            1,
            "must have exactly 1 passthrough tile"
        );
    }

    // ── Scene: disconnect_reclaim_multiagent ──────────────────────────────

    #[test]
    fn disconnect_reclaim_multiagent_has_correct_structure() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("disconnect_reclaim_multiagent", ClockMs::FIXED)
            .unwrap();
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert_eq!(spec.expected_tile_count, 4, "must have 4 tiles (2+1+1)");
    }

    #[test]
    fn disconnect_reclaim_multiagent_all_agents_start_active() {
        use crate::types::LeaseState;
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry
            .build("disconnect_reclaim_multiagent", ClockMs::FIXED)
            .unwrap();
        for ns in ["agent.one", "agent.two", "agent.three"] {
            let lease = graph
                .leases
                .values()
                .find(|l| l.namespace == ns)
                .unwrap_or_else(|| panic!("must have {ns} lease"));
            assert_eq!(
                lease.state,
                LeaseState::Active,
                "{ns} lease must start Active (tests drive disconnection)"
            );
        }
        // agent.one has 2 tiles; agent.two and agent.three each have 1
        let one_tiles = graph
            .tiles
            .values()
            .filter(|t| t.namespace == "agent.one")
            .count();
        let two_tiles = graph
            .tiles
            .values()
            .filter(|t| t.namespace == "agent.two")
            .count();
        let three_tiles = graph
            .tiles
            .values()
            .filter(|t| t.namespace == "agent.three")
            .count();
        assert_eq!(one_tiles, 2, "agent.one must have 2 tiles");
        assert_eq!(two_tiles, 1, "agent.two must have 1 tile");
        assert_eq!(three_tiles, 1, "agent.three must have 1 tile");
    }

    // ── Scene: chatty_dashboard_touch ─────────────────────────────────────

    #[test]
    fn chatty_dashboard_touch_has_fifty_tiles() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("chatty_dashboard_touch", ClockMs::FIXED)
            .unwrap();
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert_eq!(spec.expected_tile_count, 50, "must have 50 tiles");
    }

    #[test]
    fn chatty_dashboard_touch_all_tiles_are_hit_regions() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("chatty_dashboard_touch", ClockMs::FIXED)
            .unwrap();
        assert!(
            spec.has_hit_regions,
            "spec must declare has_hit_regions = true"
        );
        let hit_count = graph
            .nodes
            .values()
            .filter(|n| matches!(n.data, NodeData::HitRegion(_)))
            .count();
        assert_eq!(
            hit_count, 50,
            "all 50 tiles must have a hit region root node"
        );
    }

    // ── Scene: zone_publish_subtitle ──────────────────────────────────────

    #[test]
    fn zone_publish_subtitle_has_subtitle_zone() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("zone_publish_subtitle", ClockMs::FIXED)
            .unwrap();
        assert!(spec.has_zones, "spec must declare has_zones = true");
        assert!(
            graph.zone_registry.zones.contains_key("subtitle"),
            "must have subtitle zone"
        );
    }

    // ── Scene: zone_reject_wrong_type ─────────────────────────────────────

    #[test]
    fn zone_reject_wrong_type_has_typed_zone() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("zone_reject_wrong_type", ClockMs::FIXED)
            .unwrap();
        assert!(spec.has_zones, "spec must declare has_zones = true");
        let zone = graph
            .zone_registry
            .zones
            .get("typed_zone")
            .expect("must have typed_zone");
        assert_eq!(
            zone.accepted_media_types,
            vec![ZoneMediaType::StreamText],
            "typed_zone must accept only StreamText"
        );
    }

    // ── Scene: zone_conflict_two_publishers ───────────────────────────────

    #[test]
    fn zone_conflict_two_publishers_has_correct_structure() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("zone_conflict_two_publishers", ClockMs::FIXED)
            .unwrap();
        assert_eq!(graph.tiles.len(), spec.expected_tile_count, "tile count");
        assert!(spec.has_zones, "spec must declare has_zones = true");
        assert!(
            graph.zone_registry.zones.contains_key("shared_banner"),
            "must have shared_banner zone"
        );
    }

    #[test]
    fn zone_conflict_two_publishers_contention_is_latest_wins() {
        let registry = TestSceneRegistry::new();
        let (graph, _spec) = registry
            .build("zone_conflict_two_publishers", ClockMs::FIXED)
            .unwrap();
        let zone = graph.zone_registry.zones.get("shared_banner").unwrap();
        assert_eq!(
            zone.contention_policy,
            ContentionPolicy::LatestWins,
            "shared_banner must use LatestWins contention"
        );
    }

    // ── Scene: zone_orchestrate_then_publish ──────────────────────────────

    #[test]
    fn zone_orchestrate_then_publish_has_three_zones() {
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("zone_orchestrate_then_publish", ClockMs::FIXED)
            .unwrap();
        assert!(spec.has_zones, "spec must declare has_zones = true");
        assert_eq!(graph.zone_registry.zones.len(), 3, "must have 3 zones");
        for zone_name in &["alert_banner", "notification_area", "status_bar"] {
            assert!(
                graph.zone_registry.zones.contains_key(*zone_name),
                "must have zone '{zone_name}'"
            );
        }
    }

    // ── Scene: zone_disconnect_cleanup ────────────────────────────────────

    #[test]
    fn zone_disconnect_cleanup_publisher_is_disconnected() {
        use crate::types::LeaseState;
        let registry = TestSceneRegistry::new();
        let (graph, spec) = registry
            .build("zone_disconnect_cleanup", ClockMs::FIXED)
            .unwrap();
        assert!(spec.has_zones, "spec must declare has_zones = true");
        let pub_lease = graph
            .leases
            .values()
            .find(|l| l.namespace == "agent.zone_pub")
            .expect("must have agent.zone_pub lease");
        assert_eq!(
            pub_lease.state,
            LeaseState::Orphaned,
            "zone publisher must be in Orphaned state"
        );
    }

    // ── Registry meta ─────────────────────────────────────────────────────

    #[test]
    fn unknown_scene_name_returns_none() {
        let registry = TestSceneRegistry::new();
        assert!(registry.build("does_not_exist", ClockMs::FIXED).is_none());
    }

    #[test]
    fn all_registered_names_build_successfully() {
        let registry = TestSceneRegistry::new();
        for name in TestSceneRegistry::scene_names() {
            let result = registry.build(name, ClockMs::FIXED);
            assert!(result.is_some(), "scene '{name}' failed to build");
        }
    }

    #[test]
    fn all_registered_scenes_pass_layer0_invariants() {
        let registry = TestSceneRegistry::new();
        let mut all_violations: Vec<String> = Vec::new();

        for name in TestSceneRegistry::scene_names() {
            let (graph, _spec) = registry.build(name, ClockMs::FIXED).unwrap();
            let violations = assert_layer0_invariants(&graph);
            for v in &violations {
                all_violations.push(format!("[{name}] {v}"));
            }
        }

        if !all_violations.is_empty() {
            panic!(
                "Layer 0 violations across all scenes:\n{}",
                all_violations.join("\n")
            );
        }
    }

    // ── Clock injection ───────────────────────────────────────────────────

    #[test]
    fn clock_injection_controls_lease_granted_at() {
        let registry = TestSceneRegistry::new();
        let t1 = ClockMs(1_000_000_000_000);
        let t2 = ClockMs(2_000_000_000_000);

        let (graph1, _) = registry.build("single_tile_solid", t1).unwrap();
        let (graph2, _) = registry.build("single_tile_solid", t2).unwrap();

        let lease1 = graph1.leases.values().next().unwrap();
        let lease2 = graph2.leases.values().next().unwrap();

        assert_eq!(
            lease1.granted_at_ms, t1.0,
            "lease1 granted_at_ms should match clock t1"
        );
        assert_eq!(
            lease2.granted_at_ms, t2.0,
            "lease2 granted_at_ms should match clock t2"
        );
    }

    #[test]
    fn clock_offset_helper_adds_correctly() {
        let base = ClockMs(1_000_000_000_000);
        let offset = base.offset(5_000);
        assert_eq!(offset.0, 1_000_000_005_000);
    }

    // ── Individual invariant checks ───────────────────────────────────────

    #[test]
    fn invariant_detects_orphan_tile_tab() {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        // We can't create a tile with a non-existent tab via the safe API, so simulate by
        // creating a valid tile and then removing the tab to orphan it.
        let lease_id = graph.grant_lease(
            "test",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let real_tab = graph.create_tab("Temp", 0).unwrap();
        let _tile_id = graph
            .create_tile(
                real_tab,
                "test",
                lease_id,
                Rect::new(0.0, 0.0, 100.0, 100.0),
                1,
            )
            .unwrap();
        graph.tabs.remove(&real_tab); // orphan the tile

        let violations = check_tile_tab_refs(&graph);
        assert!(!violations.is_empty(), "expected orphan_tile_tab violation");
        assert_eq!(violations[0].code, "orphan_tile_tab");
    }

    #[test]
    fn invariant_detects_orphan_tile_lease() {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let tab_id = graph.create_tab("Main", 0).unwrap();
        let lease_id = graph.grant_lease(
            "test",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        graph
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(0.0, 0.0, 100.0, 100.0),
                1,
            )
            .unwrap();
        // Remove the lease to orphan the tile
        graph.leases.remove(&lease_id);

        let violations = check_tile_lease_refs(&graph);
        assert!(
            !violations.is_empty(),
            "expected orphan_tile_lease violation"
        );
        assert_eq!(violations[0].code, "orphan_tile_lease");
    }

    #[test]
    fn invariant_detects_duplicate_z_order() {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let tab_id = graph.create_tab("Main", 0).unwrap();
        let lease_id = graph.grant_lease(
            "test",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        graph
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(0.0, 0.0, 100.0, 100.0),
                5,
            )
            .unwrap();
        graph
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(200.0, 0.0, 100.0, 100.0),
                5,
            )
            .unwrap();

        let violations = check_z_order_unique_per_tab(&graph);
        assert!(
            !violations.is_empty(),
            "expected duplicate_z_order violation"
        );
        assert_eq!(violations[0].code, "duplicate_z_order");
    }

    #[test]
    fn invariant_detects_missing_active_tab() {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        // Set active_tab to a non-existent ID
        graph.active_tab = Some(SceneId::new());

        let violations = check_active_tab_exists(&graph);
        assert!(
            !violations.is_empty(),
            "expected missing_active_tab violation"
        );
        assert_eq!(violations[0].code, "missing_active_tab");
    }

    #[test]
    fn invariant_detects_zone_name_key_mismatch() {
        use crate::types::ZoneDefinition;

        let mut graph = SceneGraph::new(1920.0, 1080.0);
        // Insert a zone where the map key does not match the definition's name field
        graph.zone_registry.zones.insert(
            "wrong_key".to_string(),
            ZoneDefinition {
                id: SceneId::new(),
                name: "correct_name".to_string(),
                description: "Intentionally mismatched key/name.".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 1.0,
                    height_pct: 1.0,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 1,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );

        let violations = check_zone_name_key_consistency(&graph);
        assert!(
            !violations.is_empty(),
            "expected zone_name_key_mismatch violation"
        );
        assert_eq!(violations[0].code, "zone_name_key_mismatch");
    }

    #[test]
    fn invariant_detects_missing_hit_region_state() {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let tab_id = graph.create_tab("Main", 0).unwrap();
        let lease_id = graph.grant_lease(
            "test",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let tile_id = graph
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 300.0),
                1,
            )
            .unwrap();

        let hr_node = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 0.0, 100.0, 50.0),
                interaction_id: "btn".into(),
                accepts_focus: true,
                accepts_pointer: true,
                ..Default::default()
            }),
        };
        let node_id = hr_node.id;
        graph.set_tile_root(tile_id, hr_node).unwrap();
        // Simulate missing state entry
        graph.hit_region_states.remove(&node_id);

        let violations = check_hit_region_state_consistency(&graph);
        assert!(
            !violations.is_empty(),
            "expected missing_hit_region_state violation"
        );
        assert_eq!(violations[0].code, "missing_hit_region_state");
    }

    // ── 800×600 display regression tests (pixel readback resolution) ──────────
    //
    // These tests guard against BoundsOutOfRange panics when scenes are built
    // at the 800×600 resolution used by the pixel readback tests in
    // `examples/vertical_slice/tests/budget_assertions.rs` and
    // `crates/tze_hud_runtime/tests/pixel_readback.rs`.
    //
    // Previously several scene builders used hardcoded 1920×1080 coordinates
    // that exceeded the 800×600 display bounds.

    #[test]
    fn all_scenes_build_at_800x600() {
        let registry = TestSceneRegistry::with_display(800.0, 600.0);
        for name in TestSceneRegistry::scene_names() {
            let result = registry.build(name, ClockMs::FIXED);
            assert!(
                result.is_some(),
                "scene '{name}' must build at 800×600 display"
            );
        }
    }

    #[test]
    fn all_scenes_tiles_within_bounds_at_800x600() {
        let registry = TestSceneRegistry::with_display(800.0, 600.0);
        for name in TestSceneRegistry::scene_names() {
            let (graph, _spec) = registry
                .build(name, ClockMs::FIXED)
                .unwrap_or_else(|| panic!("scene '{name}' must build"));
            let out_of_bounds: Vec<_> = graph
                .tiles
                .values()
                .filter(|t| !t.bounds.is_within(&graph.display_area))
                .map(|t| format!("{:?}", t.bounds))
                .collect();
            assert!(
                out_of_bounds.is_empty(),
                "scene '{}' at 800×600: {} tile(s) outside display area: {:?}",
                name,
                out_of_bounds.len(),
                out_of_bounds
            );
        }
    }
}
