use super::*;

/// Safe mode rejects durable publishing while heartbeats and resume remain usable.
#[tokio::test]
async fn test_safe_mode_rejects_mutations() {
    let (tx, mut stream, state, server, _client) = verb_agent("safe-mode-agent").await;
    assert!(
        request(
            &tx,
            &mut stream,
            2,
            ClientPayload::Publish(subtitle_publish("before", 0, 0, 0))
        )
        .await
        .ok
    );
    state
        .lock()
        .await
        .safe_mode_atomic
        .store(true, std::sync::atomic::Ordering::Release);
    let rejected = request(
        &tx,
        &mut stream,
        3,
        ClientPayload::Publish(subtitle_publish("paused", 0, 0, 0)),
    )
    .await;
    assert_eq!(rejected.code, "SAFE_MODE_ACTIVE");
    assert_eq!(
        subtitle_texts(&*state.lock().await.scene.lock().await),
        ["before"]
    );
    tx.send(ClientMessage {
        sequence: 4,
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 999,
        })),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(matches!(
        next_server_msg(&mut stream).await.payload,
        Some(ServerPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 999
        }))
    ));
    state
        .lock()
        .await
        .safe_mode_atomic
        .store(false, std::sync::atomic::Ordering::Release);
    assert!(
        request(
            &tx,
            &mut stream,
            5,
            ClientPayload::Publish(subtitle_publish("after", 0, 0, 0))
        )
        .await
        .ok
    );
    assert_eq!(
        subtitle_texts(&*state.lock().await.scene.lock().await),
        ["after"]
    );
    server.abort();
}
