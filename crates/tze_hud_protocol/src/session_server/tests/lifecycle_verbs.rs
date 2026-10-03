use super::*;

fn tile_surface_of(id: &[u8]) -> String {
    super::verbs::tile_surface(bytes_to_scene_id(id).unwrap())
}

/// ClaimTile grants the lease, places the tile from tokens, and applies the
/// initial tree in one round trip; client node ids come back in `ids`.
#[tokio::test]
async fn claim_tile_is_one_round_trip_with_root_and_ids() {
    let (tx, mut stream, state, server, _client) = verb_agent("claim-agent").await;
    let child_id = SceneId::new();
    let mut root = crate::convert::scene_node_to_proto(&Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.1, 0.2, 0.3, 1.0),
            bounds: Rect::new(0.0, 0.0, 100.0, 50.0),
            radius: None,
        }),
    });
    root.children
        .push(crate::convert::scene_node_to_proto(&Node {
            layout: Default::default(),
            id: child_id,
            children: vec![],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::new(0.4, 0.5, 0.6, 1.0),
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                radius: None,
            }),
        }));
    let r = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::TopRight, TileSize::Small, Some(root)),
    )
    .await;
    assert!(r.ok, "{r:?}");
    assert_eq!(r.seq, 2);
    assert_eq!(r.ids.len(), 3, "tile id, then root and child");
    assert_eq!(r.lease_id.len(), 16);
    assert_eq!(r.ttl_ms, 60_000);
    assert_eq!(bytes_to_scene_id(&r.ids[2]).unwrap(), child_id);

    let tile_id = bytes_to_scene_id(&r.ids[0]).unwrap();
    let st = state.lock().await;
    let scene = st.scene.lock().await;
    let tile = &scene.tiles[&tile_id];
    let tokens = tze_hud_scene::placement::TilePlacementTokens::default();
    assert_eq!(tile.bounds.width, tokens.small.0);
    assert_eq!(
        tile.bounds.x,
        scene.display_area.width - tokens.margin - tokens.small.0
    );
    assert_eq!(tile.bounds.y, tokens.margin);
    assert!(tile.root_node.is_some(), "root applied in the same request");
    drop(scene);
    drop(st);
    server.abort();
}

/// Tiles claimed at one anchor stack instead of overlapping.
#[tokio::test]
async fn claim_tile_stacks_at_shared_anchor() {
    let (tx, mut stream, state, server, _client) = verb_agent("stack-agent").await;
    let a = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::TopLeft, TileSize::Small, None),
    )
    .await;
    let b = request(
        &tx,
        &mut stream,
        3,
        claim(TileAnchor::TopLeft, TileSize::Small, None),
    )
    .await;
    assert!(a.ok && b.ok);
    let st = state.lock().await;
    let scene = st.scene.lock().await;
    let ta = &scene.tiles[&bytes_to_scene_id(&a.ids[0]).unwrap()];
    let tb = &scene.tiles[&bytes_to_scene_id(&b.ids[0]).unwrap()];
    assert!(
        !ta.bounds.intersects(&tb.bounds),
        "stacked, not overlapping"
    );
    assert!(tb.z_order > ta.z_order, "later claims sit above");
    drop(scene);
    drop(st);
    server.abort();
}

/// A retransmitted ClaimTile replays its reply rather than claiming twice.
#[tokio::test]
async fn claim_tile_retransmit_replays_cached_result() {
    let (tx, mut stream, state, server, _client) = verb_agent("retx-agent").await;
    let first = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    let again = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    assert_eq!(first, again);
    assert_eq!(state.lock().await.scene.lock().await.tile_count(), 1);
    server.abort();
}

/// Hold renews a tile's lease; Clear releases it and removes the tile.
#[tokio::test]
async fn hold_and_clear_tile() {
    let (tx, mut stream, state, server, _client) = verb_agent("hold-agent").await;
    let c = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Center, TileSize::Medium, None),
    )
    .await;
    let surface = tile_surface_of(&c.ids[0]);
    let held = request(
        &tx,
        &mut stream,
        3,
        ClientPayload::Hold(Hold {
            surface: surface.clone(),
            ttl_ms: 90_000,
        }),
    )
    .await;
    assert!(held.ok, "{held:?}");
    assert_eq!(held.ttl_ms, 90_000);
    assert_eq!(held.lease_id, c.lease_id);

    let cleared = request(
        &tx,
        &mut stream,
        4,
        ClientPayload::Clear(Clear {
            surface: surface.clone(),
        }),
    )
    .await;
    assert!(cleared.ok, "{cleared:?}");
    assert_eq!(state.lock().await.scene.lock().await.tile_count(), 0);

    let again = request(
        &tx,
        &mut stream,
        5,
        ClientPayload::Hold(Hold { surface, ttl_ms: 0 }),
    )
    .await;
    assert_eq!(again.code, "NOT_HELD");
    server.abort();
}

/// Hold on a zone extends the agent's publication; with nothing held it
/// answers NOT_HELD.
#[tokio::test]
async fn hold_zone_publication() {
    let (tx, mut stream, state, server, _client) = verb_agent("zone-hold-agent").await;
    let hold = || {
        ClientPayload::Hold(Hold {
            surface: "zone:subtitle".to_string(),
            ttl_ms: 0,
        })
    };
    let r = request(&tx, &mut stream, 2, hold()).await;
    assert_eq!(r.code, "NOT_HELD");
    let p = request(
        &tx,
        &mut stream,
        3,
        ClientPayload::Publish(subtitle_publish("x", 5_000, 0, 0)),
    )
    .await;
    assert!(p.ok, "{p:?}");
    let r = request(&tx, &mut stream, 4, hold()).await;
    assert!(r.ok, "{r:?}");
    let st = state.lock().await;
    let scene = st.scene.lock().await;
    let record = &scene.zone_registry.active_publishes["subtitle"][0];
    assert_eq!(
        record.expires_at_wall_us, None,
        "ttl_ms 0 holds until cleared"
    );
    drop(scene);
    drop(st);
    server.abort();
}

/// Publish then Clear on a zone surface.
#[tokio::test]
async fn clear_zone_publication() {
    let (tx, mut stream, state, server, _client) = verb_agent("zone-clear-agent").await;
    let p = request(
        &tx,
        &mut stream,
        2,
        ClientPayload::Publish(subtitle_publish("bye", 0, 0, 0)),
    )
    .await;
    assert!(p.ok, "{p:?}");
    let c = request(
        &tx,
        &mut stream,
        3,
        ClientPayload::Clear(Clear {
            surface: "zone:subtitle".to_string(),
        }),
    )
    .await;
    assert!(c.ok, "{c:?}");
    assert!(subtitle_texts(&*state.lock().await.scene.lock().await).is_empty());
    server.abort();
}

/// Scene validation reasons reach the agent as the hint (no flattening).
#[tokio::test]
async fn publish_to_unknown_zone_names_the_zone() {
    let (tx, mut stream, _state, server, _client) = verb_agent("unknown-zone-agent").await;
    let mut publish = subtitle_publish("x", 0, 0, 0);
    publish.surface = "zone:nope".to_string();
    let r = request(&tx, &mut stream, 2, ClientPayload::Publish(publish)).await;
    assert_eq!(r.code, "ZONE_NOT_FOUND");
    assert!(r.hint.contains("nope"), "{r:?}");
    server.abort();
}
