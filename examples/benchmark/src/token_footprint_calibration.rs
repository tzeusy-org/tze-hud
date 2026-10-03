//! Deterministic Layer-3 token-footprint calibration for canonical MCP flows.

#[cfg(feature = "headless")]
mod calibration {
    use serde::{Deserialize, Serialize};
    use std::collections::{BTreeMap, HashMap};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tokio::sync::mpsc;
    use tze_hud_runtime::headless::{HeadlessConfig, HeadlessRuntime};
    use tze_hud_runtime::portal_projection_driver::{InProcessPortalDriver, PortalOp};
    use tze_hud_runtime::threads::{ShutdownReason, ShutdownToken};
    use tze_hud_runtime::{McpServerConfig, start_mcp_http_server};
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, RenderingPolicy, SceneId, WidgetDefinition,
        WidgetInstance, WidgetParamConstraints, WidgetParamType, WidgetParameterDeclaration,
        WidgetParameterValue,
    };

    const PSK: &str = "token-calibration-resident-psk";
    const CANONICAL_FLOW_VERSION: u32 = 2;
    const VOCAB_FINGERPRINT: &str =
        "sha256:446a9538cb6c348e3516120d7c08b09f57c36495e2acfffe59a5bf8b0cfb1a2d";

    #[derive(Debug, Deserialize)]
    struct DriverOutput {
        transactions: Vec<Transaction>,
    }

    #[derive(Debug, Deserialize)]
    struct Transaction {
        flow: String,
        operation: String,
        request_body: String,
        response_body: String,
    }

    #[derive(Clone, Debug, Serialize)]
    struct Counts {
        bytes: usize,
        tokens: usize,
    }

    #[derive(Clone, Debug, Serialize)]
    struct OperationMeasurement {
        request: Counts,
        response: Counts,
        total: Counts,
        /// What enters the model's context: the tool name plus its arguments,
        /// and the result text (for `tools/list`, the tool array). The
        /// JSON-RPC envelope never reaches the model.
        model_visible: Counts,
    }

    #[derive(Debug, Serialize)]
    struct FlowMeasurement {
        flow_version: u32,
        flow_fingerprint: String,
        operations: BTreeMap<String, OperationMeasurement>,
        total: Counts,
        model_visible: Counts,
    }

    #[derive(Debug, Serialize)]
    struct TokenizerIdentity {
        name: &'static str,
        implementation: &'static str,
        version: &'static str,
        vocab_fingerprint: &'static str,
        counting_policy: &'static str,
    }

    #[derive(Debug, Serialize)]
    struct CalibrationOutput {
        schema_version: u32,
        runtime_build: &'static str,
        tokenizer: TokenizerIdentity,
        fixture_fingerprint: String,
        flows: BTreeMap<String, FlowMeasurement>,
    }

    fn headless_config() -> HeadlessConfig {
        HeadlessConfig {
            width: 1920,
            height: 1080,
            grpc_port: 0,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted(PSK),
            config_toml: Some(String::new()),
        }
    }

    async fn prepare_scene(runtime: &HeadlessRuntime) {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
        let tab_id = scene
            .create_tab("Token Calibration", 0)
            .expect("create calibration tab");
        scene.widget_registry.register_definition(WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "Deterministic calibration gauge".to_string(),
            parameter_schema: vec![
                WidgetParameterDeclaration {
                    name: "level".to_string(),
                    param_type: WidgetParamType::F32,
                    default_value: WidgetParameterValue::F32(0.0),
                    constraints: Some(WidgetParamConstraints {
                        f32_min: Some(0.0),
                        f32_max: Some(1.0),
                        string_max_bytes: None,
                        enum_allowed_values: vec![],
                    }),
                },
                WidgetParameterDeclaration {
                    name: "label".to_string(),
                    param_type: WidgetParamType::String,
                    default_value: WidgetParameterValue::String(String::new()),
                    constraints: None,
                },
            ],
            layers: vec![],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 0.2,
                height_pct: 0.1,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        });
        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "gauge".to_string(),
            current_params: HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.0))]),
        });
    }

    async fn scene_handle(
        runtime: &HeadlessRuntime,
    ) -> std::sync::Arc<tokio::sync::Mutex<tze_hud_scene::graph::SceneGraph>> {
        let state = runtime.shared_state().lock().await;
        state.scene.clone()
    }

    fn spawn_portal_driver(
        mut rx: mpsc::UnboundedReceiver<PortalOp>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut driver = InProcessPortalDriver::new();
            let mut input_injected = false;
            while let Some(op) = rx.recv().await {
                let is_attach = matches!(&op, PortalOp::Attach { .. });
                driver.dispatch_portal_op(op);
                if is_attach && !input_injected {
                    driver
                        .authority_mut()
                        .enqueue_input(
                            "claude-main",
                            "canonical-input-0001",
                            "Canonical HUD-originated input.".to_string(),
                            1_700_000_000_100_000,
                            9_000_000_000_000_000,
                            None,
                        )
                        .expect("inject canonical portal input");
                    input_injected = true;
                }
            }
        })
    }

    fn run_python_driver(address: SocketAddr) -> DriverOutput {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let driver = root.join("examples/benchmark/token_footprint_flow.py");
        let output = Command::new("python3")
            .arg(driver)
            .env("HUD_MCP_URL", format!("http://{address}/mcp"))
            .env("HUD_PSK", PSK)
            .output()
            .expect("launch token-footprint Python flow driver");
        assert!(
            output.status.success(),
            "Python flow driver failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("parse Python flow-driver output")
    }

    fn count(body: &str, bpe: &tiktoken_rs::CoreBPE) -> Counts {
        Counts {
            bytes: body.len(),
            tokens: bpe.encode_with_special_tokens(body).len(),
        }
    }

    fn sum_counts(left: &Counts, right: &Counts) -> Counts {
        Counts {
            bytes: left.bytes + right.bytes,
            tokens: left.tokens + right.tokens,
        }
    }

    fn fingerprint(parts: impl IntoIterator<Item = String>) -> String {
        let mut hasher = blake3::Hasher::new();
        for part in parts {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        format!("blake3:{}", hasher.finalize().to_hex())
    }

    /// The model-visible text of one transaction (see `model_visible`).
    fn model_visible_text(transaction: &Transaction) -> String {
        let request: serde_json::Value =
            serde_json::from_str(&transaction.request_body).expect("request JSON");
        let response: serde_json::Value =
            serde_json::from_str(&transaction.response_body).expect("response JSON");
        let result = &response["result"];
        if request["method"] == "tools/list" {
            return result["tools"].to_string();
        }
        let params = &request["params"];
        format!(
            "{}{}{}",
            params["name"].as_str().expect("tool name"),
            params["arguments"],
            result["content"][0]["text"].as_str().expect("text block"),
        )
    }

    fn build_output(driver: DriverOutput) -> CalibrationOutput {
        let bpe = tiktoken_rs::o200k_base().expect("load bundled o200k_base vocabulary");
        let mut grouped: BTreeMap<String, Vec<Transaction>> = BTreeMap::new();
        for transaction in driver.transactions {
            grouped
                .entry(transaction.flow.clone())
                .or_default()
                .push(transaction);
        }

        let mut fixture_parts = Vec::new();
        let mut flows = BTreeMap::new();
        for (flow_name, transactions) in grouped {
            let mut operations = BTreeMap::new();
            let mut flow_total = Counts {
                bytes: 0,
                tokens: 0,
            };
            let mut flow_model = Counts {
                bytes: 0,
                tokens: 0,
            };
            let mut flow_parts = Vec::new();
            for transaction in transactions {
                let request = count(&transaction.request_body, &bpe);
                let response = count(&transaction.response_body, &bpe);
                let total = sum_counts(&request, &response);
                let model_visible = count(&model_visible_text(&transaction), &bpe);
                flow_total = sum_counts(&flow_total, &total);
                flow_model = sum_counts(&flow_model, &model_visible);
                flow_parts.extend([
                    transaction.operation.clone(),
                    transaction.request_body.clone(),
                    transaction.response_body.clone(),
                ]);
                fixture_parts.extend([
                    transaction.operation.clone(),
                    transaction.request_body.clone(),
                    transaction.response_body.clone(),
                ]);
                operations.insert(
                    transaction.operation,
                    OperationMeasurement {
                        request,
                        response,
                        total,
                        model_visible,
                    },
                );
            }
            flows.insert(
                flow_name,
                FlowMeasurement {
                    flow_version: CANONICAL_FLOW_VERSION,
                    flow_fingerprint: fingerprint(flow_parts),
                    operations,
                    total: flow_total,
                    model_visible: flow_model,
                },
            );
        }

        CalibrationOutput {
            schema_version: 1,
            runtime_build: env!("CARGO_PKG_VERSION"),
            tokenizer: TokenizerIdentity {
                name: "o200k_base",
                implementation: "tiktoken-rs",
                version: "0.12.0",
                vocab_fingerprint: VOCAB_FINGERPRINT,
                counting_policy: "each canonical JSON-RPC body independently; UTF-8 bytes; encode_with_special_tokens; operation and flow totals are integer sums; model_visible counts tool name + arguments + result text only",
            },
            fixture_fingerprint: fingerprint(fixture_parts),
            flows,
        }
    }

    fn parse_output_path() -> PathBuf {
        let args: Vec<String> = std::env::args().collect();
        match args.as_slice() {
            [_, flag, path] if flag == "--output" => PathBuf::from(path),
            _ => panic!("usage: token_footprint_calibration --output <path>"),
        }
    }

    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        let output_path = parse_output_path();
        let runtime = HeadlessRuntime::new(headless_config()).await?;
        prepare_scene(&runtime).await;
        let scene = scene_handle(&runtime).await;
        let (portal_tx, portal_rx) = mpsc::unbounded_channel();
        let portal_task = spawn_portal_driver(portal_rx);
        let shutdown = ShutdownToken::new();
        let config = McpServerConfig {
            bind_addrs: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted(PSK).shared(),
        };
        let (server_task, addresses) =
            start_mcp_http_server(scene, config, shutdown.clone(), Some(portal_tx.clone())).await?;
        let driver_output =
            tokio::task::spawn_blocking(move || run_python_driver(addresses[0])).await?;
        let output = build_output(driver_output);
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&output_path, serde_json::to_vec_pretty(&output)?)?;
        shutdown.trigger(ShutdownReason::Clean);
        server_task.await?;
        drop(portal_tx);
        portal_task.await?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn summing_counts_is_exact_integer_addition() {
            assert_eq!(
                sum_counts(
                    &Counts {
                        bytes: 1,
                        tokens: 2
                    },
                    &Counts {
                        bytes: 3,
                        tokens: 5
                    }
                )
                .bytes,
                4
            );
            assert_eq!(
                sum_counts(
                    &Counts {
                        bytes: 1,
                        tokens: 2
                    },
                    &Counts {
                        bytes: 3,
                        tokens: 5
                    }
                )
                .tokens,
                7
            );
        }

        #[test]
        fn length_prefixes_make_fingerprints_unambiguous() {
            assert_ne!(
                fingerprint(["ab".to_string(), "c".to_string()]),
                fingerprint(["a".to_string(), "bc".to_string()])
            );
        }
    }
}

#[cfg(feature = "headless")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    calibration::run().await
}

#[cfg(not(feature = "headless"))]
fn main() {
    eprintln!("token_footprint_calibration requires --features headless");
    std::process::exit(2);
}
