//! # POC demo: one binary that drives a live tze_hud through every lifecycle stage
//!
//! Talks to a running app the way an agent would: MCP over HTTP
//! (`hud_surfaces`, `hud_publish`, `hud_input`, `hud_clear`) for zones and
//! widgets. Each stage is
//! narrated in `zone:subtitle`, and every MCP call prints the model-visible
//! tokens it cost (tool name + arguments + result text, o200k), the number
//! `docs/api.md` "Token budgets" holds each stage to.
//!
//! The portal stage is not here: it is the real Claude Code session running
//! the `hud-projection` skill ([`PORTAL_INSTRUCTIONS`]).
//!
//! PSKs come from the environment or a file and are never printed.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub const SUBTITLE: &str = "zone:subtitle";

pub const NOTIFICATION: &str = "zone:notification-area";
pub const GAUGE: &str = "widget:main-gauge";
pub const PROGRESS: &str = "widget:main-progress";

/// What to run in place of a portal stage.
pub const PORTAL_INSTRUCTIONS: &str = "Portal: not simulated here. In a Claude Code session, run the hud-projection skill \
     (.claude/skills/hud-projection/SKILL.md): its first hud_publish to portal:<id> attaches, \
     hud_input carries your reply, hud_clear detaches.";

/// Where the live app listens, who we are to it, and how fast to go.
pub struct Target {
    /// `host:port` of the MCP HTTP listener.
    pub mcp: String,
    pub mcp_psk: String,
    /// Pause between steps so a person can watch.
    pub pace: Duration,
    /// How long a stage waits on a human (notification press).
    pub human_wait: Duration,
}

/// MCP over HTTP: one tool call per connection, tokens counted per call.
pub struct Mcp<'a>(pub &'a Target);

impl Mcp<'_> {
    /// Call a tool, print its model-visible tokens, and return the parsed
    /// result. Only the explicit scene-busy `hud_input` error is retried, for
    /// at most 100 ms; every attempt's request and response are counted.
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<Value> {
        let input_deadline = tokio::time::Instant::now()
            + Duration::from_millis(arguments["wait_ms"].as_u64().unwrap_or(0).min(30_000));
        let input_response_deadline = input_deadline + Duration::from_millis(100);
        let mut arguments = arguments;
        let mut recovery_deadline = None;
        loop {
            let attempt = self.call_once(tool, &arguments);
            let (is_error, parsed) = match recovery_deadline {
                // Leave fixed response-drain time after the original input wait.
                // Busy/backoff recovery still uses its own unchanged deadline.
                Some(deadline) => {
                    tokio::time::timeout_at(deadline.max(input_response_deadline), attempt)
                        .await
                        .map_err(|_| "hud_input recovery response deadline exceeded")??
                }
                None => attempt.await?,
            };
            if !is_error {
                return Ok(parsed);
            }
            let error = format!("{}: {}", parsed["code"], parsed["hint"]);
            if tool != "hud_input"
                || parsed["code"] != "INTERNAL"
                || parsed["hint"] != "input state busy; retry hud_input"
            {
                return Err(error.into());
            }
            let deadline = *recovery_deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_millis(100));
            let next_attempt = tokio::time::Instant::now() + Duration::from_millis(10);
            if next_attempt >= deadline {
                return Err(format!("{error}; 100 ms client recovery budget exhausted").into());
            }
            tokio::time::sleep_until(next_attempt).await;
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("{error}; 100 ms client recovery budget exhausted").into());
            }
            // Retrying a poll must not restart its human-input waiting window.
            // Omitted/default-zero ACK requests retain their exact arguments.
            if let Some(wait_ms) = arguments.get_mut("wait_ms") {
                *wait_ms = json!(
                    input_deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .as_millis() as u64
                );
            }
        }
    }

    async fn call_once(&self, tool: &str, arguments: &Value) -> Result<(bool, Value)> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        .to_string();
        let mut stream = TcpStream::connect(&self.0.mcp)
            .await
            .map_err(|e| format!("connect to MCP at {}: {e}", self.0.mcp))?;
        // HTTP/1.0: the server answers and closes, so no framing to parse.
        let request = format!(
            "POST /mcp HTTP/1.0\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.0.mcp_psk,
            body.len()
        );
        stream.write_all(request.as_bytes()).await?;
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await?;
        let (head, payload) = raw.split_once("\r\n\r\n").ok_or("no HTTP response")?;
        let status = head.lines().next().unwrap_or_default();
        if !status.contains(" 200") {
            return Err(format!("MCP {status}: is the MCP PSK paired?").into());
        }
        let response: Value = serde_json::from_str(payload)?;
        if let Some(error) = response.get("error") {
            return Err(format!("JSON-RPC error: {error}").into());
        }
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .ok_or("no result text")?;
        println!(
            "  {tool:<12} {:<26} {:>4} tokens",
            arguments["surface"].as_str().unwrap_or(""),
            model_tokens(&format!("{tool}{arguments}{text}"))
        );
        let parsed: Value = serde_json::from_str(text)?;
        Ok((result["isError"] == json!(true), parsed))
    }

    /// Say what the next stage shows, on `zone:subtitle`.
    pub async fn narrate(&self, text: &str) -> Result<()> {
        println!("\n{text}");
        self.call(
            "hud_publish",
            json!({"surface": SUBTITLE, "content": text, "ttl_ms": 8_000}),
        )
        .await?;
        tokio::time::sleep(self.0.pace).await;
        Ok(())
    }

    /// `hud_surfaces`, failing with a pointer to the config if any of
    /// `required` is not offered to this agent.
    pub async fn require(&self, required: &[&str]) -> Result<()> {
        let surfaces = self.call("hud_surfaces", json!({})).await?;
        for surface in required {
            let offered = surfaces["surfaces"]
                .as_array()
                .is_some_and(|all| all.iter().any(|entry| entry["s"] == *surface));
            if !offered {
                return Err(format!(
                    "{surface} is not offered: run the app with its production.toml and pair an agent that allows it"
                )
                .into());
            }
        }
        Ok(())
    }
}

/// o200k tokens, counted the way `token_footprint` counts them.
fn model_tokens(text: &str) -> usize {
    static BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::o200k_base().expect("bundled o200k_base vocabulary"))
        .encode_with_special_tokens(text)
        .len()
}

/// Zones: a notification that expires on its TTL, `delay_ms` content, and a
/// notification action pressed by the viewer reaching `hud_input`.
pub async fn zones(target: &Target) -> Result<()> {
    let mcp = Mcp(target);
    mcp.require(&[SUBTITLE, NOTIFICATION]).await?;

    mcp.narrate("Zones: a notification that takes itself down after its TTL")
        .await?;
    mcp.call(
        "hud_publish",
        json!({
            "surface": NOTIFICATION,
            "content": {
                "title": "Deploy",
                "body": "Ship v2?",
                "actions": [
                    {"label": "Ship", "callback_id": "ship"},
                    {"label": "Hold", "callback_id": "hold"},
                ],
            },
            "ttl_ms": 20_000,
        }),
    )
    .await?;

    mcp.narrate("Zones: delay_ms content appears on schedule, nobody calls again")
        .await?;
    mcp.call(
        "hud_publish",
        json!({"surface": SUBTITLE, "content": "Scheduled 2 s ago (delay_ms)", "delay_ms": 2_000, "ttl_ms": 6_000}),
    )
    .await?;

    println!(
        "\nPress Ship or Hold on the notification ({} s)...",
        target.human_wait.as_secs()
    );
    let polled = mcp
        .call(
            "hud_input",
            json!({"wait_ms": target.human_wait.as_millis().min(30_000)}),
        )
        .await?;
    let items = polled["items"].as_array().cloned().unwrap_or_default();
    match items.first() {
        Some(item) => {
            println!("  pressed: {}", item["action"]);
            mcp.call("hud_input", json!({"ack": [item["id"]]})).await?;
        }
        None => println!("  no press; skipped"),
    }
    Ok(())
}

/// Widgets: typed parameter updates only; the runtime owns the SVG and the pixels.
pub async fn widgets(target: &Target) -> Result<()> {
    let mcp = Mcp(target);
    mcp.require(&[SUBTITLE, GAUGE, PROGRESS]).await?;

    mcp.narrate("Widgets: typed params, no styling or geometry in the call")
        .await?;
    for (level, label) in [(0.25, "building"), (0.6, "testing"), (0.95, "shipping")] {
        mcp.call(
            "hud_publish",
            json!({"surface": GAUGE, "params": {"level": level, "label": label}}),
        )
        .await?;
        mcp.call(
            "hud_publish",
            json!({"surface": PROGRESS, "params": {"progress": level, "label": label}}),
        )
        .await?;
        tokio::time::sleep(target.pace).await;
    }
    mcp.call("hud_clear", json!({"surface": GAUGE})).await?;
    mcp.call("hud_clear", json!({"surface": PROGRESS})).await?;
    Ok(())
}

/// Parse a credential file without exposing its contents in errors.
pub fn psk_from_file_text(text: &str) -> Result<String> {
    let psk = if text.trim_start().starts_with('{') {
        serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|reply| reply["psk"].as_str().map(str::to_string))
            .ok_or("JSON credential file has no \"psk\" field")?
    } else {
        text.to_string()
    };
    let psk = psk.trim();
    if psk.is_empty() {
        return Err("credential is empty".into());
    }
    Ok(psk.to_string())
}

#[cfg(test)]
mod tests {
    use super::psk_from_file_text;

    #[test]
    fn credential_files_hold_a_bare_psk_or_a_pair_reply() {
        assert_eq!(psk_from_file_text("abc123\n").unwrap(), "abc123");
        assert_eq!(
            psk_from_file_text(r#"{"agent":"c","psk":"abc123"}"#).unwrap(),
            "abc123"
        );
        assert!(psk_from_file_text(" \n").is_err());
        let err = psk_from_file_text(r#"{"agent":"secret-looking"}"#).unwrap_err();
        assert!(
            !err.to_string().contains("secret-looking"),
            "must not echo the file"
        );
    }
}
