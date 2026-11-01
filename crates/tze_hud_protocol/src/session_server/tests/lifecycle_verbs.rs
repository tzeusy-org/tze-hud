use super::*;

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
    assert_eq!(
        request(&tx, &mut stream, 4, hold()).await,
        r,
        "Hold retransmission replays the original result"
    );
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
    // A replay must not clear content published after the original Clear.
    assert!(
        request(
            &tx,
            &mut stream,
            4,
            ClientPayload::Publish(subtitle_publish("new", 0, 0, 0))
        )
        .await
        .ok
    );
    assert_eq!(
        request(
            &tx,
            &mut stream,
            3,
            ClientPayload::Clear(Clear {
                surface: "zone:subtitle".into()
            })
        )
        .await,
        c
    );
    assert_eq!(
        subtitle_texts(&*state.lock().await.scene.lock().await),
        ["new"]
    );
    server.abort();
}

/// Hold retimes and Clear cancels the agent's own delayed publish, which has
/// not presented yet; Clear also drops its wake deadline.
#[tokio::test]
async fn hold_and_clear_reach_a_pending_delayed_publish() {
    let (tx, mut stream, state, server, _client) = verb_agent("pending-agent").await;
    let present_at = now_wall_us() + 60_000_000;
    let p = request(
        &tx,
        &mut stream,
        2,
        ClientPayload::Publish(subtitle_publish("later", 0, present_at, 0)),
    )
    .await;
    assert!(p.ok, "{p:?}");
    let surface = "zone:subtitle".to_string();
    let h = request(
        &tx,
        &mut stream,
        3,
        ClientPayload::Hold(Hold {
            surface: surface.clone(),
            ttl_ms: 5_000,
        }),
    )
    .await;
    assert!(h.ok, "pending publish counts as held: {h:?}");
    {
        let st = state.lock().await;
        let scene = st.scene.lock().await;
        let tze_hud_scene::mutation::SceneMutation::PublishToZone {
            expires_at_wall_us, ..
        } = &scene.scheduled_batches[0].batch.mutations[0]
        else {
            panic!("scheduled a zone publish");
        };
        assert_eq!(*expires_at_wall_us, Some(present_at + 5_000_000));
    }
    let c = request(&tx, &mut stream, 4, ClientPayload::Clear(Clear { surface })).await;
    assert!(c.ok, "{c:?}");
    let st = state.lock().await;
    let scene = st.scene.lock().await;
    assert!(scene.scheduled_batches.is_empty());
    assert_eq!(scene.next_timed_content_wall_us(), None);
    drop(scene);
    drop(st);
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
    assert!(!r.ok);
    assert_eq!(r.seq, 2);
    assert!(r.hint.contains("Use a configured zone name"), "{r:?}");
    assert!(r.hint.contains("resend Publish"), "{r:?}");
    server.abort();
}
