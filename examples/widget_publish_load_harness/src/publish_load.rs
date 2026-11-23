//! Canonical artifact contract for resident publish-load benchmark runs.
//!
//! This module defines identity fields, raw metrics, byte-accounting labels,
//! traceability fields, calibration status, and verdict semantics with explicit
//! `uncalibrated` handling for remote runs lacking approved normalization.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublishLoadTransport {
    Grpc,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublishLoadMode {
    Burst,
    Paced,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ByteAccountingMode {
    PayloadOnly,
    PayloadPlusWire,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublishLoadCalibrationStatus {
    Calibrated,
    Uncalibrated,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublishLoadVerdict {
    Pass,
    Fail,
    Uncalibrated,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct PublishLoadIdentity {
    pub target_id: String,
    pub target_host: String,
    pub network_scope: String,
    pub transport: PublishLoadTransport,
    pub mode: PublishLoadMode,
    pub widget_name: String,
    pub payload_profile: String,
    pub publish_count: Option<u64>,
    pub duration_s: Option<f64>,
    pub target_rate_rps: Option<f64>,
}

impl PublishLoadIdentity {
    pub(crate) fn stable_comparison_key(&self) -> String {
        let transport = match self.transport {
            PublishLoadTransport::Grpc => "grpc",
        };
        let mode = match self.mode {
            PublishLoadMode::Burst => "burst",
            PublishLoadMode::Paced => "paced",
        };
        let shape = match self.mode {
            PublishLoadMode::Burst => match self.publish_count {
                Some(count) => format!("count:{count}"),
                None => "count:none".to_string(),
            },
            PublishLoadMode::Paced => {
                let duration = match self.duration_s {
                    Some(duration_s) => format!("duration:{duration_s:.6}"),
                    None => "duration:none".to_string(),
                };
                let count = match self.publish_count {
                    Some(count) => format!("count:{count}"),
                    None => "count:none".to_string(),
                };
                let target_rate = match self.target_rate_rps {
                    Some(target_rate_rps) => format!("target_rate:{target_rate_rps:.6}"),
                    None => "target_rate:none".to_string(),
                };

                format!("{duration}|{count}|{target_rate}")
            }
        };

        format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.target_id,
            transport,
            mode,
            self.widget_name,
            self.payload_profile,
            self.network_scope,
            shape
        )
    }

    fn is_remote(&self) -> bool {
        !self.network_scope.eq_ignore_ascii_case("local")
            && !self.network_scope.eq_ignore_ascii_case("localhost")
            && !self.network_scope.eq_ignore_ascii_case("loopback")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct PublishLoadMetrics {
    pub request_count: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub wall_duration_us: u64,
    pub throughput_rps: f64,
    pub rtt_p50_us: u64,
    pub rtt_p95_us: u64,
    pub rtt_p99_us: u64,
    pub rtt_max_us: u64,
    pub aggregate_send_time_us: u64,
    pub aggregate_ack_drain_time_us: u64,
    pub payload_bytes_out: u64,
    pub payload_bytes_in: u64,
    pub wire_bytes_out: Option<u64>,
    pub wire_bytes_in: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct PublishLoadThresholds {
    pub target_p99_rtt_us: Option<u64>,
    pub target_throughput_rps: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct PublishLoadTraceability {
    pub spec_id: Option<String>,
    pub budget_id: Option<String>,
    pub threshold_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct PublishLoadArtifact {
    pub benchmark_key: String,
    pub identity: PublishLoadIdentity,
    pub metrics: PublishLoadMetrics,
    pub byte_accounting_mode: ByteAccountingMode,
    pub thresholds: PublishLoadThresholds,
    pub traceability: PublishLoadTraceability,
    pub calibration_status: PublishLoadCalibrationStatus,
    pub normalization_mapping_approved: bool,
    pub threshold_comparisons_informational: bool,
    pub verdict: PublishLoadVerdict,
    pub warnings: Vec<String>,
    pub histogram_path: Option<String>,
    pub calibration_path: Option<String>,
}

impl PublishLoadArtifact {
    pub(crate) fn validate(&self) -> Result<(), String> {
        self.validate_benchmark_key()?;
        self.validate_counts()?;
        self.validate_mode_shape()?;
        self.validate_byte_accounting()?;
        self.validate_uncalibrated_semantics()?;
        Ok(())
    }

    fn validate_benchmark_key(&self) -> Result<(), String> {
        let expected = self.identity.stable_comparison_key();
        if self.benchmark_key == expected {
            Ok(())
        } else {
            Err(format!(
                "benchmark_key mismatch: expected '{expected}', got '{}'",
                self.benchmark_key
            ))
        }
    }

    fn validate_counts(&self) -> Result<(), String> {
        let observed_outcomes = self
            .metrics
            .success_count
            .checked_add(self.metrics.error_count)
            .ok_or_else(|| {
                "invalid metrics counts: success_count + error_count overflowed u64".to_string()
            })?;

        if observed_outcomes > self.metrics.request_count {
            return Err(
                "invalid metrics counts: success_count + error_count exceeds request_count"
                    .to_string(),
            );
        }

        if self.metrics.rtt_p50_us > self.metrics.rtt_p95_us
            || self.metrics.rtt_p95_us > self.metrics.rtt_p99_us
            || self.metrics.rtt_p99_us > self.metrics.rtt_max_us
        {
            return Err("invalid RTT percentiles: require p50 <= p95 <= p99 <= max".to_string());
        }

        Ok(())
    }

    fn validate_mode_shape(&self) -> Result<(), String> {
        match self.identity.mode {
            PublishLoadMode::Burst => {
                if self.identity.publish_count.is_none() {
                    return Err("burst mode requires publish_count".to_string());
                }
            }
            PublishLoadMode::Paced => {
                if self.identity.duration_s.is_none() && self.identity.publish_count.is_none() {
                    return Err(
                        "paced mode requires at least one bound: duration_s or publish_count"
                            .to_string(),
                    );
                }
                if self.identity.target_rate_rps.is_none() {
                    return Err("paced mode requires target_rate_rps".to_string());
                }
            }
        }

        Ok(())
    }

    fn validate_byte_accounting(&self) -> Result<(), String> {
        let has_wire =
            self.metrics.wire_bytes_out.is_some() || self.metrics.wire_bytes_in.is_some();

        match self.byte_accounting_mode {
            ByteAccountingMode::PayloadOnly => {
                if has_wire {
                    return Err(
                        "byte_accounting_mode=payload_only forbids wire_bytes_* totals".to_string(),
                    );
                }
            }
            ByteAccountingMode::PayloadPlusWire => {
                if self.metrics.wire_bytes_out.is_none() || self.metrics.wire_bytes_in.is_none() {
                    return Err(
                        "byte_accounting_mode=payload_plus_wire requires both wire_bytes_out and wire_bytes_in"
                            .to_string(),
                    );
                }
            }
        }

        Ok(())
    }

    fn validate_uncalibrated_semantics(&self) -> Result<(), String> {
        let needs_uncalibrated = self.identity.is_remote() && !self.normalization_mapping_approved;

        if needs_uncalibrated {
            if self.calibration_status != PublishLoadCalibrationStatus::Uncalibrated {
                return Err(
                    "remote runs without approved normalization mapping must be marked uncalibrated"
                        .to_string(),
                );
            }
            if !self.threshold_comparisons_informational {
                return Err(
                    "remote uncalibrated runs must label threshold comparisons as informational"
                        .to_string(),
                );
            }
            if self.verdict != PublishLoadVerdict::Uncalibrated {
                return Err(
                    "remote runs without approved normalization mapping must use verdict=uncalibrated"
                        .to_string(),
                );
            }
        }

        if self.calibration_status == PublishLoadCalibrationStatus::Uncalibrated
            && self.verdict != PublishLoadVerdict::Uncalibrated
        {
            return Err(
                "calibration_status=uncalibrated requires verdict=uncalibrated".to_string(),
            );
        }
        if self.verdict == PublishLoadVerdict::Uncalibrated
            && self.calibration_status != PublishLoadCalibrationStatus::Uncalibrated
        {
            return Err(
                "verdict=uncalibrated requires calibration_status=uncalibrated".to_string(),
            );
        }
        if self.verdict == PublishLoadVerdict::Uncalibrated
            && !self.threshold_comparisons_informational
        {
            return Err(
                "verdict=uncalibrated requires threshold comparisons to be informational"
                    .to_string(),
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_comparison_key_changes_when_workload_shape_changes() {
        let mut a = PublishLoadIdentity {
            target_id: "t1".to_string(),
            target_host: "host".to_string(),
            network_scope: "tailnet".to_string(),
            transport: PublishLoadTransport::Grpc,
            mode: PublishLoadMode::Burst,
            widget_name: "w".to_string(),
            payload_profile: "p".to_string(),
            publish_count: Some(100),
            duration_s: None,
            target_rate_rps: None,
        };

        let key_a = a.stable_comparison_key();
        a.publish_count = Some(200);
        let key_b = a.stable_comparison_key();

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn paced_stable_key_changes_when_count_bound_changes() {
        let mut a = PublishLoadIdentity {
            target_id: "t1".to_string(),
            target_host: "host".to_string(),
            network_scope: "tailnet".to_string(),
            transport: PublishLoadTransport::Grpc,
            mode: PublishLoadMode::Paced,
            widget_name: "w".to_string(),
            payload_profile: "p".to_string(),
            publish_count: Some(100),
            duration_s: None,
            target_rate_rps: Some(60.0),
        };

        let key_a = a.stable_comparison_key();
        a.publish_count = Some(200);
        let key_b = a.stable_comparison_key();

        assert_ne!(key_a, key_b);
    }
}
