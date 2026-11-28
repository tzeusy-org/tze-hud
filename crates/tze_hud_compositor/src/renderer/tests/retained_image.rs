use super::*;
use crate::test_pixels::{assert_pixel_color, pixel_at};

#[tokio::test]
async fn test_static_image_node_composited_with_other_nodes() {
    let (width, height) = (512, 256);
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(width, height).await);
    let mut scene = SceneGraph::new(width as f32, height as f32);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("agent", 60_000);

    // Register the scene resource without uploading bytes to the compositor.
    let resource_id = ResourceId::of(b"missing 8x8 retained image");
    scene.register_resource(resource_id);
    let bounds = Rect::new(0.0, 0.0, 256.0, 256.0);
    let red = solid_node(bounds, Rgba::new(1.0, 0.0, 0.0, 1.0), None);
    let image = leaf(NodeData::StaticImage(StaticImageNode {
        resource_id,
        width: 8,
        height: 8,
        decoded_bytes: 8 * 8 * 4,
        fit_mode: ImageFitMode::Cover,
        bounds,
    }));

    for (x, z, node) in [(0.0, 1, red), (256.0, 2, image)] {
        let tile = scene
            .create_tile(tab, "agent", lease, Rect::new(x, 0.0, 256.0, 256.0), z)
            .unwrap();
        scene.set_tile_root(tile, node).unwrap();
    }

    prime_text(&mut compositor, &scene);
    let pixels = render_pixels(&mut compositor, &mut scene, &surface);
    assert_eq!(pixels.len(), (width * height * 4) as usize);
    assert_eq!(pixel_at(&pixels, width, 128, 128), [255, 0, 0, 255]);

    // Use the same sRGB swatches and tolerance as tile_static_image_fit_modes.
    for (x, rgba, label) in [
        (258, [196, 188, 179, 255], "placeholder frame"),
        (384, [225, 218, 211, 255], "placeholder inset"),
    ] {
        assert_pixel_color(&pixels, width, x, 128, rgba, 12, label).unwrap();
    }
}

fn render_resident_pixels(
    compositor: &mut Compositor,
    scene: &mut SceneGraph,
    surface: &HeadlessSurface,
) -> Vec<u8> {
    prime_text(compositor, scene);
    render_pixels(compositor, scene, surface)
}

struct RasterAuthority(tze_hud_scene::config::agents::PairingAuthorization);

impl tze_hud_resource::upload::RasterUploadAuthority for RasterAuthority {
    fn with_live_grants(
        &self,
        commit: &mut dyn FnMut(&mut std::collections::HashSet<[u8; 32]>),
    ) -> bool {
        self.0.with_live_grants(|grants| commit(grants)).is_some()
    }
}

async fn resident_raster_scene(
    capacity: u64,
) -> (
    SceneGraph,
    tze_hud_resource::ResourceStore,
    tze_hud_resource::ResidentLedger,
    tze_hud_scene::config::AgentDirectory,
    std::sync::Arc<ResidentRasterSource>,
) {
    use tze_hud_resource::{
        AgentBudget, ResidentLedger, ResidentLedgerLimits, ResourceStore, ResourceStoreConfig,
        ResourceType, UploadId, UploadStartRequest,
    };
    let mut directory = tze_hud_scene::config::AgentDirectory::default();
    directory.insert(
        "raster-a",
        tze_hud_scene::config::agents::hash_psk("fixture-key"),
        vec!["*".into()],
    );
    let authorization = directory
        .resolve("fixture-key", "raster-a")
        .unwrap()
        .authorization;
    let ledger = ResidentLedger::new(ResidentLedgerLimits {
        aggregate_bytes: capacity,
        resource_bytes: capacity,
        widget_source_bytes: 0,
        widget_raster_bytes: 0,
        font_bytes: 0,
    });
    let store =
        ResourceStore::new_with_resident_ledger(ResourceStoreConfig::default(), ledger.clone());
    let bytes = vec![255, 0, 0, 255, 0, 255, 0, 255];
    let id = tze_hud_resource::ResourceId::from_content(&bytes);
    store
        .handle_resident_upload_start(
            UploadStartRequest {
                agent_namespace: "raster-a".into(),
                agent_capabilities: vec!["upload_resource".into()],
                agent_budget: AgentBudget {
                    texture_bytes_total_limit: 0,
                    texture_bytes_total_used: 0,
                },
                upload_id: UploadId::from_bytes([1; 16]),
                resource_type: ResourceType::ImageRgba8,
                expected_hash: *id.as_bytes(),
                total_size: bytes.len(),
                inline_data: bytes,
                width: 2,
                height: 1,
            },
            std::sync::Arc::new(RasterAuthority(authorization.clone())),
        )
        .await
        .unwrap()
        .unwrap();
    let raster = store.validated_raster(&id).unwrap();
    let source = std::sync::Arc::new(ResidentRasterSource::new(
        ResourceId::from_bytes(*id.as_bytes()),
        raster.width,
        raster.height,
        raster.pixels.clone(),
        authorization,
    ));
    let mut scene = SceneGraph::new(128.0, 64.0);
    let defaults = ZoneRegistry::with_defaults();
    for (name, x) in [("resident", 0.0), ("generic", 0.5)] {
        let mut zone = defaults.get_by_name("ambient-background").unwrap().clone();
        zone.id = SceneId::new();
        zone.name = name.into();
        zone.geometry_policy = GeometryPolicy::Relative {
            x_pct: x,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 1.0,
        };
        zone.layer_attachment = LayerAttachment::Content;
        zone.auto_clear_ms = None;
        zone.rendering_policy.transition_in_ms = None;
        zone.rendering_policy.transition_out_ms = None;
        scene.register_zone(zone);
    }
    let publication = tze_hud_scene::Publication::Zone {
        zone_name: "resident".into(),
        content: ZoneContent::StaticImage(source.resource_id()),
        merge_key: None,
        expires_at_wall_us: None,
        content_classification: None,
        breakpoints: Vec::new(),
        held: false,
    };
    scene
        .publish(
            &tze_hud_scene::Publisher {
                resident_raster: Some(source.clone()),
                ..tze_hud_scene::Publisher::namespace("raster-a")
            },
            &publication,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "generic",
            ZoneContent::StaticImage(source.resource_id()),
            "mcp-fixture",
            None,
            None,
            None,
        )
        .unwrap();
    (scene, store, ledger, directory, source)
}

#[tokio::test]
async fn resident_raster_uses_store_pixels_without_generic_image_authority() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(128, 64).await);
    let (mut scene, store, ledger, directory, source) = resident_raster_scene(64).await;
    compositor.set_resident_ledger(ledger.clone());
    // Upload-before-publication and zero compositor registrations: the sole CPU
    // charge is the store's 8 bytes; the renderer reserves only 8 GPU bytes.
    let pixels = render_resident_pixels(&mut compositor, &mut scene, &surface);
    assert_pixel_color(
        &pixels,
        128,
        8,
        32,
        [255, 0, 0, 255],
        8,
        "resident left red",
    )
    .unwrap();
    assert_pixel_color(
        &pixels,
        128,
        56,
        32,
        [0, 255, 0, 255],
        8,
        "resident right green",
    )
    .unwrap();
    assert_ne!(
        pixel_at(&pixels, 128, 72, 32),
        [255, 0, 0, 255],
        "generic same hash cannot borrow resident texture"
    );
    assert!(compositor.image_bytes.is_empty());
    assert!(compositor.image_texture_cache.is_empty());
    assert_eq!(compositor.resident_image_texture_cache.len(), 1);
    assert_eq!(ledger.snapshot().resource_bytes, 16);
    assert_eq!(ledger.snapshot().allocation_count, 2);
    let retained = store
        .validated_raster(&tze_hud_resource::ResourceId::from_bytes(
            *source.resource_id().as_bytes(),
        ))
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(
        &source.pixels_if_authorized().unwrap().0,
        &retained.pixels
    ));
    scene
        .clear_zone_and_cancel_pending("resident", "raster-a")
        .unwrap();
    render_resident_pixels(&mut compositor, &mut scene, &surface);
    assert!(compositor.resident_image_texture_cache.is_empty());
    assert_eq!(ledger.snapshot().resource_bytes, 8);
    // Preserve an independently owned eligible peer behind the newer raster.
    scene
        .zone_registry
        .zones
        .get_mut("resident")
        .unwrap()
        .contention_policy = ContentionPolicy::MergeByKey { max_keys: 4 };
    scene
        .publish_to_zone(
            "resident",
            ZoneContent::SolidColor(Rgba::new(0.0, 0.0, 1.0, 1.0)),
            "mcp-peer",
            Some("peer".into()),
            None,
            None,
        )
        .unwrap();
    scene
        .publish(
            &tze_hud_scene::Publisher {
                resident_raster: Some(source.clone()),
                ..tze_hud_scene::Publisher::namespace("raster-a")
            },
            &tze_hud_scene::Publication::Zone {
                zone_name: "resident".into(),
                content: ZoneContent::StaticImage(source.resource_id()),
                merge_key: None,
                expires_at_wall_us: None,
                content_classification: None,
                breakpoints: Vec::new(),
                held: false,
            },
        )
        .unwrap();
    let republished = render_resident_pixels(&mut compositor, &mut scene, &surface);
    assert_pixel_color(&republished, 128, 8, 32, [255, 0, 0, 255], 8, "republished").unwrap();
    directory.with_agents_of(Default::default());
    let revoked = render_resident_pixels(&mut compositor, &mut scene, &surface);
    assert_pixel_color(
        &revoked,
        128,
        8,
        32,
        [0, 0, 255, 255],
        8,
        "eligible peer wins after raster revocation",
    )
    .unwrap();
    assert_eq!(scene.drain_revoked_resident_rasters(), 1);
    assert_eq!(
        scene.zone_registry.active_for_zone("resident")[0].publisher_namespace,
        "mcp-peer"
    );
    assert!(compositor.resident_image_texture_cache.is_empty());
    assert_eq!(
        ledger.snapshot().resource_bytes,
        8,
        "revocation does not free shared store pixels"
    );
}

#[tokio::test]
async fn resident_raster_gpu_budget_and_degradation_keep_store_cpu_charge() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(128, 64).await);
    let (mut scene, _store, ledger, _directory, source) = resident_raster_scene(12).await;
    compositor.set_resident_ledger(ledger.clone());
    render_resident_pixels(&mut compositor, &mut scene, &surface);
    assert!(
        compositor.resident_image_texture_cache.is_empty(),
        "8 CPU + 8 GPU exceeds cap"
    );
    assert_eq!(ledger.snapshot().resource_bytes, 8);
    compositor.set_degradation_policy(CompositorDegradationPolicy {
        level: DegradationLevel::Simplified,
        texture_quality_threshold_px: 1,
        texture_scale_factor: 0.5,
    });
    render_resident_pixels(&mut compositor, &mut scene, &surface);
    let entry = &compositor.resident_image_texture_cache[&source.resource_id()];
    assert_eq!((entry.width, entry.height), (1, 1));
    assert_eq!(ledger.snapshot().resource_bytes, 12);
    assert_eq!(ledger.snapshot().allocation_count, 2);
    compositor.set_degradation_policy(Default::default());
    assert_eq!(
        ledger.snapshot().resource_bytes,
        8,
        "policy invalidation releases resident GPU charge"
    );
}

#[tokio::test]
async fn resident_raster_stale_draw_denies_revoked_source_with_live_same_hash_peer() {
    use crate::surface::CompositorSurface;
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(128, 64).await);
    let (mut scene, _store, ledger, directory, source_a) = resident_raster_scene(64).await;
    compositor.set_resident_ledger(ledger);
    // Renderer seam fixture: grant B the already validated allocation. Real
    // first-grant admission is covered separately through the tonic handler.
    let auth_b = tze_hud_scene::config::agents::PairingAuthorization::default();
    auth_b.with_live_grants(|grants| {
        grants.insert(*source_a.resource_id().as_bytes());
    });
    let (pixels, width, height) = source_a.pixels_if_authorized().unwrap();
    let source_b = std::sync::Arc::new(ResidentRasterSource::new(
        source_a.resource_id(),
        width,
        height,
        pixels,
        auth_b,
    ));
    scene
        .publish(
            &tze_hud_scene::Publisher {
                resident_raster: Some(source_b),
                ..tze_hud_scene::Publisher::namespace("raster-b")
            },
            &tze_hud_scene::Publication::Zone {
                zone_name: "generic".into(),
                content: ZoneContent::StaticImage(source_a.resource_id()),
                merge_key: None,
                expires_at_wall_us: None,
                content_classification: None,
                breakpoints: Vec::new(),
                held: false,
            },
        )
        .unwrap();
    let (_, commands, _, _) =
        compositor.build_frame_vertices(&scene, 128.0, 64.0, &mut FrameTelemetry::new(0));
    assert_eq!(commands.len(), 2);
    assert_eq!(compositor.resident_image_texture_cache.len(), 1);
    // Both exact draw commands were selected while A was live. B's live source
    // keeps the shared GPU cache entry after A is revoked before encoding.
    directory.with_agents_of(Default::default());
    let frame = surface.acquire_frame().unwrap();
    let (mut encoder, _, _, signature_current) = compositor.encode_frame(
        &[],
        &frame.view,
        &scene,
        &commands,
        128,
        64,
        false,
        0,
        0,
        &[],
    );
    assert!(
        !signature_current,
        "denied stale draw invalidates predicted presentation signature"
    );
    surface.copy_to_buffer(&mut encoder);
    compositor.queue.submit(std::iter::once(encoder.finish()));
    let pixels = surface.read_pixels(&compositor.device);
    assert_ne!(
        pixel_at(&pixels, 128, 8, 32),
        [255, 0, 0, 255],
        "stale A command cannot borrow B's live texture"
    );
    assert_pixel_color(
        &pixels,
        128,
        72,
        32,
        [255, 0, 0, 255],
        8,
        "authorized B remains visible",
    )
    .unwrap();
}

#[tokio::test]
async fn resident_raster_respects_background_and_content_bands() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(128, 64).await);
    for layer in [LayerAttachment::Background, LayerAttachment::Content] {
        let (mut scene, _store, _ledger, _directory, _source) = resident_raster_scene(64).await;
        scene
            .zone_registry
            .zones
            .get_mut("resident")
            .unwrap()
            .layer_attachment = layer;
        let mut foreground = scene.zone_registry.zones["generic"].clone();
        foreground.name = "foreground".into();
        foreground.id = SceneId::new();
        foreground.layer_attachment = LayerAttachment::Chrome;
        foreground.geometry_policy = GeometryPolicy::Relative {
            x_pct: 0.25,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 1.0,
        };
        scene.register_zone(foreground);
        scene
            .publish_to_zone(
                "foreground",
                ZoneContent::SolidColor(Rgba::new(0.0, 0.0, 1.0, 1.0)),
                "peer",
                None,
                None,
                None,
            )
            .unwrap();
        let tab = scene.create_tab("foreground tile", 0).unwrap();
        let lease = scene.grant_lease("peer", 60_000);
        let tile = scene
            .create_tile(tab, "peer", lease, Rect::new(10.0, 0.0, 6.0, 64.0), 1)
            .unwrap();
        scene
            .set_tile_root(
                tile,
                solid_node(
                    Rect::new(0.0, 0.0, 6.0, 64.0),
                    Rgba::new(1.0, 1.0, 0.0, 1.0),
                    None,
                ),
            )
            .unwrap();
        let pixels = render_resident_pixels(&mut compositor, &mut scene, &surface);
        assert_pixel_color(
            &pixels,
            128,
            8,
            32,
            [255, 0, 0, 255],
            8,
            "uncovered resident",
        )
        .unwrap();
        assert_pixel_color(
            &pixels,
            128,
            56,
            32,
            [0, 0, 255, 255],
            8,
            "Chrome peer occludes resident",
        )
        .unwrap();
        let expected = if layer == LayerAttachment::Background {
            [255, 255, 0, 255]
        } else {
            [255, 0, 0, 255]
        };
        assert_pixel_color(
            &pixels,
            128,
            13,
            32,
            expected,
            8,
            "tile respects resident band",
        )
        .unwrap();
    }
}
