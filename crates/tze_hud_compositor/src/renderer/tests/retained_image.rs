use super::*;

#[tokio::test]
async fn test_static_image_node_renders_placeholder_quad() {
    // The static image placeholder renders a warm-gray outer quad ~[0.55, 0.50, 0.45].
    // In sRGB output the linear values are gamma-compressed.
    // We just verify that *some* non-background pixels appear in the expected warm range.
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    // RS-4: StaticImageNode uses resource_id + decoded_bytes; no raw blob embedded.
    let resource_id = ResourceId::of(b"8x8 test image placeholder");
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 8,
            height: 8,
            decoded_bytes: 8 * 8 * 4,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
        }),
    };

    // Resource must be registered before set_tile_root inserts the StaticImageNode tree.
    let mut scene = SceneGraph::new(256.0, 256.0);
    scene.register_resource(resource_id);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene.set_tile_root(tile_id, node).unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    // The background clear color is ~[0.05, 0.05, 0.1] in linear; tile bg is [0.05,0.05,0.05].
    // The placeholder outer quad is warm gray [0.55, 0.50, 0.45] in linear.
    // In sRGB this is approximately [198, 188, 176]. We look for pixels brighter than 150 in
    // all three channels to confirm the quad was rendered (not just the dark background).
    let any_warm_pixel = pixels
        .chunks(4)
        .any(|p| p[0] > 150 && p[1] > 140 && p[2] > 130);
    assert!(
        any_warm_pixel,
        "expected warm-gray placeholder pixels from StaticImageNode"
    );
}

/// A real recovered window-surface lifecycle outcome must invalidate the
/// compositor-private retained proof lane. The next observed full frame is a
/// structured diagnostic, never a proportional-render certification.
#[tokio::test]
async fn reconfigured_surface_recovery_is_a_non_passing_retained_diagnostic() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1_000, 500).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (mut scene, first_tile_id, first_root_id) = canonical_retained_scene();
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    // Seed a genuine retained baseline, then make the one direct text change
    // that normally qualifies for a proportional retained render. The recovery
    // outcome below must preempt that otherwise eligible path.
    compositor.render_frame_headless(&mut scene, &surface);
    let baseline_diagnostic = compositor
        .take_change_efficiency_diagnostic()
        .expect("canonical baseline emits a surface-creation diagnostic");
    assert_eq!(
        baseline_diagnostic
            .full_surface_invalidation
            .expect("baseline invalidation metadata")
            .reason,
        tze_hud_telemetry::FullSurfaceInvalidationReason::SurfaceCreation
    );
    let mut changed_text = match &scene.nodes[&first_root_id].data {
        NodeData::TextMarkdown(text) => text.clone(),
        other => panic!("expected canonical text root, got {other:?}"),
    };
    changed_text.content = "BA".into();
    scene
        .update_node_content(
            first_tile_id,
            first_root_id,
            NodeData::TextMarkdown(changed_text),
        )
        .expect("controlled canonical text mutation");
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    // This is the exact compositor-private outcome emitted after a real
    // Lost/Outdated surface acquire has been reconfigured, not a model-only
    // retained-planner signal.
    compositor.record_surface_recovery_outcome(SurfaceRecoveryOutcome::Reconfigured {
        trigger: SurfaceAcquireFailure::Lost,
    });
    compositor.render_frame_headless(&mut scene, &surface);

    assert!(
        compositor.take_change_efficiency_capture().is_none(),
        "a recovered surface must not leave a scoped retained capture available"
    );
    let diagnostic = compositor
        .take_change_efficiency_diagnostic()
        .expect("surface recovery emits a structured retained diagnostic");
    let report = diagnostic.validate();
    assert!(!report.passed, "{report:#?}");
    assert_eq!(
        report.status,
        tze_hud_telemetry::ChangeEfficiencyValidationStatus::DiagnosticFullSurface,
        "{report:#?}"
    );
    assert_eq!(
        diagnostic
            .full_surface_invalidation
            .expect("full-surface diagnostic metadata")
            .reason,
        tze_hud_telemetry::FullSurfaceInvalidationReason::DeviceRecovery
    );
}

#[tokio::test]
async fn test_static_image_node_composited_with_other_nodes() {
    // Render a scene with both a SolidColor node and a StaticImage node in adjacent tiles.
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(512, 256).await);

    let mut scene = SceneGraph::new(512.0, 256.0);
    // Resource must be registered before set_tile_root inserts a StaticImageNode tree.
    let static_image_resource_id = ResourceId::of(b"8x8 green placeholder");
    scene.register_resource(static_image_resource_id);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    // Left tile: red solid color
    let left_tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root(
            left_tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(1.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
                    radius: None,
                }),
            },
        )
        .unwrap();

    // Right tile: static image
    // RS-4: StaticImageNode uses resource_id + decoded_bytes; no raw blob embedded.
    let right_tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(256.0, 0.0, 256.0, 256.0),
            2,
        )
        .unwrap();
    scene
        .set_tile_root(
            right_tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::StaticImage(StaticImageNode {
                    resource_id: static_image_resource_id,
                    width: 8,
                    height: 8,
                    decoded_bytes: 8 * 8 * 4,
                    fit_mode: ImageFitMode::Cover,
                    bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
                }),
            },
        )
        .unwrap();

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 512 * 256 * 4, "pixel buffer size mismatch");
    // Just verify the frame completed without panic and returned the expected buffer size.
}
