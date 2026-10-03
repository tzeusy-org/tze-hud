use super::*;

#[tokio::test]
async fn rejected_zone_publish_does_not_wake_the_compositor() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let generations = Arc::new(AtomicU64::new(0));
    let callback_generations = Arc::clone(&generations);
    let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_generations.fetch_add(1, Ordering::AcqRel);
    });
    let (mut client, _server, _state) = setup_test_with_state_and_render_wake(notifier).await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "zone-reject-agent", "test-key").await;
    let before = generations.load(Ordering::Acquire);

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "zone:missing-zone".to_string(),
            content: None,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let result = next_server_msg(&mut stream).await;
    assert!(matches!(
        result.payload,
        Some(ServerPayload::RequestResult(RequestResult {
            ok: false,
            ..
        }))
    ));
    assert_eq!(
        generations.load(Ordering::Acquire),
        before,
        "rejected ZonePublish must not synthesize compositor work"
    );
}
