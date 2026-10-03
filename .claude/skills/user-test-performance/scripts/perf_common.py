#!/usr/bin/env python3
"""Shared utilities for user-test-performance benchmarks."""

from __future__ import annotations

import csv
import datetime as dt
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "user-test" / "scripts"))
import hud_env  # noqa: E402


SCHEMA_VERSION = 1

DEFAULT_TARGETS = {
    "schema_version": 1,
    "default_target_id": "user-test-windows-tailnet",
    "targets": {
        "user-test-windows-tailnet": {
            "description": "Windows HUD host used by /user-test",
            "mcp_url": "http://windows-host.example:9090/mcp",
            "grpc_target": "windows-host.example:50051",
            "network_scope": "tailnet",
        }
    },
}

RESULTS_CSV_HEADER = [
    "recorded_at_utc",
    "schema_version",
    "script",
    "script_version",
    "git_commit",
    "benchmark_name",
    "primary_key",
    "transport",
    "mode",
    "target_id",
    "endpoint",
    "target_description",
    "target_network_scope",
    "namespace",
    "widget_name",
    "zone_name",
    "count",
    "success_count",
    "error_count",
    "concurrency",
    "duration_ms_requested",
    "e2e_latency_ms",
    "send_phase_ms",
    "result_drain_ms",
    "throughput_rps",
    "send_rps",
    "end_to_end_rps",
    "min_ms",
    "p50_ms",
    "p95_ms",
    "p99_ms",
    "max_ms",
    "mean_ms",
    "stddev_ms",
    "bytes_out",
    "bytes_in",
    "bytes_out_per_success",
    "bytes_in_per_success",
    "transition_ms",
    "ttl_us",
    "start_value",
    "end_value",
    "merge_key",
    "result_timeout_ms",
    "expect_results",
    "warnings_count",
    "sample_error",
    "label_template_hash",
    "zone_text_template_hash",
    "run_notes",
    "trace_spec_ref",
    "trace_rfc_ref",
    "trace_doctrine_ref",
    "trace_budget_ref",
    "expected_e2e_ms_max",
    "expected_p95_ms_max",
    "expected_p99_ms_max",
    "expected_throughput_rps_min",
    "expected_error_rate_max",
    "primary_key_fields_json",
]


def utc_now_iso() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def stable_primary_key(fields: dict[str, Any]) -> tuple[str, str]:
    payload = json.dumps(fields, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    return sha256_hex(payload), payload


def load_target_registry(path: str) -> dict[str, Any]:
    if not os.path.exists(path):
        return DEFAULT_TARGETS
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    if not isinstance(data, dict):
        raise ValueError(f"invalid targets file at {path}: expected JSON object")
    if "targets" not in data or not isinstance(data["targets"], dict):
        raise ValueError(f"invalid targets file at {path}: missing object 'targets'")
    return data


def resolve_target_endpoint(
    *,
    targets_file: str,
    target_id: str | None,
    direct_endpoint: str | None,
    endpoint_key: str,
) -> tuple[str, str, dict[str, Any]]:
    """Resolve endpoint from direct override, HUD_HOST, or target registry."""
    if direct_endpoint is None and target_id is None and os.environ.get("HUD_HOST"):
        endpoint = hud_env.mcp_url() if endpoint_key == "mcp_url" else hud_env.grpc_target()
        return "hud-host", endpoint, {"description": "HUD_HOST", "network_scope": "tailnet"}
    registry = load_target_registry(targets_file)
    resolved_id = target_id or registry.get("default_target_id")
    if not resolved_id:
        raise ValueError("target_id not provided and targets file has no default_target_id")

    targets = registry.get("targets", {})
    target = targets.get(resolved_id)
    if not isinstance(target, dict):
        raise ValueError(f"unknown target_id '{resolved_id}' in {targets_file}")

    endpoint = direct_endpoint or target.get(endpoint_key)
    if not endpoint:
        raise ValueError(
            f"target '{resolved_id}' has no '{endpoint_key}' and no direct endpoint override was provided"
        )

    return resolved_id, str(endpoint), target


def git_commit_short(cwd: str | None = None) -> str:
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
            cwd=cwd,
        )
        return result.stdout.strip()
    except Exception:
        return "unknown"


def append_results_csv(path: str, record: dict[str, Any]) -> None:
    dirname = os.path.dirname(path)
    if dirname:
        os.makedirs(dirname, exist_ok=True)
    exists = os.path.exists(path)
    if exists:
        with open(path, "r", encoding="utf-8", newline="") as f:
            reader = csv.reader(f)
            existing_header = next(reader, [])
        if existing_header != RESULTS_CSV_HEADER:
            _migrate_results_csv_header(path)

    row = {k: "" for k in RESULTS_CSV_HEADER}
    for key, value in record.items():
        if key in row:
            if isinstance(value, bool):
                row[key] = "true" if value else "false"
            elif value is None:
                row[key] = ""
            elif isinstance(value, (dict, list)):
                row[key] = json.dumps(value, sort_keys=True, ensure_ascii=True)
            else:
                row[key] = value

    with open(path, "a", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=RESULTS_CSV_HEADER)
        if not exists:
            writer.writeheader()
        writer.writerow(row)


def _migrate_results_csv_header(path: str) -> None:
    rows: list[dict[str, Any]] = []
    with open(path, "r", encoding="utf-8", newline="") as src:
        reader = csv.DictReader(src, restkey="__extra__")
        for old_row in reader:
            row = {k: "" for k in RESULTS_CSV_HEADER}
            for key, value in old_row.items():
                if key in row and value is not None:
                    row[key] = value
            rows.append(row)

    with open(path, "w", encoding="utf-8", newline="") as dst:
        writer = csv.DictWriter(dst, fieldnames=RESULTS_CSV_HEADER)
        writer.writeheader()
        for row in rows:
            writer.writerow(row)
