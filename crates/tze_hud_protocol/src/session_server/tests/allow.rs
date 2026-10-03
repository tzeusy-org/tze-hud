use super::*;

// ─── Allow-list boundary checks ─────────────────────────────────────────────

/// A Publish to a zone outside the agent's allow list is rejected at the
/// boundary.
#[tokio::test]
async fn zone_publish_outside_allow_list_rejected() {
    let (tx, mut stream, _state, server, _client) = verb_agent("widget-no-cap-agent").await;
    let r = request(
        &tx,
        &mut stream,
        2,
        ClientPayload::Publish(subtitle_publish("hello", 0, 0, 0)),
    )
    .await;
    assert!(!r.ok);
    assert_eq!(r.code, "NOT_ALLOWED");
    assert!(r.hint.contains("zone:subtitle"));
    server.abort();
}

/// Clear needs the surface in the allow list.
#[tokio::test]
async fn clear_outside_allow_list_rejected() {
    let (tx, mut stream, _state, server, _client) = verb_agent("widget-no-cap-agent").await;
    for (seq, surface, entry) in [
        (2, "zone:subtitle", "zone:subtitle"),
        (3, "widget:gauge", "widget:gauge"),
    ] {
        let r = request(
            &tx,
            &mut stream,
            seq,
            ClientPayload::Clear(Clear {
                surface: surface.to_string(),
            }),
        )
        .await;
        assert_eq!(r.code, "NOT_ALLOWED");
        assert!(r.hint.contains(entry), "{r:?}");
    }
    server.abort();
}

/// ClaimTile needs `tiles` in the allow list.
#[tokio::test]
async fn claim_tile_without_tiles_allow_rejected() {
    let (tx, mut stream, _state, server, _client) = verb_agent("zone-only-agent").await;
    let r = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    assert_eq!(r.code, "NOT_ALLOWED");
    assert!(r.hint.contains("tiles"));
    server.abort();
}

/// With `tiles` allowed, the same ClaimTile succeeds.
#[tokio::test]
async fn claim_tile_with_tiles_allow_accepted() {
    let (tx, mut stream, _state, server, _client) = verb_agent("widget-no-cap-agent").await;
    let r = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    assert!(r.ok, "{}: {}", r.code, r.hint);
    server.abort();
}

/// CreateTile and the portal mutations are runtime-internal: an agent batch
/// carrying one is rejected with a hint naming the replacement.
#[tokio::test]
async fn internal_mutations_rejected_from_agents() {
    let (tx, mut stream, _state, server, _client) = verb_agent("internal-agent").await;
    let c = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    let create =
        crate::proto::mutation_proto::Mutation::CreateTile(crate::proto::CreateTileMutation {
            tab_id: vec![],
            bounds: None,
            z_order: 1,
        });
    let accent = crate::proto::mutation_proto::Mutation::SetTileUnreadCount(
        crate::proto::SetTileUnreadCountMutation {
            tile_id: c.ids[0].clone(),
            count: 3,
        },
    );
    for (seq, m, needle) in [(3, create, "ClaimTile"), (4, accent, "runtime-internal")] {
        let r = request(
            &tx,
            &mut stream,
            seq,
            ClientPayload::MutationBatch(MutationBatch {
                batch_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
                lease_id: c.lease_id.clone(),
                mutations: vec![crate::proto::MutationProto { mutation: Some(m) }],
                timing: None,
            }),
        )
        .await;
        assert_eq!(r.code, "INVALID_ARGUMENT");
        assert!(r.hint.contains(needle), "{r:?}");
    }
    server.abort();
}
