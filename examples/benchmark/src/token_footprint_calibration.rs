//! Deterministic Layer-3 token-footprint calibration for canonical MCP flows.

#[cfg(feature = "headless")]
mod calibration {
    use serde::{Deserialize, Serialize};
    use std::collections::{BTreeMap, HashMap};
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use tze_hud_runtime::headless::{HeadlessConfig, HeadlessRuntime};
    use tze_hud_runtime::portal::{PortalHandle, PortalHub, PortalKey};
    use tze_hud_runtime::threads::{ShutdownReason, ShutdownToken};
    use tze_hud_runtime::{McpServerConfig, start_mcp_http_server_with_render_wake};
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, RenderingPolicy, SceneId, WidgetDefinition,
        WidgetInstance, WidgetParamConstraints, WidgetParamType, WidgetParameterDeclaration,
        WidgetParameterValue, ZoneContent, ZoneInteractionKind,
    };

    const PSK: &str = "token-calibration-resident-psk";
    const CANONICAL_FLOW_VERSION: u32 = 4;
    const VOCAB_FINGERPRINT: &str =
        "sha256:446a9538cb6c348e3516120d7c08b09f57c36495e2acfffe59a5bf8b0cfb1a2d";

    #[derive(Debug, Deserialize)]
    struct DriverOutput {
        transactions: Vec<Transaction>,
        notification_action_limit_error: Transaction,
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
    struct ErrorCandidateMeasurement {
        transaction: OperationMeasurement,
        result_text: Counts,
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
        notification_action_limit_error: ErrorCandidateMeasurement,
        client_layer: ClientLayerMeasurement,
    }

    #[derive(Debug, Serialize)]
    struct ClientLayerMeasurement {
        skill_bodies: BTreeMap<String, Counts>,
        model_turns: BTreeMap<String, usize>,
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
                    audience: Default::default(),
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
                    audience: Default::default(),
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
            auto_clear_ms: None,
        });
    }

    async fn scene_handle(
        runtime: &HeadlessRuntime,
    ) -> std::sync::Arc<tokio::sync::Mutex<tze_hud_scene::graph::SceneGraph>> {
        let state = runtime.shared_state().lock().await;
        state.scene.clone()
    }

    fn run_python_driver(
        address: SocketAddr,
        choice_ready: mpsc::Receiver<()>,
        producer: FixtureProducer,
    ) -> DriverOutput {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let driver = root.join("examples/benchmark/token_footprint_flow.py");
        let mut child = Command::new("python3")
            .arg(driver)
            .env("HUD_MCP_URL", format!("http://{address}/mcp"))
            .env("HUD_PSK", PSK)
            .env(
                "HUD_MAX_NOTIFICATION_ACTIONS",
                tze_hud_scene::types::MAX_NOTIFICATION_ACTIONS.to_string(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch token-footprint Python flow driver");
        // Fixture-only barrier: a default (zero-wait) hud_input is sufficient
        // after the real shared activation has queued its answer. Never race
        // the producer or replace the answer with a synthetic PendingAction.
        let readiness = choice_ready
            .recv_timeout(std::time::Duration::from_secs(60))
            .map_err(|error| format!("choice activation was not ready: {error}"))
            .and_then(|()| {
                child
                    .stdin
                    .take()
                    .expect("piped flow-driver stdin")
                    .write_all(b"notification-choice-ready\n")
                    .map_err(|error| format!("notify flow driver of choice activation: {error}"))
            });
        if let Err(error) = readiness {
            let _ = child.kill();
            let output = child.wait_with_output().expect("reap failed flow driver");
            panic!("{error}: {}", String::from_utf8_lossy(&output.stderr));
        }
        // The driver reaches this fixture-only barrier after the old poll/ACK
        // and error flow are complete. Re-arm scene work only for the new ask.
        let marker = b"notification-choice-wait-start\n";
        let mut actual = vec![0; marker.len()];
        child
            .stdout
            .as_mut()
            .expect("piped driver stdout")
            .read_exact(&mut actual)
            .expect("read second choice barrier");
        assert_eq!(actual, marker, "driver second choice barrier");
        *producer.phase.lock().expect("calibration phase") = PortalPhase::ChoiceAllowed;
        producer.changed.notify_one();
        let output = child
            .wait_with_output()
            .expect("wait for Python flow driver");
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
        let candidate = &driver.notification_action_limit_error;
        let response: serde_json::Value =
            serde_json::from_str(&candidate.response_body).expect("candidate response JSON");
        let request = count(&candidate.request_body, &bpe);
        let response_counts = count(&candidate.response_body, &bpe);
        let notification_action_limit_error = ErrorCandidateMeasurement {
            result_text: count(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .expect("candidate result text"),
                &bpe,
            ),
            transaction: OperationMeasurement {
                total: sum_counts(&request, &response_counts),
                request,
                response: response_counts,
                model_visible: count(&model_visible_text(candidate), &bpe),
            },
        };
        let mut grouped: BTreeMap<String, Vec<Transaction>> = BTreeMap::new();
        for transaction in driver.transactions {
            grouped
                .entry(transaction.flow.clone())
                .or_default()
                .push(transaction);
        }

        let mut fixture_parts = Vec::new();
        let mut flows = BTreeMap::new();
        let mut model_turns = BTreeMap::new();
        for (flow_name, transactions) in grouped {
            model_turns.insert(
                flow_name.clone(),
                transactions
                    .iter()
                    .filter(|transaction| {
                        let request: serde_json::Value =
                            serde_json::from_str(&transaction.request_body).expect("request JSON");
                        request["method"] == "tools/call"
                    })
                    .count(),
            );
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
            if flow_name == "notification_choice" {
                assert_eq!(
                    operations.len(),
                    3,
                    "ask, returned answer and ACK are all counted"
                );
            }
            if flow_name == "notification_choice_wait" {
                assert_eq!(operations.len(), 1, "ask and answer share one call");
                assert!(
                    flow_model.tokens <= 90,
                    "one-call choice exceeds its signed ceiling"
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

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let skill_bodies = ["th-hud-publish", "hud-projection"]
            .into_iter()
            .map(|name| {
                let path = root.join(".claude/skills").join(name).join("SKILL.md");
                let body = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                (name.to_string(), count(&body, &bpe))
            })
            .collect();
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
            notification_action_limit_error,
            client_layer: ClientLayerMeasurement {
                skill_bodies,
                model_turns,
            },
        }
    }

    fn parse_output_path() -> PathBuf {
        let args: Vec<String> = std::env::args().collect();
        match args.as_slice() {
            [_, flag, path] if flag == "--output" => PathBuf::from(path),
            _ => panic!("usage: token_footprint_calibration --output <path>"),
        }
    }

    fn server_config() -> McpServerConfig {
        McpServerConfig {
            publication_defaults: Default::default(),
            widget_transition_ms: tze_hud_config::tokens::resolve_motion_duration_ms(
                &Default::default(),
                "motion.state.ms",
            ),
            bind_addrs: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted(PSK).shared(),
            presents: None,
            surfaces: None,
            capture: None,
            restart: None,
            update: None,
            bind_gate: None,
            pairing: None,
            appearance: None,
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PortalPhase {
        AwaitAttach,
        AwaitClear,
        ChoiceAllowed,
        ChoiceActivated,
    }

    // Fixture ordering only. Choice work still owns SharedState -> scene and
    // real activation; no runtime input/deadline/retention contract changes.
    #[derive(Clone)]
    struct FixtureProducer {
        portals: PortalHandle,
        key: PortalKey,
        phase: std::sync::Arc<std::sync::Mutex<PortalPhase>>,
        input_ready: std::sync::Arc<tokio::sync::Notify>,
        changed: std::sync::Arc<tokio::sync::Notify>,
    }

    impl FixtureProducer {
        fn new(portals: PortalHandle, agent: String) -> Self {
            let input_ready = portals
                .input_notify(&agent)
                .expect("calibration input readiness");
            Self {
                portals,
                key: PortalKey::new(agent, "claude-main"),
                phase: std::sync::Arc::new(std::sync::Mutex::new(PortalPhase::AwaitAttach)),
                input_ready,
                changed: Default::default(),
            }
        }

        fn ingress_wake(&self) -> tze_hud_scene::render_wake::RenderWakeNotifier {
            let (portals, key, phase, input_ready, changed) = (
                self.portals.clone(),
                self.key.clone(),
                self.phase.clone(),
                self.input_ready.clone(),
                self.changed.clone(),
            );
            tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
                // MCP calls this synchronously after Hub unlock, before sending
                // its already-snapshotted response. Never acquire the scene here.
                let (injected, cleared) = {
                    let mut phase = phase.lock().expect("calibration phase");
                    if matches!(
                        *phase,
                        PortalPhase::ChoiceAllowed | PortalPhase::ChoiceActivated
                    ) {
                        return;
                    }
                    let mut hub = portals.hub.lock().expect("calibration hub");
                    match *phase {
                        PortalPhase::AwaitAttach if hub.get(&key).is_some() => {
                            hub.inject_input(
                                &key,
                                "canonical-input-0001",
                                "Canonical HUD-originated input.",
                            )
                            .expect("inject canonical portal input");
                            *phase = PortalPhase::AwaitClear;
                            (true, false)
                        }
                        PortalPhase::AwaitClear if hub.get(&key).is_none() => {
                            *phase = PortalPhase::ChoiceAllowed;
                            (false, true)
                        }
                        _ => (false, false),
                    }
                };
                // Readiness never owns an answer, and no guard crosses a signal.
                if injected {
                    input_ready.notify_waiters();
                }
                if cleared {
                    changed.notify_one();
                }
            })
        }

        fn choice_work_pending(&self) -> bool {
            *self.phase.lock().expect("calibration phase") == PortalPhase::ChoiceAllowed
        }

        async fn step(&mut self, choice_work: impl std::future::Future<Output = bool>) -> bool {
            if self.choice_work_pending() && choice_work.await {
                *self.phase.lock().expect("calibration phase") = PortalPhase::ChoiceActivated;
                return true;
            }
            false
        }
    }

    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        let output_path = parse_output_path();
        let runtime = HeadlessRuntime::new(headless_config()).await?;
        prepare_scene(&runtime).await;
        let scene = scene_handle(&runtime).await;
        let portals = PortalHandle::new(
            PortalHub::default(),
            std::sync::Arc::new(tze_hud_scene::SystemClock::new()),
        );
        let shutdown = ShutdownToken::new();
        let agent = tze_hud_scene::config::AgentDirectory::unrestricted(PSK)
            .resolve(PSK, "")
            .expect("calibration agent")
            .agent_id;
        let mut producer = FixtureProducer::new(portals.clone(), agent.clone());
        let changed = producer.changed.clone();
        let (server_task, addresses) = start_mcp_http_server_with_render_wake(
            scene,
            server_config(),
            shutdown.clone(),
            Some(portals.clone()),
            Default::default(),
            producer.ingress_wake(),
            Default::default(),
        )
        .await?;
        let (choice_ready_tx, choice_ready_rx) = mpsc::channel();
        let driver_producer = producer.clone();
        let mut python = tokio::task::spawn_blocking(move || {
            run_python_driver(addresses[0], choice_ready_rx, driver_producer)
        });
        let mut choice_barrier_sent = false;
        let driver_output = loop {
            let activated = producer
                .step(async {
                    let shared = runtime.shared_state().lock().await;
                    let mut scene = shared.scene.lock().await;
                    // Do not shape earlier notifications while the independent
                    // portal flow needs this scene lock. Wait for the choice itself.
                    let choice_pending = scene
                        .zone_registry
                        .active_publishes
                        .values()
                        .flatten()
                        .any(|record| {
                            if record.publisher_namespace != agent {
                                return false;
                            }
                            match &record.content {
                                ZoneContent::Notification(notification) => notification
                                    .actions
                                    .iter()
                                    .any(|action| action.callback_id.as_deref() == Some("c")),
                                _ => false,
                            }
                        });
                    let wake = if choice_pending {
                        // Exercise the production exact-live activation operation,
                        // shared by pointer and keyboard. Never inject a PendingAction.
                        runtime
                            .compositor
                            .populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);
                        let target = scene
                            .overlay
                            .zone_hit_regions
                            .iter()
                            .find(|region| {
                                region.publisher_namespace == agent
                                    && matches!(
                                        &region.kind,
                                        ZoneInteractionKind::Action { index: 2 }
                                    )
                            })
                            .cloned();
                        target.and_then(|region| scene.activate_notification(&region, 0))
                    } else {
                        None
                    };
                    drop(scene);
                    drop(shared);
                    if let Some(tze_hud_scene::graph::NotificationActivation::Callback(wake)) = wake
                    {
                        wake.notify();
                        true
                    } else {
                        false
                    }
                })
                .await;
            if activated && !choice_barrier_sent {
                choice_barrier_sent = true;
                choice_ready_tx
                    .send(())
                    .expect("notify driver after shared choice activation");
            }
            tokio::select! {
                result = &mut python => break result?,
                _ = changed.notified() => {},
                _ = tokio::time::sleep(std::time::Duration::from_millis(1)),
                    if producer.choice_work_pending() => {}
            }
        };
        let output = build_output(driver_output);
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&output_path, serde_json::to_vec_pretty(&output)?)?;
        shutdown.trigger(ShutdownReason::Clean);
        server_task.await?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // A plain scene and loopback MCP exercise the real producer scheduling
        // boundary; neither this parent nor its injected work constructs a GPU.
        #[tokio::test]
        async fn portal_lifecycle_excludes_fixture_scene_work_from_input() {
            use serde_json::{Value, json};
            use std::future::Future;
            use std::sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            };
            use std::task::{Context, Waker};
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            async fn call(address: SocketAddr, name: &str, args: Value) -> (bool, Value) {
                let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":name,"arguments":args}})
                .to_string();
                let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
                let request = format!(
                    "POST /mcp HTTP/1.0\r\nAuthorization: Bearer {PSK}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut raw = Vec::new();
                stream.read_to_end(&mut raw).await.unwrap();
                let raw = String::from_utf8(raw).unwrap();
                let (header, body) = raw.split_once("\r\n\r\n").unwrap();
                assert!(
                    header.starts_with("HTTP/1.1 200") || header.starts_with("HTTP/1.0 200"),
                    "{raw}"
                );
                let response: Value = serde_json::from_str(body).unwrap();
                assert!(response.get("error").is_none(), "{response}");
                let result = &response["result"];
                (
                    result["isError"].as_bool().unwrap_or(false),
                    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap(),
                )
            }

            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                let mut overlap_failures = Vec::new();
                for case in ["poll", "ack", "controls"] {
                    if case == "controls" && !overlap_failures.is_empty() {
                        continue; // Report both semantic RED rows before positive controls.
                    }
                    let mut graph = tze_hud_scene::graph::SceneGraph::new(1920.0, 1080.0);
                    graph.zone_registry = tze_hud_scene::ZoneRegistry::with_defaults();
                    let scene = Arc::new(tokio::sync::Mutex::new(graph));
                    let portals = PortalHandle::new(PortalHub::default(), Arc::new(tze_hud_scene::SystemClock::new()));
                    let agent = tze_hud_scene::config::AgentDirectory::unrestricted(PSK).resolve(PSK, "").unwrap().agent_id;
                    let key = PortalKey::new(agent.clone(), "claude-main");
                    let mut producer = FixtureProducer::new(portals.clone(), agent.clone());
                    let ingress = producer.ingress_wake();
                    let shutdown = ShutdownToken::new();
                    let (server, addresses) = start_mcp_http_server_with_render_wake(
                        scene.clone(), server_config(), shutdown.clone(), Some(portals.clone()),
                        Default::default(), ingress.clone(), Default::default(),
                    ).await.unwrap();
                    let address = addresses[0];

                    if case == "controls" {
                        let work = AtomicUsize::new(0);
                        ingress.notify(); // Boot absence never means clear.
                        producer.step(async { work.fetch_add(1, Ordering::SeqCst); false }).await;
                        assert_eq!(work.load(Ordering::SeqCst), 0);
                        assert!(!producer.choice_work_pending());
                        let foreign = call(address, "hud_publish", json!({"surface":"portal:foreign", "content":"other"})).await;
                        assert!(!foreign.0);
                        ingress.notify();
                        producer.step(async { work.fetch_add(1, Ordering::SeqCst); false }).await;
                        assert_eq!(work.load(Ordering::SeqCst), 0);
                        assert!(portals.hub.lock().unwrap().get(&PortalKey::new(agent.clone(), "foreign")).unwrap().inputs.is_empty());
                        let attached = call(address, "hud_publish", json!({"surface":"portal:claude-main", "content":"Tests pass. Ship it?", "status":"active", "expects_reply":true, "ttl_ms":120000})).await;
                        assert_eq!(attached, (false, json!({"ok":true,"reattached":false,"pending_input":0,"expires_in_ms":120000})));
                        // Repeated ingress, real delivery/lease and ACK must neither
                        // re-inject nor authorize choice work while this key exists.
                        ingress.notify();
                        ingress.notify();
                        assert_eq!(portals.hub.lock().unwrap().get(&key).unwrap().inputs.len(), 1);
                        let polled = call(address, "hud_input", json!({"wait_ms":1000})).await;
                        assert_eq!(polled, (false, json!({"items":[{"id":"canonical-input-0001","s":"portal:claude-main","text":"Canonical HUD-originated input."}],"remaining":0})));
                        let acked = call(address, "hud_input", json!({"ack":[polled.1["items"][0]["id"]]})).await;
                        assert_eq!(acked, (false, json!({"items":[],"remaining":0})));
                        assert!(portals.hub.lock().unwrap().get(&key).unwrap().inputs.is_empty());
                        call(address, "hud_clear", json!({"surface":"portal:foreign"})).await;
                        producer.step(async { work.fetch_add(1, Ordering::SeqCst); false }).await;
                        assert_eq!(work.load(Ordering::SeqCst), 0, "foreign clear is not this portal clear");
                        assert!(!call(address, "hud_clear", json!({"surface":"portal:claude-main"})).await.0);
                        assert!(producer.choice_work_pending());
                        let before = {
                            let scene = scene.lock().await;
                            (scene.version, scene.leases.clone(), scene.zone_registry.active_publishes.len())
                        };
                        let rejected = call(address, "hud_publish", json!({"surface":"zone:subtitles","content":"hello"})).await;
                        let mut names: Vec<_> = scene.lock().await.zone_registry.zones.keys().cloned().collect();
                        names.sort();
                        assert_eq!(rejected, (true, json!({"code":"ZONE_NOT_FOUND","hint":format!("no zone subtitles; known: {}", names.join(", "))})));
                        let after = scene.lock().await;
                        assert_eq!((after.version, after.leases.clone(), after.zone_registry.active_publishes.len()), before);
                        drop(after);
                        // This closure witnesses only permission for scene work;
                        // canonical calibration retains actual layout/live activation.
                        assert!(producer.step(async { work.fetch_add(1, Ordering::SeqCst); true }).await);
                        assert_eq!(work.load(Ordering::SeqCst), 1);
                        ingress.notify();
                        assert!(portals.hub.lock().unwrap().get(&key).is_none());
                        assert!(!producer.step(async { panic!("no post-readiness scene work"); }).await);
                        assert!(!producer.choice_work_pending());
                    } else {
                        let attached = call(address, "hud_publish", json!({"surface":"portal:claude-main", "content":"Tests pass. Ship it?", "status":"active", "expects_reply":true, "ttl_ms":120000})).await;
                        assert_eq!(attached, (false, json!({"ok":true,"reattached":false,"pending_input":0,"expires_in_ms":120000})));
                        let mut returned_id = None;
                        if case == "ack" {
                            producer.step(async { let _held = scene.lock().await; false }).await;
                            let polled = call(address, "hud_input", json!({"wait_ms":1000})).await;
                            assert!(!polled.0, "{polled:?}");
                            returned_id = Some(polled.1["items"][0]["id"].as_str().unwrap().to_string());
                        }
                        let acquired = AtomicUsize::new(0);
                        // Manually poll the actual step once. Only if it acquires
                        // its actual scene-work guard does this test-only seam hold
                        // it pending across real HTTP; no sleep decides ownership.
                        let mut step = Box::pin(producer.step(async {
                            let _held = scene.lock().await;
                            acquired.fetch_add(1, Ordering::SeqCst);
                            std::future::pending::<()>().await;
                            false
                        }));
                        let _ = step.as_mut().poll(&mut Context::from_waker(Waker::noop()));
                        let args = match &returned_id {
                            Some(id) => json!({"ack":[id]}), // omitted wait_ms remains0
                            None => json!({"wait_ms":1000}),
                        };
                        let result = call(address, "hud_input", args.clone()).await;
                        drop(step);
                        if result.0 {
                            assert_eq!(acquired.load(Ordering::SeqCst), 1);
                            assert_eq!(result.1, json!({"code":"INTERNAL","hint":"input state busy; retry hud_input"}));
                            eprintln!("controlled overlap RED: {case} args={args} result={:?}", result.1);
                            overlap_failures.push(case);
                        } else {
                            assert_eq!(acquired.load(Ordering::SeqCst), 0, "fixture must not probe scene through poll/ACK");
                            let expected = if case == "poll" {
                                json!({"items":[{"id":"canonical-input-0001","s":"portal:claude-main","text":"Canonical HUD-originated input."}],"remaining":0})
                            } else { json!({"items":[],"remaining":0}) };
                            assert_eq!(result.1, expected);
                            // A different owner's real scene contention remains a
                            // bounded error after the fixture overlap is repaired.
                            let held = scene.lock().await;
                            let busy = call(address, "hud_input", args).await;
                            assert_eq!(busy, (true, json!({"code":"INTERNAL","hint":"input state busy; retry hud_input"})));
                            drop(held);
                        }
                    }
                    shutdown.trigger(ShutdownReason::Clean);
                    server.await.unwrap();
                }
                assert!(overlap_failures.is_empty(), "fixture must avoid controlled scene overlap for real MCP calls; failed rows: {overlap_failures:?}");
            }).await.expect("bounded calibration scheduling parent");
        }

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
