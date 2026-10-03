#!/usr/bin/env python3
"""
Benchmark MCP publish performance for widgets and zones.

Examples:
  # 100 widget publishes as fast as possible
  mcp_publish_perf.py --mode widget --widget-name main-progress --count 100

  # 100 zone publishes paced over 5 seconds
  mcp_publish_perf.py --mode zone --zone-name subtitle --count 100 --duration-ms 5000
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import math
import os
import statistics
import time
import urllib.error
import urllib.request
from typing import Any

from perf_common import (
    SCHEMA_VERSION,
    append_results_csv,
    git_commit_short,
    resolve_target_endpoint,
    sha256_hex,
    stable_primary_key,
    utc_now_iso,
)
import hud_env  # noqa: E402  (perf_common puts user-test/scripts on sys.path)


_SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
_REFERENCE_DIR = os.path.normpath(os.path.join(_SCRIPT_DIR, "..", "reference"))
DEFAULT_TARGETS_FILE = os.path.join(_REFERENCE_DIR, "targets.json")
DEFAULT_RESULTS_CSV = os.path.join(_REFERENCE_DIR, "results.csv")

SCRIPT_VERSION = "1.0"


def percentile(sorted_data: list[float], p: float) -> float:
    if not sorted_data:
        return float("nan")
    if len(sorted_data) == 1:
        return sorted_data[0]
    idx = (p / 100.0) * (len(sorted_data) - 1)
    lo = int(math.floor(idx))
    hi = int(math.ceil(idx))
    if lo == hi:
        return sorted_data[lo]
    frac = idx - lo
    return sorted_data[lo] + frac * (sorted_data[hi] - sorted_data[lo])


def build_widget_params(i: int, count: int, args: argparse.Namespace) -> tuple[str, dict[str, Any]]:
    if count <= 1:
        value = args.end_value
    else:
        t = float(i - 1) / float(count - 1)
        value = args.start_value + (args.end_value - args.start_value) * t
    pct = int(round(value * 100.0))
    label = args.label_template.format(i=i, count=count, value=value, pct=pct)

    params: dict[str, Any] = {
        "surface": f"widget:{args.widget_name}",
        "params": {
            "progress": value,
            "label": label,
        },
        "ttl_ms": int(args.ttl_us) // 1000,
    }
    return "hud_publish", params


def build_zone_params(i: int, count: int, args: argparse.Namespace) -> tuple[str, dict[str, Any]]:
    content = args.zone_text_template.format(i=i, count=count)
    params: dict[str, Any] = {
        "surface": f"zone:{args.zone_name}",
        "content": content,
        "ttl_ms": int(args.ttl_us) // 1000,
    }
    if args.merge_key:
        params["key"] = args.merge_key
    return "hud_publish", params


def invoke_rpc(
    *,
    url: str,
    token: str,
    method: str,
    params: dict[str, Any],
    request_id: int,
) -> dict[str, Any]:
    payload = {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "tools/call",
        "params": {"name": method, "arguments": params},
    }
    body = json.dumps(payload, ensure_ascii=True).encode("utf-8")
    req_bytes = len(body)

    req = urllib.request.Request(
        url=url,
        data=body,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {token}",
        },
        method="POST",
    )

    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            raw = resp.read()
        resp_bytes = len(raw)
        parsed = json.loads(raw.decode("utf-8"))
        result = parsed.get("result") or {}
        if "error" in parsed or result.get("isError"):
            err = parsed.get("error") or json.loads(result["content"][0]["text"])
            message = (err.get("message") or err.get("code")) if isinstance(err, dict) else str(err)
            return {
                "ok": False,
                "req_bytes": req_bytes,
                "resp_bytes": resp_bytes,
                "error": message,
            }
        return {
            "ok": True,
            "req_bytes": req_bytes,
            "resp_bytes": resp_bytes,
            "error": "",
        }
    except urllib.error.HTTPError as exc:
        raw = exc.read()
        resp_bytes = len(raw)
        detail = raw.decode("utf-8", errors="replace")
        return {
            "ok": False,
            "req_bytes": req_bytes,
            "resp_bytes": resp_bytes,
            "error": f"HTTP {exc.code}: {detail[:240]}",
        }
    except urllib.error.URLError as exc:
        return {
            "ok": False,
            "req_bytes": req_bytes,
            "resp_bytes": 0,
            "error": f"URL error: {exc}",
        }
    except Exception as exc:  # noqa: BLE001
        return {
            "ok": False,
            "req_bytes": req_bytes,
            "resp_bytes": 0,
            "error": str(exc),
        }


def run_once(
    idx: int,
    count: int,
    args: argparse.Namespace,
    url: str,
    token: str,
) -> dict[str, Any]:
    if args.mode == "widget":
        method, params = build_widget_params(idx, count, args)
    else:
        method, params = build_zone_params(idx, count, args)

    t0 = time.perf_counter()
    result = invoke_rpc(url=url, token=token, method=method, params=params, request_id=idx)
    latency_ms = (time.perf_counter() - t0) * 1000.0

    return {
        "ok": result["ok"],
        "latency_ms": latency_ms,
        "req_bytes": int(result["req_bytes"]),
        "resp_bytes": int(result["resp_bytes"]),
        "error": result.get("error", ""),
    }


def maybe_preflight(args: argparse.Namespace, url: str, token: str) -> dict[str, Any] | None:
    if not args.preflight:
        return None
    return invoke_rpc(url=url, token=token, method="hud_surfaces", params={}, request_id=1)


def default_benchmark_name(args: argparse.Namespace) -> str:
    subject = args.widget_name if args.mode == "widget" else args.zone_name
    return (
        f"mcp-{args.mode}-{subject}-n{args.count}-c{args.concurrency}"
        f"-d{args.duration_ms}"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description="MCP publish performance benchmark")
    parser.add_argument("--url", default=None, help="Direct MCP HTTP URL override")
    parser.add_argument("--target-id", default=None, help="Target id from targets file")
    parser.add_argument("--targets-file", default=DEFAULT_TARGETS_FILE, help="Target registry JSON")
    parser.add_argument("--psk-env", help="Read the PSK from this environment variable instead of the paired file")

    parser.add_argument("--mode", choices=["widget", "zone"], default="widget")
    parser.add_argument("--count", type=int, default=100, help="Number of publishes")
    parser.add_argument("--concurrency", type=int, default=1, help="Parallel worker count")
    parser.add_argument("--duration-ms", type=int, default=0, help="Target total duration (sequential pacing only)")
    parser.add_argument("--namespace", default="user-test-performance", help="Ignored (publisher is the PSK's agent); kept for old command lines")
    parser.add_argument("--ttl-us", type=int, default=60_000_000, help="TTL in microseconds")
    parser.add_argument("--preflight", action="store_true", help="Call hud_surfaces before benchmark")

    parser.add_argument("--widget-name", default="main-progress", help="Widget instance name")
    parser.add_argument("--instance-id", default="", help="Optional widget instance_id override")
    parser.add_argument("--transition-ms", type=int, default=0, help="Widget transition duration")
    parser.add_argument("--start-value", type=float, default=0.01, help="Start value for progress")
    parser.add_argument("--end-value", type=float, default=1.0, help="End value for progress")
    parser.add_argument("--label-template", default="{pct}%", help="Label template for widget publishes")

    parser.add_argument("--zone-name", default="subtitle", help="Zone name")
    parser.add_argument("--merge-key", default="", help="Optional zone merge key")
    parser.add_argument("--zone-text-template", default="perf publish {i}/{count}", help="Zone text template")

    parser.add_argument("--benchmark-name", default=None, help="Stable benchmark label")
    parser.add_argument("--results-csv", default=DEFAULT_RESULTS_CSV, help="Append-only results CSV")
    parser.add_argument("--run-notes", default="", help="Optional notes for this run")
    parser.add_argument("--trace-spec-ref", default="", help="Spec requirement reference id")
    parser.add_argument("--trace-rfc-ref", default="", help="RFC/design contract reference")
    parser.add_argument("--trace-doctrine-ref", default="", help="Doctrine principle reference")
    parser.add_argument("--trace-budget-ref", default="", help="Performance budget identifier")
    parser.add_argument("--expected-e2e-ms-max", type=float, default=None, help="Expected max e2e latency (ms)")
    parser.add_argument("--expected-p95-ms-max", type=float, default=None, help="Expected max p95 latency (ms)")
    parser.add_argument("--expected-p99-ms-max", type=float, default=None, help="Expected max p99 latency (ms)")
    parser.add_argument(
        "--expected-throughput-rps-min",
        type=float,
        default=None,
        help="Expected minimum throughput (req/s)",
    )
    parser.add_argument(
        "--expected-error-rate-max",
        type=float,
        default=None,
        help="Expected maximum error rate (0.0-1.0)",
    )
    parser.add_argument("--record-results", action="store_true", default=True, help="Append run to results CSV")
    parser.add_argument("--no-record-results", action="store_false", dest="record_results", help="Do not append results CSV")

    args = parser.parse_args()

    if args.count <= 0:
        raise SystemExit("--count must be > 0")
    if args.concurrency <= 0:
        raise SystemExit("--concurrency must be > 0")
    if args.expected_error_rate_max is not None and not (0.0 <= args.expected_error_rate_max <= 1.0):
        raise SystemExit("--expected-error-rate-max must be between 0.0 and 1.0")

    target_id, resolved_url, target_meta = resolve_target_endpoint(
        targets_file=args.targets_file,
        target_id=args.target_id,
        direct_endpoint=args.url,
        endpoint_key="mcp_url",
    )

    try:
        token = hud_env.load_psk(resolved_url, args.psk_env)
    except hud_env.HudEnvError as error:
        raise SystemExit(str(error)) from None

    benchmark_name = args.benchmark_name or default_benchmark_name(args)
    preflight = maybe_preflight(args, resolved_url, token)

    latencies: list[float] = []
    errors: list[str] = []
    request_bytes_total = 0
    response_bytes_total = 0

    started = time.perf_counter()

    if args.concurrency == 1:
        interval_s = 0.0
        if args.duration_ms > 0:
            interval_s = (float(args.duration_ms) / 1000.0) / float(args.count)
        t_anchor = time.perf_counter()

        for i in range(1, args.count + 1):
            if interval_s > 0.0:
                deadline = t_anchor + interval_s * float(i - 1)
                now = time.perf_counter()
                if deadline > now:
                    time.sleep(deadline - now)

            result = run_once(i, args.count, args, resolved_url, token)
            latencies.append(result["latency_ms"])
            request_bytes_total += int(result["req_bytes"])
            response_bytes_total += int(result["resp_bytes"])
            if not result["ok"]:
                errors.append(result.get("error", "unknown error"))
    else:
        if args.duration_ms > 0:
            errors.append("warning: --duration-ms ignored when --concurrency > 1")

        with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
            futures = [
                pool.submit(run_once, i, args.count, args, resolved_url, token)
                for i in range(1, args.count + 1)
            ]
            for fut in concurrent.futures.as_completed(futures):
                result = fut.result()
                latencies.append(result["latency_ms"])
                request_bytes_total += int(result["req_bytes"])
                response_bytes_total += int(result["resp_bytes"])
                if not result["ok"]:
                    errors.append(result.get("error", "unknown error"))

    total_ms = (time.perf_counter() - started) * 1000.0

    warning_count = sum(1 for e in errors if str(e).startswith("warning:"))
    hard_errors = [e for e in errors if not str(e).startswith("warning:")]
    error_count = len(hard_errors)
    success_count = args.count - error_count
    error_rate = (error_count / args.count) if args.count > 0 else 0.0

    sorted_lat = sorted(latencies)
    stddev = statistics.pstdev(sorted_lat) if len(sorted_lat) >= 2 else 0.0

    stats = {
        "min_ms": min(sorted_lat) if sorted_lat else float("nan"),
        "p50_ms": percentile(sorted_lat, 50),
        "p95_ms": percentile(sorted_lat, 95),
        "p99_ms": percentile(sorted_lat, 99),
        "max_ms": max(sorted_lat) if sorted_lat else float("nan"),
        "mean_ms": statistics.mean(sorted_lat) if sorted_lat else float("nan"),
        "stddev_ms": stddev,
    }

    throughput_rps = (success_count / (total_ms / 1000.0)) if total_ms > 0 else 0.0
    bytes_out_per_success = (request_bytes_total / success_count) if success_count > 0 else None
    bytes_in_per_success = (response_bytes_total / success_count) if success_count > 0 else None

    primary_key_fields = {
        "schema_version": SCHEMA_VERSION,
        "transport": "mcp_http",
        "mode": args.mode,
        "target_id": target_id,
        "endpoint": resolved_url,
        "widget_name": args.widget_name if args.mode == "widget" else "",
        "zone_name": args.zone_name if args.mode == "zone" else "",
        "count": args.count,
        "concurrency": args.concurrency,
        "duration_ms_requested": args.duration_ms,
        "namespace": args.namespace,
        "ttl_us": int(args.ttl_us),
        "transition_ms": int(args.transition_ms) if args.mode == "widget" else 0,
        "start_value": args.start_value if args.mode == "widget" else 0.0,
        "end_value": args.end_value if args.mode == "widget" else 0.0,
        "merge_key": args.merge_key if args.mode == "zone" else "",
        "label_template_hash": sha256_hex(args.label_template) if args.mode == "widget" else "",
        "zone_text_template_hash": sha256_hex(args.zone_text_template) if args.mode == "zone" else "",
        "trace_spec_ref": args.trace_spec_ref,
        "trace_rfc_ref": args.trace_rfc_ref,
        "trace_doctrine_ref": args.trace_doctrine_ref,
        "trace_budget_ref": args.trace_budget_ref,
        "expected_e2e_ms_max": args.expected_e2e_ms_max,
        "expected_p95_ms_max": args.expected_p95_ms_max,
        "expected_p99_ms_max": args.expected_p99_ms_max,
        "expected_throughput_rps_min": args.expected_throughput_rps_min,
        "expected_error_rate_max": args.expected_error_rate_max,
        "target_network_scope": target_meta.get("network_scope", ""),
    }
    primary_key, primary_key_json = stable_primary_key(primary_key_fields)

    result = {
        "transport": "mcp_http",
        "mode": args.mode,
        "target_id": target_id,
        "url": resolved_url,
        "target_description": target_meta.get("description", ""),
        "target_network_scope": target_meta.get("network_scope", ""),
        "benchmark_name": benchmark_name,
        "primary_key": primary_key,
        "count": args.count,
        "concurrency": args.concurrency,
        "duration_ms_requested": args.duration_ms,
        "namespace": args.namespace,
        "success_count": success_count,
        "error_count": error_count,
        "warnings_count": warning_count,
        "error_rate": round(error_rate, 4),
        "e2e_latency_ms": round(total_ms, 2),
        "throughput_rps": round(throughput_rps, 2),
        "latency_ms": {k: round(v, 2) if not math.isnan(v) else None for k, v in stats.items()},
        "bytes_out": request_bytes_total,
        "bytes_in": response_bytes_total,
        "bytes_out_per_success": round(bytes_out_per_success, 2) if bytes_out_per_success is not None else None,
        "bytes_in_per_success": round(bytes_in_per_success, 2) if bytes_in_per_success is not None else None,
        "sample_errors": hard_errors[:10],
        "preflight": preflight,
        "traceability": {
            "spec_ref": args.trace_spec_ref,
            "rfc_ref": args.trace_rfc_ref,
            "doctrine_ref": args.trace_doctrine_ref,
            "budget_ref": args.trace_budget_ref,
        },
        "expected_thresholds": {
            "e2e_ms_max": args.expected_e2e_ms_max,
            "p95_ms_max": args.expected_p95_ms_max,
            "p99_ms_max": args.expected_p99_ms_max,
            "throughput_rps_min": args.expected_throughput_rps_min,
            "error_rate_max": args.expected_error_rate_max,
        },
    }

    if args.record_results:
        record = {
            "recorded_at_utc": utc_now_iso(),
            "schema_version": SCHEMA_VERSION,
            "script": "mcp_publish_perf.py",
            "script_version": SCRIPT_VERSION,
            "git_commit": git_commit_short(),
            "benchmark_name": benchmark_name,
            "primary_key": primary_key,
            "transport": "mcp_http",
            "mode": args.mode,
            "target_id": target_id,
            "endpoint": resolved_url,
            "target_description": target_meta.get("description", ""),
            "target_network_scope": target_meta.get("network_scope", ""),
            "namespace": args.namespace,
            "widget_name": args.widget_name if args.mode == "widget" else "",
            "zone_name": args.zone_name if args.mode == "zone" else "",
            "count": args.count,
            "success_count": success_count,
            "error_count": error_count,
            "concurrency": args.concurrency,
            "duration_ms_requested": args.duration_ms,
            "e2e_latency_ms": round(total_ms, 2),
            "throughput_rps": round(throughput_rps, 2),
            "min_ms": round(stats["min_ms"], 2) if not math.isnan(stats["min_ms"]) else "",
            "p50_ms": round(stats["p50_ms"], 2) if not math.isnan(stats["p50_ms"]) else "",
            "p95_ms": round(stats["p95_ms"], 2) if not math.isnan(stats["p95_ms"]) else "",
            "p99_ms": round(stats["p99_ms"], 2) if not math.isnan(stats["p99_ms"]) else "",
            "max_ms": round(stats["max_ms"], 2) if not math.isnan(stats["max_ms"]) else "",
            "mean_ms": round(stats["mean_ms"], 2) if not math.isnan(stats["mean_ms"]) else "",
            "stddev_ms": round(stats["stddev_ms"], 2) if not math.isnan(stats["stddev_ms"]) else "",
            "bytes_out": request_bytes_total,
            "bytes_in": response_bytes_total,
            "bytes_out_per_success": round(bytes_out_per_success, 2) if bytes_out_per_success is not None else "",
            "bytes_in_per_success": round(bytes_in_per_success, 2) if bytes_in_per_success is not None else "",
            "transition_ms": args.transition_ms if args.mode == "widget" else "",
            "ttl_us": args.ttl_us,
            "start_value": args.start_value if args.mode == "widget" else "",
            "end_value": args.end_value if args.mode == "widget" else "",
            "merge_key": args.merge_key if args.mode == "zone" else "",
            "warnings_count": warning_count,
            "sample_error": hard_errors[0] if hard_errors else "",
            "label_template_hash": sha256_hex(args.label_template) if args.mode == "widget" else "",
            "zone_text_template_hash": sha256_hex(args.zone_text_template) if args.mode == "zone" else "",
            "run_notes": args.run_notes,
            "trace_spec_ref": args.trace_spec_ref,
            "trace_rfc_ref": args.trace_rfc_ref,
            "trace_doctrine_ref": args.trace_doctrine_ref,
            "trace_budget_ref": args.trace_budget_ref,
            "expected_e2e_ms_max": args.expected_e2e_ms_max,
            "expected_p95_ms_max": args.expected_p95_ms_max,
            "expected_p99_ms_max": args.expected_p99_ms_max,
            "expected_throughput_rps_min": args.expected_throughput_rps_min,
            "expected_error_rate_max": args.expected_error_rate_max,
            "primary_key_fields_json": primary_key_json,
        }
        append_results_csv(args.results_csv, record)
        result["results_csv"] = args.results_csv

    print(json.dumps(result, ensure_ascii=True, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
