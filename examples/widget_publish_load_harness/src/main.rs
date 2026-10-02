use std::collections::{BTreeMap, HashMap};
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use prost::Message;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant as TokioInstant, sleep, sleep_until, timeout};
use tokio_stream::wrappers::ReceiverStream;
use tze_hud_protocol::proto::WidgetParameterValueProto;
use tze_hud_protocol::proto::session as session_proto;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_telemetry::{
    ByteAccountingMode, PublishLoadArtifact, PublishLoadCalibrationStatus, PublishLoadIdentity,
    PublishLoadMetrics, PublishLoadMode, PublishLoadThresholds, PublishLoadTraceability,
    PublishLoadTransport, PublishLoadVerdict,
};

type DynError = Box<dyn Error + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkloadMode {
    Burst,
    Paced,
}

impl WorkloadMode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "burst" => Ok(Self::Burst),
            "paced" => Ok(Self::Paced),
            other => Err(format!("invalid mode '{other}' (expected burst|paced)")),
        }
    }

    fn as_publish_mode(self) -> PublishLoadMode {
        match self {
            Self::Burst => PublishLoadMode::Burst,
            Self::Paced => PublishLoadMode::Paced,
        }
    }
}

#[derive(Debug)]
struct Cli {
    target_id: String,
    targets_file: PathBuf,
    mode: WorkloadMode,
    publish_count: Option<u64>,
    duration_s: Option<f64>,
    target_rate_rps: Option<f64>,
    widget_name: String,
    instance_id: String,
    payload_profile: String,
    param_name: String,
    param_start: f32,
    param_step: f32,
    transition_ms: u32,
    timeout_s: f64,
    output: PathBuf,
    agent_id: String,
    psk_override: Option<String>,
    normalization_mapping_approved: bool,
    target_p99_rtt_us: Option<u64>,
    target_throughput_rps: Option<f64>,
}

impl Cli {
    fn parse() -> Result<Self, String> {
        Self::parse_from(env::args().skip(1))
    }

    fn parse_from<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut kv = BTreeMap::<String, String>::new();
        let mut flags = Vec::<String>::new();

        let mut args = args.into_iter().peekable();
        while let Some(arg) = args.next() {
            if !arg.starts_with("--") {
                return Err(format!("unexpected positional argument: {arg}"));
            }
            if arg == "--help" || arg == "-h" {
                print_usage();
                std::process::exit(0);
            }

            let key = arg.trim_start_matches("--").to_string();
            match args.peek() {
                Some(next) if !next.starts_with("--") => {
                    let value = args.next().unwrap_or_default();
                    kv.insert(key, value);
                }
                _ => flags.push(key),
            }
        }

        validate_known_args(&kv, &flags)?;
        validate_transport_intent(kv.get("transport").map(String::as_str).unwrap_or("grpc"))?;
        validate_publish_intent(
            kv.get("publish-intent")
                .map(String::as_str)
                .unwrap_or("widget"),
        )?;

        let mode = WorkloadMode::parse(kv.get("mode").map(String::as_str).unwrap_or("burst"))?;

        let target_id = required_string(&kv, "target-id")?;
        let targets_file = PathBuf::from(
            kv.get("targets-file")
                .cloned()
                .unwrap_or_else(|| "./targets/publish_load_targets.toml".to_string()),
        );

        let publish_count = parse_opt_u64(&kv, "publish-count")?;
        let duration_s = parse_opt_f64(&kv, "duration-s")?;
        let target_rate_rps = parse_opt_f64(&kv, "target-rate-rps")?;

        match mode {
            WorkloadMode::Burst => {
                if let Some(count) = publish_count {
                    if count == 0 {
                        return Err(
                            "burst mode requires --publish-count > 0 (or omit for default 1000)"
                                .to_string(),
                        );
                    }
                }
            }
            WorkloadMode::Paced => {
                if target_rate_rps.unwrap_or(0.0) <= 0.0 {
                    return Err("paced mode requires --target-rate-rps > 0".to_string());
                }
                if publish_count.is_none() && duration_s.is_none() {
                    return Err(
                        "paced mode requires at least one bound: --publish-count or --duration-s"
                            .to_string(),
                    );
                }
            }
        }

        let timeout_s = kv
            .get("timeout-s")
            .map(|v| parse_f64(v, "timeout-s"))
            .transpose()?
            .unwrap_or(30.0);
        if timeout_s <= 0.0 {
            return Err("--timeout-s must be > 0".to_string());
        }

        let output = PathBuf::from(
            kv.get("output")
                .cloned()
                .unwrap_or_else(default_output_path),
        );

        Ok(Self {
            target_id,
            targets_file,
            mode,
            publish_count: match mode {
                WorkloadMode::Burst => Some(publish_count.unwrap_or(1000)),
                WorkloadMode::Paced => publish_count,
            },
            duration_s,
            target_rate_rps,
            widget_name: kv
                .get("widget-name")
                .cloned()
                .unwrap_or_else(|| "gauge".to_string()),
            instance_id: kv
                .get("instance-id")
                .cloned()
                .unwrap_or_else(|| "publish-load-harness".to_string()),
            payload_profile: kv
                .get("payload-profile")
                .cloned()
                .unwrap_or_else(|| "gauge_default".to_string()),
            param_name: kv
                .get("param-name")
                .cloned()
                .unwrap_or_else(|| "value".to_string()),
            param_start: kv
                .get("param-start")
                .map(|v| parse_f32(v, "param-start"))
                .transpose()?
                .unwrap_or(0.0),
            param_step: kv
                .get("param-step")
                .map(|v| parse_f32(v, "param-step"))
                .transpose()?
                .unwrap_or(1.0),
            transition_ms: kv
                .get("transition-ms")
                .map(|v| parse_u32(v, "transition-ms"))
                .transpose()?
                .unwrap_or(0),
            timeout_s,
            output,
            agent_id: kv
                .get("agent-id")
                .cloned()
                .unwrap_or_else(|| "widget-publish-load-harness".to_string()),
            psk_override: kv.get("psk").cloned(),
            normalization_mapping_approved: flags
                .iter()
                .any(|f| f == "normalization-mapping-approved"),
            target_p99_rtt_us: parse_opt_u64(&kv, "target-p99-rtt-us")?,
            target_throughput_rps: parse_opt_f64(&kv, "target-throughput-rps")?,
        })
    }
}

const SUPPORTED_KV_ARGS: &[&str] = &[
    "target-id",
    "targets-file",
    "mode",
    "publish-count",
    "duration-s",
    "target-rate-rps",
    "widget-name",
    "instance-id",
    "payload-profile",
    "param-name",
    "param-start",
    "param-step",
    "transition-ms",
    "timeout-s",
    "output",
    "agent-id",
    "psk",
    "target-p99-rtt-us",
    "target-throughput-rps",
    "transport",
    "publish-intent",
];

const SUPPORTED_FLAGS: &[&str] = &["normalization-mapping-approved"];

fn validate_known_args(kv: &BTreeMap<String, String>, flags: &[String]) -> Result<(), String> {
    for key in kv.keys() {
        if !SUPPORTED_KV_ARGS.contains(&key.as_str()) {
            return Err(format!("unsupported argument --{key}"));
        }
    }

    for flag in flags {
        if !SUPPORTED_FLAGS.contains(&flag.as_str()) {
            return Err(format!("unsupported flag --{flag}"));
        }
    }

    Ok(())
}

fn validate_transport_intent(raw: &str) -> Result<(), String> {
    match raw {
        "grpc" => Ok(()),
        "mcp" => Err(
            "unsupported transport intent 'mcp' (initial release supports only --transport grpc)"
                .to_string(),
        ),
        other => Err(format!(
            "invalid --transport '{other}' (expected grpc; mcp is not yet supported)"
        )),
    }
}

fn validate_publish_intent(raw: &str) -> Result<(), String> {
    match raw {
        "widget" => Ok(()),
        "zone" | "tile" => Err(format!(
            "unsupported publish intent '{raw}' (initial release supports only --publish-intent widget)"
        )),
        other => Err(format!(
            "invalid --publish-intent '{other}' (expected widget; zone/tile are not yet supported)"
        )),
    }
}

#[derive(Debug, Deserialize)]
struct TargetRegistry {
    #[serde(default)]
    targets: Vec<TargetConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct TargetConfig {
    #[serde(alias = "id")]
    target_id: String,
    #[serde(alias = "endpoint")]
    grpc_endpoint: String,
    #[serde(alias = "host")]
    target_host: String,
    #[serde(default = "default_network_scope")]
    network_scope: String,
    #[serde(default = "default_psk_env")]
    psk_env: String,
}

fn default_network_scope() -> String {
    "tailnet".to_string()
}

fn default_psk_env() -> String {
    "MCP_TEST_PSK".to_string()
}

#[derive(Debug, Default)]
struct RunStats {
    request_count: u64,
    success_count: u64,
    error_count: u64,
    rtt_us: Vec<u64>,
    payload_bytes_out: u64,
    payload_bytes_in: u64,
    aggregate_send_time_us: u64,
    aggregate_ack_drain_time_us: u64,
    warnings: Vec<String>,
}

struct RunState {
    inflight: Mutex<HashMap<u64, Instant>>,
    stats: Mutex<RunStats>,
    ack_count: AtomicU64,
    stream_terminal: Mutex<Option<String>>,
    ack_notify: Notify,
}

impl Default for RunState {
    fn default() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
            stats: Mutex::new(RunStats::default()),
            ack_count: AtomicU64::new(0),
            stream_terminal: Mutex::new(None),
            ack_notify: Notify::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AckDrainOutcome {
    Complete,
    StreamEnded,
    DeadlineExceeded,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), DynError> {
    let cli = Cli::parse().map_err(|e| format!("argument error: {e}"))?;
    let target = resolve_target(&cli.targets_file, &cli.target_id)?;

    let psk = match cli.psk_override.clone() {
        Some(psk) => psk,
        None => env::var(&target.psk_env)
            .map_err(|_| format!("PSK not set: expected env var '{}'", target.psk_env))?,
    };

    let endpoint = normalize_endpoint(&target.grpc_endpoint);
    let mut client = HudSessionClient::connect(endpoint.clone()).await?;

    let (tx, rx) = mpsc::channel::<session_proto::ClientMessage>(2048);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.session(stream).await?.into_inner();

    let mut client_seq: u64 = 1;
    let init = build_session_init(&cli, &psk, client_seq);
    tx.send(init).await?;
    client_seq += 1;

    wait_for_session_established(&mut response_stream, Duration::from_secs_f64(cli.timeout_s))
        .await?;

    let run_state = Arc::new(RunState::default());
    let mut response_drain_handle = spawn_response_drain(response_stream, Arc::clone(&run_state));
    let send_start = Instant::now();

    match cli.mode {
        WorkloadMode::Burst => {
            let count = cli
                .publish_count
                .expect("burst mode always has publish_count");
            for i in 0..count {
                send_publish(&tx, &mut client_seq, &cli, i, &run_state).await?;
            }
        }
        WorkloadMode::Paced => {
            let target_rate = cli
                .target_rate_rps
                .expect("paced mode always has target_rate_rps");
            let interval = Duration::from_secs_f64(1.0 / target_rate);
            let pace_start = Instant::now();
            let max_count = cli.publish_count;
            let max_duration = cli.duration_s.map(Duration::from_secs_f64);

            let mut index: u64 = 0;
            loop {
                if let Some(limit) = max_count {
                    if index >= limit {
                        break;
                    }
                }
                if let Some(limit) = max_duration {
                    if pace_start.elapsed() >= limit {
                        break;
                    }
                }

                let due = pace_start + interval.mul_f64(index as f64);
                let now = Instant::now();
                if due > now {
                    sleep_until(TokioInstant::from_std(due)).await;
                }

                send_publish(&tx, &mut client_seq, &cli, index, &run_state).await?;
                index += 1;
            }
        }
    }

    let send_done = Instant::now();
    let expected_acks = {
        let stats = run_state.stats.lock().await;
        stats.request_count
    };
    let ack_drain_outcome = wait_for_expected_acks(
        &run_state,
        expected_acks,
        Duration::from_secs_f64(cli.timeout_s),
    )
    .await?;
    let aggregate_ack_drain_time_us = elapsed_us(send_done, Instant::now());

    let close = session_proto::ClientMessage {
        sequence: client_seq,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionClose(session_proto::SessionClose {
            reason: "publish-load complete".to_string(),
        })),
    };
    let _ = tx.send(close).await;
    drop(tx);
    let drain_shutdown_warning = wait_for_response_drain_shutdown(&mut response_drain_handle).await;

    let mut stats = run_state.stats.lock().await;
    stats.aggregate_ack_drain_time_us = aggregate_ack_drain_time_us;

    if ack_drain_outcome != AckDrainOutcome::Complete {
        stats.warnings.push(format!(
            "final ack drain ended with outcome={:?} after timeout_s={:.3}",
            ack_drain_outcome, cli.timeout_s
        ));
    }
    if let Some(warning) = drain_shutdown_warning {
        stats.warnings.push(warning);
    }

    let missing_acks = expected_acks.saturating_sub(run_state.ack_count.load(Ordering::Relaxed));
    if missing_acks > 0 {
        stats
            .warnings
            .push(format!("{missing_acks} publishes missing durable acks"));
        let accounted_outcomes = stats.success_count.saturating_add(stats.error_count);
        let unaccounted_outcomes = stats.request_count.saturating_sub(accounted_outcomes);
        stats.error_count = stats
            .error_count
            .saturating_add(unaccounted_outcomes.min(missing_acks));
    }

    let stats = std::mem::take(&mut *stats);

    let wall_duration_us = elapsed_us(send_start, Instant::now());
    let throughput_rps = if wall_duration_us == 0 {
        0.0
    } else {
        stats.request_count as f64 / (wall_duration_us as f64 / 1_000_000.0)
    };

    let (rtt_p50_us, rtt_p95_us, rtt_p99_us, rtt_max_us) = percentile_summary(&stats.rtt_us);

    let uncalibrated = !cli.normalization_mapping_approved;
    let calibration_status = if uncalibrated {
        PublishLoadCalibrationStatus::Uncalibrated
    } else {
        PublishLoadCalibrationStatus::Calibrated
    };

    let threshold_comparisons_informational = uncalibrated;

    let verdict = if uncalibrated {
        PublishLoadVerdict::Uncalibrated
    } else {
        let latency_ok = cli
            .target_p99_rtt_us
            .map(|budget| rtt_p99_us <= budget)
            .unwrap_or(true);
        let throughput_ok = cli
            .target_throughput_rps
            .map(|budget| throughput_rps >= budget)
            .unwrap_or(true);
        if latency_ok && throughput_ok {
            PublishLoadVerdict::Pass
        } else {
            PublishLoadVerdict::Fail
        }
    };

    let identity = PublishLoadIdentity {
        target_id: target.target_id.clone(),
        target_host: target.target_host.clone(),
        network_scope: target.network_scope.clone(),
        transport: PublishLoadTransport::Grpc,
        mode: cli.mode.as_publish_mode(),
        widget_name: cli.widget_name.clone(),
        payload_profile: cli.payload_profile.clone(),
        publish_count: Some(stats.request_count),
        duration_s: cli.duration_s,
        target_rate_rps: cli.target_rate_rps,
    };

    let mut warnings = stats.warnings;
    if uncalibrated {
        warnings.push("normalization mapping not approved; verdict is informational".to_string());
    }

    let artifact = PublishLoadArtifact {
        benchmark_key: identity.stable_comparison_key(),
        identity,
        metrics: PublishLoadMetrics {
            request_count: stats.request_count,
            success_count: stats.success_count,
            error_count: stats.error_count,
            wall_duration_us,
            throughput_rps,
            rtt_p50_us,
            rtt_p95_us,
            rtt_p99_us,
            rtt_max_us,
            aggregate_send_time_us: stats.aggregate_send_time_us,
            aggregate_ack_drain_time_us: stats.aggregate_ack_drain_time_us,
            payload_bytes_out: stats.payload_bytes_out,
            payload_bytes_in: stats.payload_bytes_in,
            wire_bytes_out: None,
            wire_bytes_in: None,
        },
        byte_accounting_mode: ByteAccountingMode::PayloadOnly,
        thresholds: PublishLoadThresholds {
            target_p99_rtt_us: cli.target_p99_rtt_us,
            target_throughput_rps: cli.target_throughput_rps,
        },
        traceability: PublishLoadTraceability {
            spec_id: Some("publish-load-harness".to_string()),
            rfc_id: Some("RFC-0005".to_string()),
            budget_id: cli
                .target_p99_rtt_us
                .map(|_| "publish_load_widget_p99".to_string()),
            threshold_id: Some("publish_load_default".to_string()),
        },
        calibration_status,
        normalization_mapping_approved: cli.normalization_mapping_approved,
        threshold_comparisons_informational,
        verdict,
        warnings,
        histogram_path: None,
        calibration_path: None,
    };

    artifact
        .validate()
        .map_err(|e| format!("artifact validation failed: {e}"))?;

    write_artifact(&cli.output, &artifact)?;

    println!(
        "completed: target_id={} mode={:?} requests={} success={} errors={} p99_us={} throughput_rps={:.2} output={}",
        target.target_id,
        cli.mode,
        stats.request_count,
        stats.success_count,
        stats.error_count,
        rtt_p99_us,
        throughput_rps,
        cli.output.display(),
    );

    if missing_acks > 0 {
        return Err(format!(
            "publish-load incomplete: {missing_acks} WidgetPublishResult acks missing; diagnostic artifact written to {}",
            cli.output.display()
        )
        .into());
    }

    Ok(())
}

fn build_session_init(cli: &Cli, psk: &str, sequence: u64) -> session_proto::ClientMessage {
    session_proto::ClientMessage {
        sequence,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(session_proto::SessionInit {
            agent_id: cli.agent_id.clone(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(session_proto::AuthCredential {
                credential: Some(session_proto::auth_credential::Credential::PreSharedKey(
                    session_proto::PreSharedKeyCredential {
                        key: psk.to_string(),
                    },
                )),
            }),
        })),
    }
}

async fn wait_for_session_established(
    response_stream: &mut tonic::Streaming<session_proto::ServerMessage>,
    timeout_dur: Duration,
) -> Result<(), DynError> {
    loop {
        let next = timeout(timeout_dur, response_stream.message()).await??;
        let Some(message) = next else {
            return Err("stream closed before SessionEstablished".into());
        };

        match message.payload {
            Some(ServerPayload::SessionEstablished(_)) => return Ok(()),
            Some(ServerPayload::SessionError(err)) => {
                return Err(format!("session error: {} ({})", err.code, err.message).into());
            }
            _ => {}
        }
    }
}

async fn send_publish(
    tx: &mpsc::Sender<session_proto::ClientMessage>,
    next_seq: &mut u64,
    cli: &Cli,
    index: u64,
    run_state: &Arc<RunState>,
) -> Result<(), DynError> {
    let seq = *next_seq;
    let value = cli.param_start + (index as f32 * cli.param_step);

    let widget = if cli.instance_id.is_empty() {
        &cli.widget_name
    } else {
        &cli.instance_id
    };
    let publish = session_proto::Publish {
        surface: format!("widget:{widget}"),
        params: vec![WidgetParameterValueProto {
            param_name: cli.param_name.clone(),
            value: Some(
                tze_hud_protocol::proto::widget_parameter_value_proto::Value::F32Value(value),
            ),
        }],
        transition_ms: cli.transition_ms,
        ..Default::default()
    };

    let msg = session_proto::ClientMessage {
        sequence: seq,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(publish)),
    };

    let payload_bytes_out = msg.encoded_len() as u64;
    let send_begin = Instant::now();
    {
        let mut inflight = run_state.inflight.lock().await;
        inflight.insert(seq, send_begin);
    }
    {
        let mut stats = run_state.stats.lock().await;
        stats.request_count += 1;
        stats.payload_bytes_out += payload_bytes_out;
    }
    tx.send(msg).await?;
    {
        let mut stats = run_state.stats.lock().await;
        stats.aggregate_send_time_us += send_begin.elapsed().as_micros() as u64;
    }
    *next_seq += 1;

    Ok(())
}

fn spawn_response_drain(
    mut response_stream: tonic::Streaming<session_proto::ServerMessage>,
    run_state: Arc<RunState>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match response_stream.message().await {
                Ok(Some(message)) => {
                    apply_server_message(&message, &run_state).await;
                }
                Ok(None) => {
                    record_stream_terminal(
                        &run_state,
                        "stream closed while waiting for WidgetPublishResult acks".to_string(),
                    )
                    .await;
                    break;
                }
                Err(err) => {
                    record_stream_terminal(
                        &run_state,
                        format!("stream error while waiting for WidgetPublishResult acks: {err}"),
                    )
                    .await;
                    break;
                }
            }
        }
    })
}

async fn wait_for_response_drain_shutdown(handle: &mut JoinHandle<()>) -> Option<String> {
    match timeout(Duration::from_secs(1), &mut *handle).await {
        Ok(Ok(())) => None,
        Ok(Err(err)) => Some(format!("response drain task failed during shutdown: {err}")),
        Err(_) => {
            handle.abort();
            match handle.await {
                Ok(()) => {
                    Some("response drain task completed only after shutdown timeout".to_string())
                }
                Err(err) if err.is_cancelled() => Some(
                    "response drain task did not stop within 1s after SessionClose; aborted"
                        .to_string(),
                ),
                Err(err) => Some(format!(
                    "response drain task failed after shutdown abort: {err}"
                )),
            }
        }
    }
}

async fn record_stream_terminal(run_state: &RunState, warning: String) {
    {
        let mut stream_terminal = run_state.stream_terminal.lock().await;
        *stream_terminal = Some(warning.clone());
    }
    {
        let mut stats = run_state.stats.lock().await;
        stats.warnings.push(warning);
    }
    run_state.ack_notify.notify_waiters();
}

async fn apply_server_message(message: &session_proto::ServerMessage, run_state: &RunState) {
    if let Some(ServerPayload::RequestResult(result)) = &message.payload {
        run_state.ack_count.fetch_add(1, Ordering::Relaxed);
        let rtt_start = {
            let mut inflight = run_state.inflight.lock().await;
            inflight.remove(&result.seq)
        };

        let mut stats = run_state.stats.lock().await;
        stats.payload_bytes_in += message.encoded_len() as u64;

        if result.ok {
            stats.success_count += 1;
        } else {
            stats.error_count += 1;
            let detail = format!("{}: {}", result.code, result.hint);
            stats.warnings.push(format!(
                "publish rejected for request_sequence={}: {}",
                result.seq, detail
            ));
        }

        if let Some(start) = rtt_start {
            stats.rtt_us.push(elapsed_us(start, Instant::now()));
        } else {
            stats.warnings.push(format!(
                "received ack for unknown request_sequence={}",
                result.seq
            ));
        }
        drop(stats);
        run_state.ack_notify.notify_waiters();
    }
}

async fn wait_for_expected_acks(
    run_state: &Arc<RunState>,
    expected_acks: u64,
    timeout_dur: Duration,
) -> Result<AckDrainOutcome, DynError> {
    let deadline = TokioInstant::now() + timeout_dur;

    loop {
        if run_state.ack_count.load(Ordering::Relaxed) >= expected_acks {
            return Ok(AckDrainOutcome::Complete);
        }
        {
            let stream_terminal = run_state.stream_terminal.lock().await;
            if stream_terminal.is_some() {
                return Ok(AckDrainOutcome::StreamEnded);
            }
        }

        let now = TokioInstant::now();
        if now >= deadline {
            return Ok(AckDrainOutcome::DeadlineExceeded);
        }

        let remaining = deadline.saturating_duration_since(now);
        tokio::select! {
            _ = run_state.ack_notify.notified() => {}
            _ = sleep(remaining.min(Duration::from_millis(25))) => {}
        }
    }
}

fn resolve_target(path: &Path, target_id: &str) -> Result<TargetConfig, DynError> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("failed to read targets file '{}': {e}", path.display()))?;
    let registry: TargetRegistry = toml::from_str(&raw)
        .map_err(|e| format!("failed to parse targets file '{}': {e}", path.display()))?;

    registry
        .targets
        .into_iter()
        .find(|target| target.target_id == target_id)
        .ok_or_else(|| {
            format!(
                "target_id '{}' not found in '{}'",
                target_id,
                path.display()
            )
            .into()
        })
}

fn write_artifact(path: &Path, artifact: &PublishLoadArtifact) -> Result<(), DynError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(artifact)?;
    fs::write(path, body)?;
    Ok(())
}

fn percentile_summary(samples: &[u64]) -> (u64, u64, u64, u64) {
    if samples.is_empty() {
        return (0, 0, 0, 0);
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();

    let p50 = percentile(&sorted, 0.50);
    let p95 = percentile(&sorted, 0.95);
    let p99 = percentile(&sorted, 0.99);
    let max = *sorted.last().unwrap_or(&0);

    (p50, p95, p99, max)
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    let n = sorted.len();
    if n == 0 {
        return 0;
    }
    let rank = (q * (n.saturating_sub(1) as f64)).round() as usize;
    sorted[rank.min(n - 1)]
}

fn elapsed_us(start: Instant, end: Instant) -> u64 {
    end.duration_since(start).as_micros() as u64
}

fn now_wall_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn normalize_endpoint(endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    }
}

fn default_output_path() -> String {
    let ts = now_wall_us();
    format!("benchmarks/publish-load/widget_publish_load_{ts}.json")
}

fn required_string(kv: &BTreeMap<String, String>, key: &str) -> Result<String, String> {
    kv.get(key)
        .cloned()
        .ok_or_else(|| format!("missing required argument --{key}"))
}

fn parse_opt_u64(kv: &BTreeMap<String, String>, key: &str) -> Result<Option<u64>, String> {
    kv.get(key).map(|v| parse_u64(v, key)).transpose()
}

fn parse_opt_f64(kv: &BTreeMap<String, String>, key: &str) -> Result<Option<f64>, String> {
    kv.get(key).map(|v| parse_f64(v, key)).transpose()
}

fn parse_u64(raw: &str, key: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .map_err(|e| format!("invalid --{key} '{raw}': {e}"))
}

fn parse_u32(raw: &str, key: &str) -> Result<u32, String> {
    raw.parse::<u32>()
        .map_err(|e| format!("invalid --{key} '{raw}': {e}"))
}

fn parse_f64(raw: &str, key: &str) -> Result<f64, String> {
    raw.parse::<f64>()
        .map_err(|e| format!("invalid --{key} '{raw}': {e}"))
}

fn parse_f32(raw: &str, key: &str) -> Result<f32, String> {
    raw.parse::<f32>()
        .map_err(|e| format!("invalid --{key} '{raw}': {e}"))
}

fn print_usage() {
    println!(
        "widget_publish_load_harness\n\
         \n\
         Required:\n\
           --target-id <id>\n\
         Optional:\n\
           --targets-file <path>                     (default: ./targets/publish_load_targets.toml)\n\
           --mode <burst|paced>                      (default: burst)\n\
           --publish-count <n>                       (burst default: 1000)\n\
           --duration-s <seconds>                    (paced bound)\n\
           --target-rate-rps <rps>                   (required for paced)\n\
           --transport <grpc>                        (default: grpc)\n\
           --publish-intent <widget>                 (default: widget)\n\
           --widget-name <name>                      (default: gauge)\n\
           --instance-id <id>                        (default: publish-load-harness)\n\
           --payload-profile <name>                  (default: gauge_default)\n\
           --param-name <name>                       (default: value)\n\
           --param-start <f32>                       (default: 0)\n\
           --param-step <f32>                        (default: 1)\n\
           --transition-ms <u32>                     (default: 0)\n\
           --timeout-s <seconds>                     (default: 30)\n\
           --target-p99-rtt-us <us>\n\
           --target-throughput-rps <rps>\n\
           --normalization-mapping-approved          (flag)\n\
           --psk <value>                             (defaults to target's psk_env)\n\
           --agent-id <id>                           (default: widget-publish-load-harness)\n\
           --output <path>                           (default: benchmarks/publish-load/widget_publish_load_<ts>.json)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_summary_empty_defaults_to_zeroes() {
        assert_eq!(percentile_summary(&[]), (0, 0, 0, 0));
    }

    #[test]
    fn percentile_summary_monotonic() {
        let (p50, p95, p99, max) = percentile_summary(&[100, 200, 300, 400, 500]);
        assert!(p50 <= p95 && p95 <= p99 && p99 <= max);
    }

    #[tokio::test]
    async fn apply_server_message_counts_widget_ack_and_rtt() {
        let state = RunState::default();
        state
            .inflight
            .lock()
            .await
            .insert(42, Instant::now() - Duration::from_millis(2));

        let message = session_proto::ServerMessage {
            sequence: 9,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::RequestResult(session_proto::RequestResult {
                seq: 42,
                ok: true,
                code: String::new(),
                hint: String::new(),
                ..Default::default()
            })),
        };

        apply_server_message(&message, &state).await;

        assert_eq!(state.ack_count.load(Ordering::Relaxed), 1);
        let stats = state.stats.lock().await;
        assert_eq!(stats.success_count, 1);
        assert_eq!(stats.error_count, 0);
        assert!(state.inflight.lock().await.is_empty());
        assert_eq!(stats.rtt_us.len(), 1);
        assert!(stats.payload_bytes_in > 0);
    }

    #[tokio::test]
    async fn apply_server_message_records_rejected_widget_ack() {
        let state = RunState::default();
        state.inflight.lock().await.insert(7, Instant::now());

        let message = session_proto::ServerMessage {
            sequence: 10,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::RequestResult(session_proto::RequestResult {
                seq: 7,
                ok: false,
                code: "WIDGET_PARAMETER_INVALID".to_string(),
                hint: "bad progress".to_string(),
                ..Default::default()
            })),
        };

        apply_server_message(&message, &state).await;

        assert_eq!(state.ack_count.load(Ordering::Relaxed), 1);
        let stats = state.stats.lock().await;
        assert_eq!(stats.success_count, 0);
        assert_eq!(stats.error_count, 1);
        assert!(stats.warnings.iter().any(|warning| {
            warning.contains("publish rejected for request_sequence=7")
                && warning.contains("WIDGET_PARAMETER_INVALID")
        }));
    }

    #[tokio::test]
    async fn wait_for_expected_acks_completes_after_concurrent_update() {
        let run_state = Arc::new(RunState::default());
        let updater_state = Arc::clone(&run_state);
        tokio::spawn(async move {
            sleep(Duration::from_millis(5)).await;
            updater_state.ack_count.store(3, Ordering::Relaxed);
            updater_state.ack_notify.notify_waiters();
        });

        let outcome = wait_for_expected_acks(&run_state, 3, Duration::from_millis(200))
            .await
            .expect("wait should not fail");

        assert_eq!(outcome, AckDrainOutcome::Complete);
    }

    #[tokio::test]
    async fn wait_for_expected_acks_uses_bounded_deadline() {
        let run_state = Arc::new(RunState::default());
        {
            let mut stats = run_state.stats.lock().await;
            stats.request_count = 3;
            run_state.ack_count.store(1, Ordering::Relaxed);
        }

        let started = Instant::now();
        let outcome = wait_for_expected_acks(&run_state, 3, Duration::from_millis(20))
            .await
            .expect("wait should not fail");

        assert_eq!(outcome, AckDrainOutcome::DeadlineExceeded);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "ack wait must be bounded, elapsed={:?}",
            started.elapsed()
        );
    }

    #[test]
    fn normalize_endpoint_adds_http_prefix() {
        assert_eq!(
            normalize_endpoint("127.0.0.1:50051"),
            "http://127.0.0.1:50051"
        );
        assert_eq!(
            normalize_endpoint("http://example:50051"),
            "http://example:50051"
        );
    }

    #[test]
    fn target_registry_parses_aliases() {
        let raw = r#"
            [[targets]]
            id = "local"
            endpoint = "127.0.0.1:50051"
            host = "localhost"
        "#;

        let parsed: TargetRegistry = toml::from_str(raw).expect("registry parses");
        assert_eq!(parsed.targets.len(), 1);
        let t = &parsed.targets[0];
        assert_eq!(t.target_id, "local");
        assert_eq!(t.grpc_endpoint, "127.0.0.1:50051");
        assert_eq!(t.target_host, "localhost");
        assert_eq!(t.network_scope, "tailnet");
        assert_eq!(t.psk_env, "MCP_TEST_PSK");
    }

    #[test]
    fn cli_parse_rejects_unsupported_transport_intent() {
        let args = vec![
            "--target-id".to_string(),
            "local".to_string(),
            "--transport".to_string(),
            "mcp".to_string(),
        ];

        let err = Cli::parse_from(args).expect_err("mcp transport must fail fast");
        assert!(err.contains("unsupported transport intent 'mcp'"));
    }

    #[test]
    fn cli_parse_rejects_unsupported_zone_intent() {
        let args = vec![
            "--target-id".to_string(),
            "local".to_string(),
            "--publish-intent".to_string(),
            "zone".to_string(),
        ];

        let err = Cli::parse_from(args).expect_err("zone intent must fail fast");
        assert!(err.contains("unsupported publish intent 'zone'"));
    }

    #[test]
    fn cli_parse_rejects_unsupported_tile_intent() {
        let args = vec![
            "--target-id".to_string(),
            "local".to_string(),
            "--publish-intent".to_string(),
            "tile".to_string(),
        ];

        let err = Cli::parse_from(args).expect_err("tile intent must fail fast");
        assert!(err.contains("unsupported publish intent 'tile'"));
    }

    #[test]
    fn cli_parse_rejects_unknown_argument() {
        let args = vec![
            "--target-id".to_string(),
            "local".to_string(),
            "--zone-name".to_string(),
            "status-bar".to_string(),
        ];

        let err = Cli::parse_from(args).expect_err("unknown args must be rejected");
        assert!(err.contains("unsupported argument --zone-name"));
    }

    #[test]
    fn cli_parse_accepts_explicit_supported_transport_and_intent() {
        let args = vec![
            "--target-id".to_string(),
            "local".to_string(),
            "--transport".to_string(),
            "grpc".to_string(),
            "--publish-intent".to_string(),
            "widget".to_string(),
        ];

        let cli = Cli::parse_from(args).expect("supported intent should parse");
        assert_eq!(cli.target_id, "local");
        assert_eq!(cli.mode, WorkloadMode::Burst);
        assert_eq!(cli.publish_count, Some(1000));
    }

    #[test]
    fn default_registry_includes_user_test_windows_tailnet_target() {
        let raw = include_str!("../../../targets/publish_load_targets.toml");
        let registry: TargetRegistry = toml::from_str(raw).expect("default registry parses");

        assert!(
            registry
                .targets
                .iter()
                .any(|target| target.target_id == "local-dev"),
            "default publish-load targets registry should retain local-dev",
        );
        assert!(
            registry
                .targets
                .iter()
                .any(|target| target.target_id == "user-test-windows-tailnet"),
            "default publish-load targets registry should include user-test-windows-tailnet",
        );
    }
}
