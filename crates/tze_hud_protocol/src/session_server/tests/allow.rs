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
    for (seq, surface, entry, key) in [
        (2, "zone:subtitle", "zone:subtitle", ""),
        (3, "widget:gauge", "widget:gauge", ""),
        (4, "zone:subtitle", "zone:subtitle", "slot"),
        (5, "widget:gauge", "widget:gauge", "slot"),
    ] {
        let r = request(
            &tx,
            &mut stream,
            seq,
            ClientPayload::Clear(Clear {
                key: key.into(),
                surface: surface.to_string(),
            }),
        )
        .await;
        assert_eq!(r.code, "NOT_ALLOWED");
        assert!(r.hint.contains(entry), "{r:?}");
    }
    let (wildcard, mut replies, _scene, wildcard_server, _client) =
        verb_agent("wildcard-agent").await;
    let mut sequence = 2;
    for surface in [
        format!("tile:{}", SceneId::new().as_uuid()),
        "tile:nope".into(),
        "tile:".into(),
    ] {
        for payload in [
            ClientPayload::Publish(Publish {
                surface: surface.clone(),
                ..Default::default()
            }),
            ClientPayload::Clear(Clear {
                key: String::new(),
                surface: surface.clone(),
            }),
            ClientPayload::Hold(Hold {
                surface: surface.clone(),
                ttl_ms: 0,
            }),
        ] {
            let result = request(&wildcard, &mut replies, sequence, payload).await;
            assert!(!result.ok);
            assert_eq!(result.code, "INVALID_ARGUMENT", "{surface}: {result:?}");
            assert_eq!(result.seq, sequence);
            sequence += 1;
        }
    }
    wildcard_server.abort();
    server.abort();
}
