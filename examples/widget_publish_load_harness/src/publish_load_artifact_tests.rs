use crate::publish_load::{
    ByteAccountingMode, PublishLoadArtifact, PublishLoadCalibrationStatus, PublishLoadIdentity,
    PublishLoadMetrics, PublishLoadMode, PublishLoadThresholds, PublishLoadTraceability,
    PublishLoadTransport, PublishLoadVerdict,
};

fn baseline_artifact() -> PublishLoadArtifact {
    let identity = PublishLoadIdentity {
        target_id: "user-test-windows-tailnet".to_string(),
        target_host: "windows-host.example".to_string(),
        network_scope: "tailnet".to_string(),
        transport: PublishLoadTransport::Grpc,
        mode: PublishLoadMode::Burst,
        widget_name: "hud_gauge".to_string(),
        payload_profile: "gauge_default".to_string(),
        publish_count: Some(1000),
        duration_s: None,
        target_rate_rps: None,
    };

    PublishLoadArtifact {
        benchmark_key: identity.stable_comparison_key(),
        identity,
        metrics: PublishLoadMetrics {
            request_count: 1000,
            success_count: 1000,
            error_count: 0,
            wall_duration_us: 2_000_000,
            throughput_rps: 500.0,
            rtt_p50_us: 1200,
            rtt_p95_us: 2000,
            rtt_p99_us: 2800,
            rtt_max_us: 4500,
            aggregate_send_time_us: 900_000,
            aggregate_ack_drain_time_us: 1_100_000,
            payload_bytes_out: 420_000,
            payload_bytes_in: 310_000,
            wire_bytes_out: None,
            wire_bytes_in: None,
        },
        byte_accounting_mode: ByteAccountingMode::PayloadOnly,
        thresholds: PublishLoadThresholds {
            target_p99_rtt_us: Some(5_000),
            target_throughput_rps: Some(450.0),
        },
        traceability: PublishLoadTraceability {
            spec_id: Some("publish-load-harness".to_string()),
            budget_id: Some("publish_load_p99".to_string()),
            threshold_id: Some("publish_load_default".to_string()),
        },
        calibration_status: PublishLoadCalibrationStatus::Uncalibrated,
        normalization_mapping_approved: false,
        threshold_comparisons_informational: true,
        verdict: PublishLoadVerdict::Uncalibrated,
        warnings: vec!["no approved normalization mapping for remote target".to_string()],
        histogram_path: Some("benchmarks/publish-load/rtt_histogram.json".to_string()),
        calibration_path: None,
    }
}

#[test]
fn schema_json_contains_required_fields() {
    let json = serde_json::to_value(baseline_artifact()).expect("serialize artifact");

    for key in [
        "benchmark_key",
        "identity",
        "metrics",
        "byte_accounting_mode",
        "thresholds",
        "traceability",
        "calibration_status",
        "normalization_mapping_approved",
        "threshold_comparisons_informational",
        "verdict",
        "warnings",
        "histogram_path",
        "calibration_path",
    ] {
        assert!(json.get(key).is_some(), "missing required field: {key}");
    }

    assert_eq!(json["byte_accounting_mode"], "payload_only");
    assert_eq!(json["calibration_status"], "uncalibrated");
    assert_eq!(json["verdict"], "uncalibrated");
}

#[test]
fn payload_only_mode_forbids_wire_totals() {
    let mut artifact = baseline_artifact();
    artifact.metrics.wire_bytes_out = Some(1);
    artifact.metrics.wire_bytes_in = Some(1);

    let err = artifact
        .validate()
        .expect_err("expected validation failure");
    assert!(
        err.contains("payload_only") && err.contains("wire_bytes"),
        "unexpected error: {err}"
    );
}

#[test]
fn payload_plus_wire_mode_requires_wire_totals() {
    let mut artifact = baseline_artifact();
    artifact.byte_accounting_mode = ByteAccountingMode::PayloadPlusWire;

    let err = artifact
        .validate()
        .expect_err("expected validation failure");
    assert!(
        err.contains("payload_plus_wire") && err.contains("wire_bytes"),
        "unexpected error: {err}"
    );
}

#[test]
fn remote_without_mapping_must_be_uncalibrated_and_informational() {
    let mut artifact = baseline_artifact();
    artifact.verdict = PublishLoadVerdict::Pass;
    artifact.threshold_comparisons_informational = false;

    let err = artifact
        .validate()
        .expect_err("expected validation failure");
    assert!(
        err.contains("uncalibrated") && err.contains("informational"),
        "unexpected error: {err}"
    );
}

#[test]
fn calibrated_mode_allows_formal_verdict_when_mapping_is_approved() {
    let mut artifact = baseline_artifact();
    artifact.calibration_status = PublishLoadCalibrationStatus::Calibrated;
    artifact.normalization_mapping_approved = true;
    artifact.threshold_comparisons_informational = false;
    artifact.verdict = PublishLoadVerdict::Pass;

    artifact
        .validate()
        .expect("calibrated verdict should validate");
}

#[test]
fn verdict_uncalibrated_requires_uncalibrated_status() {
    let mut artifact = baseline_artifact();
    artifact.identity.network_scope = "local".to_string();
    artifact.normalization_mapping_approved = true;
    artifact.calibration_status = PublishLoadCalibrationStatus::Calibrated;
    artifact.benchmark_key = artifact.identity.stable_comparison_key();

    let err = artifact
        .validate()
        .expect_err("expected validation failure");
    assert!(
        err.contains("verdict=uncalibrated") && err.contains("calibration_status=uncalibrated"),
        "unexpected error: {err}"
    );
}

#[test]
fn overflowed_success_plus_error_is_rejected() {
    let mut artifact = baseline_artifact();
    artifact.metrics.success_count = u64::MAX;
    artifact.metrics.error_count = 1;

    let err = artifact
        .validate()
        .expect_err("expected overflow validation failure");
    assert!(err.contains("overflowed"), "unexpected error: {err}");
}
