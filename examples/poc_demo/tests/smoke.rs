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
    // Exercise the real client's typed-error boundary before the shipped-config flow.
    // This endpoint only controls responses; the runtime below still owns delivery.
    let tool_error = |code, hint| {
        json!({"jsonrpc":"2.0", "id":1, "result":{
            "isError":true, "content":[{"type":"text", "text":json!({"code":code,"hint":hint}).to_string()}]
        }})
    };
    let busy = "input state busy; retry hud_input";
    for (tool, arguments, status, response, retries) in [
        (
            "hud_input",
            json!({"ack":["retained-id"]}),
            200,
            tool_error("INTERNAL", busy),
            true,
        ),
        (
            "hud_input",
            json!({"wait_ms":1000}),
            200,
            tool_error("INTERNAL", busy),
            true,
        ),
        (
            "hud_input",
            json!({}),
            200,
            tool_error("INTERNAL", "another runtime fault"),
            false,
        ),
        (
            "hud_input",
            json!({}),
            200,
            tool_error("NOT_ALLOWED", busy),
            false,
        ),
        (
            "hud_publish",
            json!({"surface":"zone:subtitle","content":"once"}),
            200,
            tool_error("INTERNAL", busy),
            false,
        ),
        (
            "hud_clear",
            json!({"surface":"zone:subtitle"}),
            200,
            tool_error("INTERNAL", busy),
            false,
        ),
        ("hud_input", json!({}), 401, json!({}), false),
        (
            "hud_input",
            json!({}),
            200,
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"invalid request"}}),
            false,
        ),
    ] {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let observed = Arc::clone(&seen);
        let endpoint = Target {
            mcp: listener.local_addr().unwrap().to_string(),
            mcp_psk: PSK.into(),
            pace: Duration::ZERO,
            human_wait: Duration::ZERO,
        };
        let replies = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut raw = Vec::new();
                let header_end = loop {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    raw.push(byte[0]);
                    assert!(raw.len() < 8192, "bounded fixture request headers");
                    if raw.ends_with(b"\r\n\r\n") {
                        break raw.len();
                    }
                };
                let headers = std::str::from_utf8(&raw).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("Content-Length: ")
                            .map(|n| n.parse().unwrap())
                    })
                    .unwrap();
                assert!(length < 8192, "bounded fixture request body");
                raw.resize(header_end + length, 0);
                stream.read_exact(&mut raw[header_end..]).await.unwrap();
                observed
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&raw[header_end..]).unwrap());
                let payload = if attempt == 0 {
                    response.clone()
                } else {
                    json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"items\":[],\"remaining\":0}"}]}})
                }.to_string();
                let status = if attempt == 0 { status } else { 200 };
                stream
                    .write_all(
                        format!(
                            "HTTP/1.0 {status} fixture\r\nContent-Length: {}\r\n\r\n{payload}",
                            payload.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            poc_demo::Mcp(&endpoint).call(tool, arguments.clone()),
        )
        .await
        .unwrap();
        replies.abort();
        let _ = replies.await;
        let requests = seen.lock().unwrap();
        if retries {
            assert_eq!(
                result.expect("a typed scene-busy response is recoverable"),
                json!({"items":[],"remaining":0})
            );
            assert_eq!(requests.len(), 2, "one failed attempt and one success");
            if arguments.get("wait_ms").is_some() {
                let remaining = requests[1]["params"]["arguments"]["wait_ms"]
                    .as_u64()
                    .unwrap();
                assert!(
                    remaining > 0 && remaining < 1000,
                    "retry retains the original input deadline"
                );
            } else {
                assert_eq!(requests[0]["params"]["arguments"], arguments);
                assert_eq!(
                    requests[1]["params"]["arguments"], arguments,
                    "repeat the same ACK, without an extra event"
                );
            }
        } else {
            assert!(result.is_err(), "terminal errors and other tools must fail");
            assert_eq!(requests.len(), 1, "no retry for this error/tool");
        }
    }

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
