//! The demo's stages against the GPU-free headless runtime (production.toml,
//! real MCP over loopback HTTP and a real gRPC session), as the live app
//! would serve them.

use std::sync::Arc;
use std::time::Duration;

use poc_demo::{Result, Target};
use serde_json::json;
use tze_hud_runtime::windowed::{HeadlessEventLoopHarness, WindowedConfig};
use tze_hud_scene::SystemClock;

const PSK: &str = "poc-demo-test-psk";

async fn boot() -> HeadlessEventLoopHarness {
    HeadlessEventLoopHarness::with_network(
        WindowedConfig {
            agents: tze_hud_config::agents_file::AgentsFile::default()
                .with_agent("demo", PSK, &["*"])
                .directory()
                .expect("valid agents entry")
                .shared(),
            config_toml: Some(
                include_str!("../../../app/tze_hud_app/config/production.toml").to_string(),
            ),
            config_file_path: Some(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../app/tze_hud_app/config/production.toml"
                )
                .to_string(),
            ),
            ..WindowedConfig::default()
        },
        Arc::new(SystemClock::new()),
    )
    .await
    .expect("boot the runtime from production.toml")
}

fn target(hud: &HeadlessEventLoopHarness, human_wait: Duration) -> Target {
    Target {
        mcp: hud.mcp_addr().to_string(),
        mcp_psk: PSK.to_string(),
        pace: Duration::ZERO,
        human_wait,
    }
}

/// Run a stage while the event loop keeps turning, as it does in the app.
/// `each_turn` is where a test plays the viewer.
async fn drive(
    hud: &mut HeadlessEventLoopHarness,
    stage: impl Future<Output = Result<()>>,
    mut each_turn: impl FnMut(&mut HeadlessEventLoopHarness),
) {
    let mut stage = Box::pin(stage);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        hud.tick();
        each_turn(hud);
        tokio::select! {
            biased;
            done = &mut stage => return done.expect("stage completes"),
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "stage never finished"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zones_and_widgets_run_against_the_shipped_config() {
    let mut hud = boot().await;

    // The viewer presses Ship; the stage must ack it, so nothing is redelivered.
    let t = target(&hud, Duration::from_secs(30));
    let mut pressed = false;
    drive(&mut hud, poc_demo::zones(&t), |hud| {
        if let Some((x, y)) = hud.notification_action_center("ship")
            && !std::mem::replace(&mut pressed, true)
        {
            hud.click(x, y);
        }
    })
    .await;
    let mut redelivered = None;
    drive(
        &mut hud,
        async {
            redelivered = Some(poc_demo::Mcp(&t).call("hud_input", json!({})).await?);
            Ok(())
        },
        |_| {},
    )
    .await;
    assert_eq!(
        redelivered.expect("polled")["items"],
        json!([]),
        "the action press was acked"
    );

    let t = target(&hud, Duration::ZERO);
    drive(&mut hud, poc_demo::widgets(&t), |_| {}).await;
}
