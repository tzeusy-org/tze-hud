#!/usr/bin/env python3
"""Run a three-agent live WidgetPublish soak and emit a summary artifact."""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import json
import os
import shutil
import subprocess
import time
from pathlib import Path
from typing import Any


DEFAULT_AGENTS = "agent-alpha,agent-beta,agent-gamma"
DEFAULT_TARGETS_FILE = "targets/publish_load_targets.toml"
DEFAULT_WIDGET = "main-progress"
DEFAULT_INSTANCE = "main-progress"
DEFAULT_DURATION_S = 3600.0
DEFAULT_RATE_RPS = 1.0
MAX_ARTIFACT_BYTES = 16 * 1024 * 1024
LIVE_METRICS_COPY_NAME = "live_metrics_source.json"
LIVE_METRICS_SUMMARY_NAME = "live_metrics_summary.json"
REQUIRED_INPUT_BUCKETS = (
    "input_to_local_ack",
    "input_to_scene_commit",
    "input_to_next_present",
)
DEFAULT_WINDOWS_PROCESS_NAME = "tze_hud*"


def utc_now_iso() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def parse_agents(raw: str) -> list[str]:
    agents = [part.strip() for part in raw.split(",") if part.strip()]
    if not agents:
        raise SystemExit("--agent-ids must contain at least one agent id")
    return agents


def binary_path(root: Path) -> Path:
    suffix = ".exe" if os.name == "nt" else ""
    return root / "target" / "release" / f"widget_publish_load_harness{suffix}"


def powershell_single_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def encode_powershell_command(script: str) -> str:
    return base64.b64encode(script.encode("utf-16le")).decode("ascii")


def windows_resource_sample_script(
    *,
    label: str,
    process_name: str,
    command_match: str,
) -> str:
    label_literal = powershell_single_quote(label)
    process_name_literal = powershell_single_quote(process_name)
    command_match_literal = powershell_single_quote(command_match)
    return f"""
$processName = {process_name_literal}
$commandMatch = {command_match_literal}
$candidates = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {{
    $_.Name -like $processName -and (
        [string]::IsNullOrWhiteSpace($commandMatch) -or
        ([string]$_.CommandLine).IndexOf($commandMatch, [System.StringComparison]::OrdinalIgnoreCase) -ge 0
    )
}})
$ids = @($candidates | ForEach-Object {{ [int]$_.ProcessId }})
$p = @($ids | ForEach-Object {{ Get-Process -Id $_ -ErrorAction SilentlyContinue }})
$gpu = $null
try {{
    $gpu = (nvidia-smi --query-gpu=utilization.gpu,memory.used --format=csv,noheader,nounits 2>$null | Select-Object -First 1)
}} catch {{}}
[pscustomobject]@{{
    label = {label_literal}
    timestamp_utc = (Get-Date).ToUniversalTime().ToString('o')
    process_name_filter = $processName
    command_match_applied = -not [string]::IsNullOrWhiteSpace($commandMatch)
    process_count = ($p | Measure-Object).Count
    process_ids = @($p | ForEach-Object {{ $_.Id }})
    process_names = @($p | ForEach-Object {{ $_.ProcessName }})
    cpu_seconds_total = ($p | Measure-Object CPU -Sum).Sum
    working_set_bytes_total = ($p | Measure-Object WorkingSet64 -Sum).Sum
    private_memory_bytes_total = ($p | Measure-Object PrivateMemorySize64 -Sum).Sum
    gpu_csv = $gpu
}} | ConvertTo-Json -Compress
""".strip()


def sample_windows_resources(args: argparse.Namespace, label: str) -> dict[str, Any]:
    if not args.sample_windows_resources:
        return {"enabled": False, "label": label}

    ssh_target = f"{args.win_user}@{args.win_host}"
    script = windows_resource_sample_script(
        label=label,
        process_name=getattr(args, "windows_process_name", DEFAULT_WINDOWS_PROCESS_NAME),
        command_match=getattr(args, "windows_process_command_match", ""),
    )
    cmd = [
        "ssh",
        "-o",
        "BatchMode=yes",
    ]
    if args.ssh_identity:
        cmd.extend(["-i", args.ssh_identity])
    cmd.extend(
        [
            ssh_target,
            "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass "
            f"-EncodedCommand {encode_powershell_command(script)}",
        ]
    )
    try:
        result = subprocess.run(cmd, check=False, capture_output=True, text=True, timeout=30)
        if result.returncode != 0:
            return {
                "enabled": True,
                "label": label,
                "ok": False,
                "returncode": result.returncode,
                "stderr": result.stderr.strip(),
            }
        parsed = json.loads(result.stdout)
        parsed["enabled"] = True
        parsed["ok"] = True
        return parsed
    except Exception as exc:  # noqa: BLE001
        return {"enabled": True, "label": label, "ok": False, "error": str(exc)}


def build_harness(root: Path, skip_build: bool) -> Path:
    bin_path = binary_path(root)
    if skip_build:
        if not bin_path.exists():
            raise SystemExit(f"--skip-build requested but binary is missing: {bin_path}")
        return bin_path

    subprocess.run(
        ["cargo", "build", "--release", "-p", "widget_publish_load_harness"],
        cwd=root,
        check=True,
    )
    if not bin_path.exists():
        raise SystemExit(f"expected harness binary not found after build: {bin_path}")
    return bin_path


def harness_command(
    *,
    bin_path: Path,
    args: argparse.Namespace,
    agent_id: str,
    output: Path,
    publish_count: int,
    param_step: float,
) -> list[str]:
    cmd = [
        str(bin_path),
        "--targets-file",
        args.targets_file,
        "--target-id",
        args.target_id,
        "--mode",
        "paced",
        "--duration-s",
        f"{args.duration_s:.3f}",
        "--publish-count",
        str(publish_count),
        "--target-rate-rps",
        f"{args.rate_rps:.6f}",
        "--widget-name",
        args.widget_name,
        "--instance-id",
        args.instance_id,
        "--payload-profile",
        "progress_soak",
        "--param-name",
        args.param_name,
        "--param-start",
        "0.0",
        "--param-step",
        f"{param_step:.9f}",
        "--transition-ms",
        str(args.transition_ms),
        "--ttl-us",
        str(args.ttl_us),
        "--timeout-s",
        str(args.timeout_s),
        "--agent-id",
        agent_id,
        "--output",
        str(output),
    ]
    if args.target_p99_rtt_us is not None:
        cmd.extend(["--target-p99-rtt-us", str(args.target_p99_rtt_us)])
    if args.target_throughput_rps is not None:
        cmd.extend(["--target-throughput-rps", str(args.target_throughput_rps)])
    if args.normalization_mapping_approved:
        cmd.append("--normalization-mapping-approved")
    return cmd


def load_agent_artifact(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {"artifact_path": str(path), "artifact_missing": True}
    try:
        artifact_size = path.stat().st_size
        if artifact_size > MAX_ARTIFACT_BYTES:
            return {
                "artifact_path": str(path),
                "artifact_error": f"artifact exceeds {MAX_ARTIFACT_BYTES} byte limit",
                "artifact_size_bytes": artifact_size,
            }
        with path.open("r", encoding="utf-8") as f:
            data = json.load(f)
    except (json.JSONDecodeError, OSError) as exc:
        return {"artifact_path": str(path), "artifact_error": str(exc)}
    data["artifact_path"] = str(path)
    return data


def repo_relative_path(path: str | Path, root: Path) -> str:
    candidate = Path(path)
    if not candidate.is_absolute():
        return str(path)
    try:
        return candidate.relative_to(root).as_posix()
    except ValueError:
        return str(path)


def percentile(samples: list[int], pct: float) -> int | None:
    if not samples:
        return None
    ordered = sorted(samples)
    rank = int((pct / 100.0) * len(ordered) + 0.999999999)
    idx = max(0, min(rank - 1, len(ordered) - 1))
    return ordered[idx]


def summarize_sample_bucket(bucket: Any, *, include_p95: bool = False) -> dict[str, Any]:
    if not isinstance(bucket, dict):
        return {"sample_count": 0}

    samples = bucket.get("samples")
    if isinstance(samples, list):
        numeric_samples = [int(sample) for sample in samples if isinstance(sample, (int, float))]
        result: dict[str, Any] = {
            "sample_count": len(numeric_samples),
            "p50_us": percentile(numeric_samples, 50.0),
            "p99_us": percentile(numeric_samples, 99.0),
        }
        if include_p95:
            result["p95_us"] = percentile(numeric_samples, 95.0)
        else:
            result["p99_9_us"] = percentile(numeric_samples, 99.9)
        return result

    result = {
        "sample_count": bucket.get("sample_count") or bucket.get("count"),
        "p50_us": bucket.get("p50_us") or bucket.get("p50"),
        "p99_us": bucket.get("p99_us") or bucket.get("p99"),
    }
    if include_p95:
        result["p95_us"] = bucket.get("p95_us") or bucket.get("p95")
    else:
        result["p99_9_us"] = (
            bucket.get("p99_9_us") or bucket.get("p99.9_us") or bucket.get("p999_us")
        )
    return result


def extract_summary_live_metrics(summary: dict[str, Any], *, source_schema: str) -> dict[str, Any]:
    frame_time = summarize_sample_bucket(summary.get("frame_time"))
    input_latency = {
        bucket_name: summarize_sample_bucket(summary.get(bucket_name), include_p95=True)
        for bucket_name in REQUIRED_INPUT_BUCKETS
    }
    return {
        "source_schema": source_schema,
        "frame_time": frame_time,
        "input_latency": input_latency,
    }


def extract_live_metrics_payload(data: dict[str, Any]) -> dict[str, Any]:
    schema = data.get("schema") or data.get("schema_version") or data.get("kind") or "unknown"
    if isinstance(data.get("summary"), dict):
        metrics = extract_summary_live_metrics(data["summary"], source_schema=str(schema))
        if isinstance(data.get("frame_time"), dict):
            frame_time = dict(metrics["frame_time"])
            explicit = data["frame_time"]
            frame_time.update(
                {
                    "p50_us": explicit.get("p50_us", frame_time.get("p50_us")),
                    "p99_us": explicit.get("p99_us", frame_time.get("p99_us")),
                    "p99_9_us": explicit.get("p99_9_us", frame_time.get("p99_9_us")),
                    "peak_us": explicit.get("peak_us"),
                    "sample_count": frame_time.get("sample_count")
                    or data.get("benchmark", {}).get("recorded_frames"),
                }
            )
            metrics["frame_time"] = frame_time
        return metrics

    if isinstance(data.get("sessions"), list):
        frame_samples: list[int] = []
        input_samples = {name: [] for name in REQUIRED_INPUT_BUCKETS}
        session_names: list[str] = []
        for session in data["sessions"]:
            if not isinstance(session, dict):
                continue
            if isinstance(session.get("name"), str):
                session_names.append(session["name"])
            summary = session.get("summary")
            if not isinstance(summary, dict):
                continue
            frame_bucket = summary.get("frame_time", {})
            if isinstance(frame_bucket, dict) and isinstance(frame_bucket.get("samples"), list):
                frame_samples.extend(
                    int(v) for v in frame_bucket["samples"] if isinstance(v, (int, float))
                )
            for name in REQUIRED_INPUT_BUCKETS:
                bucket = summary.get(name, {})
                if isinstance(bucket, dict) and isinstance(bucket.get("samples"), list):
                    input_samples[name].extend(
                        int(v) for v in bucket["samples"] if isinstance(v, (int, float))
                    )

        return {
            "source_schema": str(schema),
            "sessions": session_names,
            "frame_time": summarize_sample_bucket({"samples": frame_samples}),
            "input_latency": {
                name: summarize_sample_bucket({"samples": samples}, include_p95=True)
                for name, samples in input_samples.items()
            },
        }

    return {
        "source_schema": str(schema),
        "frame_time": summarize_sample_bucket(data.get("frame_time")),
        "input_latency": {
            name: summarize_sample_bucket(data.get(name), include_p95=True)
            for name in REQUIRED_INPUT_BUCKETS
        },
    }


def validate_live_metrics(metrics: dict[str, Any]) -> list[str]:
    missing: list[str] = []
    frame_time = metrics.get("frame_time", {})
    for field in ("p50_us", "p99_us", "p99_9_us"):
        if frame_time.get(field) is None:
            missing.append(f"frame_time.{field}")
    if not frame_time.get("sample_count"):
        missing.append("frame_time.sample_count")

    input_latency = metrics.get("input_latency", {})
    for bucket_name in REQUIRED_INPUT_BUCKETS:
        bucket = input_latency.get(bucket_name, {})
        for field in ("p50_us", "p95_us", "p99_us"):
            if bucket.get(field) is None:
                missing.append(f"{bucket_name}.{field}")
        if not bucket.get("sample_count"):
            missing.append(f"{bucket_name}.sample_count")
    return missing


def required_live_metric_fields() -> list[str]:
    fields = [
        "frame_time.p50_us",
        "frame_time.p99_us",
        "frame_time.p99_9_us",
        "frame_time.sample_count",
    ]
    for bucket_name in REQUIRED_INPUT_BUCKETS:
        fields.extend(
            [
                f"{bucket_name}.p50_us",
                f"{bucket_name}.p95_us",
                f"{bucket_name}.p99_us",
                f"{bucket_name}.sample_count",
            ]
        )
    return fields


def load_live_metrics_artifact(path: Path) -> dict[str, Any]:
    result: dict[str, Any] = {"artifact_path": str(path)}
    if not path.exists():
        return {
            **result,
            "ok": False,
            "error": "live metrics artifact missing",
            "missing_metrics": required_live_metric_fields(),
        }
    try:
        artifact_size = path.stat().st_size
        if artifact_size > MAX_ARTIFACT_BYTES:
            return {
                **result,
                "ok": False,
                "error": f"live metrics artifact exceeds {MAX_ARTIFACT_BYTES} byte limit",
                "artifact_size_bytes": artifact_size,
            }
        data = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as exc:
        return {**result, "ok": False, "error": str(exc)}

    if not isinstance(data, dict):
        return {**result, "ok": False, "error": "live metrics artifact must be a JSON object"}
    metrics = extract_live_metrics_payload(data)
    missing = validate_live_metrics(metrics)
    return {
        **result,
        "ok": not missing,
        "missing_metrics": missing,
        **metrics,
    }


def copy_local_live_metrics_artifact(path: Path, output_root: Path) -> Path:
    if not path.exists():
        return path

    dest = output_root / LIVE_METRICS_COPY_NAME
    if path.resolve() != dest.resolve():
        shutil.copyfile(path, dest)
    return dest


def fetch_windows_live_metrics(args: argparse.Namespace, output_root: Path) -> Path | None:
    if not args.windows_live_metrics_path:
        return None

    dest = output_root / LIVE_METRICS_COPY_NAME
    ssh_target = f"{args.win_user}@{args.win_host}"
    remote_path = args.windows_live_metrics_path.replace("\\", "/")
    remote = f"{ssh_target}:{remote_path}"
    cmd = [
        "scp",
        "-o",
        "BatchMode=yes",
    ]
    if args.ssh_identity:
        cmd.extend(["-i", args.ssh_identity])
    cmd.extend([remote, str(dest)])
    subprocess.run(cmd, check=True, capture_output=True, text=True, timeout=60)
    return dest


def resolve_live_metrics(
    *,
    args: argparse.Namespace,
    output_root: Path,
    dry_run: bool,
) -> dict[str, Any]:
    required = not args.allow_missing_live_metrics
    result: dict[str, Any] = {
        "required": required,
        "ok": False,
        "collection": "not_attempted",
    }
    if dry_run:
        result["collection"] = "dry_run"
        result["expected_artifact"] = args.live_metrics_artifact or args.windows_live_metrics_path
        return result

    try:
        source_path = fetch_windows_live_metrics(args, output_root)
        if source_path is None and args.live_metrics_artifact:
            source_path = copy_local_live_metrics_artifact(
                Path(args.live_metrics_artifact), output_root
            )
        if source_path is None:
            result.update(
                {
                    "collection": "missing_source",
                    "error": "no --live-metrics-artifact or --windows-live-metrics-path was provided",
                }
            )
            return result

        loaded = load_live_metrics_artifact(source_path)
        result.update({"collection": "loaded", **loaded})
        summary_path = output_root / LIVE_METRICS_SUMMARY_NAME
        with summary_path.open("w", encoding="utf-8") as f:
            json.dump(result, f, indent=2, sort_keys=True)
            f.write("\n")
        result["summary_path"] = str(summary_path)
        return result
    except Exception as exc:  # noqa: BLE001
        result.update({"collection": "error", "error": str(exc)})
        return result


def summarize(
    *,
    args: argparse.Namespace,
    root: Path,
    started_at: str,
    ended_at: str,
    output_root: Path,
    commands: dict[str, list[str]],
    agent_results: dict[str, dict[str, Any]],
    resource_samples: list[dict[str, Any]],
    live_metrics: dict[str, Any],
    dry_run: bool,
) -> dict[str, Any]:
    metrics_by_agent: dict[str, Any] = {}
    total_requests = 0
    total_success = 0
    total_errors = 0
    max_rtt_jitter_us = 0

    for agent_id, artifact in agent_results.items():
        metrics = artifact.get("metrics", {})
        total_requests += int(metrics.get("request_count", 0) or 0)
        total_success += int(metrics.get("success_count", 0) or 0)
        total_errors += int(metrics.get("error_count", 0) or 0)
        rtt_jitter_us = max(
            0,
            int(metrics.get("rtt_p99_us", 0) or 0) - int(metrics.get("rtt_p50_us", 0) or 0),
        )
        max_rtt_jitter_us = max(max_rtt_jitter_us, rtt_jitter_us)
        metrics_by_agent[agent_id] = {
            "request_count": metrics.get("request_count"),
            "success_count": metrics.get("success_count"),
            "error_count": metrics.get("error_count"),
            "throughput_rps": metrics.get("throughput_rps"),
            "rtt_p50_us": metrics.get("rtt_p50_us"),
            "rtt_p99_us": metrics.get("rtt_p99_us"),
            "rtt_jitter_us": rtt_jitter_us,
            "verdict": artifact.get("verdict"),
            "artifact_path": repo_relative_path(artifact.get("artifact_path"), root)
            if artifact.get("artifact_path")
            else None,
            "artifact_error": artifact.get("artifact_error"),
            "artifact_missing": artifact.get("artifact_missing"),
            "returncode": artifact.get("returncode"),
        }

    drift = None
    ok_samples = [s for s in resource_samples if s.get("ok")]
    if len(ok_samples) >= 2:
        first = ok_samples[0]
        last = ok_samples[-1]
        drift = {
            "working_set_bytes_delta": (last.get("working_set_bytes_total") or 0)
            - (first.get("working_set_bytes_total") or 0),
            "private_memory_bytes_delta": (last.get("private_memory_bytes_total") or 0)
            - (first.get("private_memory_bytes_total") or 0),
            "cpu_seconds_delta": (last.get("cpu_seconds_total") or 0)
            - (first.get("cpu_seconds_total") or 0),
        }

    return {
        "schema_version": 1,
        "kind": "widget_publish_three_agent_soak",
        "dry_run": dry_run,
        "started_at_utc": started_at,
        "ended_at_utc": ended_at,
        "target_id": args.target_id,
        "targets_file": args.targets_file,
        "widget_name": args.widget_name,
        "instance_id": args.instance_id,
        "duration_s": args.duration_s,
        "rate_rps_per_agent": args.rate_rps,
        "agents": list(agent_results.keys()),
        "aggregate": {
            "request_count": total_requests,
            "success_count": total_success,
            "error_count": total_errors,
            "max_rtt_jitter_us": max_rtt_jitter_us,
        },
        "metrics_by_agent": metrics_by_agent,
        "live_metrics": live_metrics,
        "resource_samples": resource_samples,
        "resource_drift": drift,
        "commands": {
            agent_id: [repo_relative_path(part, root) for part in command]
            for agent_id, command in commands.items()
        },
        "output_root": repo_relative_path(output_root, root),
    }


def write_summary(output_root: Path, summary: dict[str, Any]) -> Path:
    path = output_root / "soak_summary.json"
    with path.open("w", encoding="utf-8") as f:
        json.dump(summary, f, indent=2, sort_keys=True)
        f.write("\n")
    return path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-id", default="user-test-windows-tailnet")
    parser.add_argument("--targets-file", default=DEFAULT_TARGETS_FILE)
    parser.add_argument("--agent-ids", default=DEFAULT_AGENTS)
    parser.add_argument("--widget-name", default=DEFAULT_WIDGET)
    parser.add_argument("--instance-id", default=DEFAULT_INSTANCE)
    parser.add_argument("--param-name", default="progress")
    parser.add_argument("--duration-s", type=float, default=DEFAULT_DURATION_S)
    parser.add_argument("--rate-rps", type=float, default=DEFAULT_RATE_RPS)
    parser.add_argument("--transition-ms", type=int, default=0)
    parser.add_argument("--ttl-us", type=int, default=65_000_000)
    parser.add_argument("--timeout-s", type=float, default=30.0)
    parser.add_argument("--target-p99-rtt-us", type=int, default=None)
    parser.add_argument("--target-throughput-rps", type=float, default=None)
    parser.add_argument("--normalization-mapping-approved", action="store_true")
    parser.add_argument("--output-root", default="")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--live-metrics-artifact",
        default="",
        help="Local JSON artifact containing live compositor frame/input metrics to embed in soak_summary.json.",
    )
    parser.add_argument(
        "--windows-live-metrics-path",
        default="",
        help="Remote Windows JSON artifact path to copy into the soak output and parse after the run.",
    )
    parser.add_argument(
        "--allow-missing-live-metrics",
        action="store_true",
        help="Do not fail a real soak when live frame/input metrics cannot be collected.",
    )
    parser.add_argument("--sample-windows-resources", action="store_true")
    parser.add_argument(
        "--windows-process-name",
        default=DEFAULT_WINDOWS_PROCESS_NAME,
        help=(
            "PowerShell wildcard used to select HUD processes for resource sampling. "
            "Defaults to tze_hud* so production and isolated benchmark executables match."
        ),
    )
    parser.add_argument(
        "--windows-process-command-match",
        default="",
        help=(
            "Optional case-insensitive command-line substring required for Windows resource "
            "sampling, for example C:\\tze_hud\\benchmark.toml."
        ),
    )
    parser.add_argument("--resource-sample-interval-s", type=float, default=300.0)
    parser.add_argument("--win-user", default="hud-user")
    parser.add_argument("--win-host", default="windows-host.example")
    parser.add_argument("--ssh-identity", default="")
    args = parser.parse_args()

    if args.duration_s <= 0:
        raise SystemExit("--duration-s must be > 0")
    if args.rate_rps <= 0:
        raise SystemExit("--rate-rps must be > 0")
    if args.resource_sample_interval_s < 0:
        raise SystemExit("--resource-sample-interval-s must be >= 0")

    agents = parse_agents(args.agent_ids)
    root = repo_root()
    timestamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output_root = Path(args.output_root) if args.output_root else root / "benchmarks" / "soak" / timestamp
    if not output_root.is_absolute():
        output_root = root / output_root
    agent_dir = output_root / "agents"
    log_dir = output_root / "logs"
    agent_dir.mkdir(parents=True, exist_ok=True)
    log_dir.mkdir(parents=True, exist_ok=True)

    publish_count = max(1, int(round(args.duration_s * args.rate_rps)))
    param_step = 0.0 if publish_count <= 1 else 1.0 / float(publish_count - 1)
    planned_bin = binary_path(root)

    commands: dict[str, list[str]] = {}
    for agent_id in agents:
        commands[agent_id] = harness_command(
            bin_path=planned_bin,
            args=args,
            agent_id=agent_id,
            output=agent_dir / f"{agent_id}.json",
            publish_count=publish_count,
            param_step=param_step,
        )

    started_at = utc_now_iso()
    if args.dry_run:
        dry_results = {
            agent_id: {
                "artifact_path": str(agent_dir / f"{agent_id}.json"),
                "returncode": None,
            }
            for agent_id in agents
        }
        summary = summarize(
            args=args,
            root=root,
            started_at=started_at,
            ended_at=utc_now_iso(),
            output_root=output_root,
            commands=commands,
            agent_results=dry_results,
            resource_samples=[],
            live_metrics=resolve_live_metrics(args=args, output_root=output_root, dry_run=True),
            dry_run=True,
        )
        path = write_summary(output_root, summary)
        print(f"dry-run summary: {path}")
        return 0

    bin_path = build_harness(root, args.skip_build)
    commands = {
        agent_id: harness_command(
            bin_path=bin_path,
            args=args,
            agent_id=agent_id,
            output=agent_dir / f"{agent_id}.json",
            publish_count=publish_count,
            param_step=param_step,
        )
        for agent_id in agents
    }

    resource_samples = [sample_windows_resources(args, "before")]
    processes: dict[str, subprocess.Popen[Any]] = {}
    log_handles: list[Any] = []
    interrupted = False
    launch_error = None
    try:
        for agent_id, cmd in commands.items():
            stdout = (log_dir / f"{agent_id}.stdout.log").open("w", encoding="utf-8")
            stderr = (log_dir / f"{agent_id}.stderr.log").open("w", encoding="utf-8")
            log_handles.extend([stdout, stderr])
            processes[agent_id] = subprocess.Popen(cmd, cwd=root, stdout=stdout, stderr=stderr)
            stdout.close()
            stderr.close()
            log_handles.remove(stdout)
            log_handles.remove(stderr)

        next_resource_sample = (
            time.monotonic() + args.resource_sample_interval_s
            if args.sample_windows_resources and args.resource_sample_interval_s > 0
            else None
        )
        resource_sample_index = 1
        while any(proc.poll() is None for proc in processes.values()):
            time.sleep(2.0)
            if next_resource_sample is not None and time.monotonic() >= next_resource_sample:
                resource_samples.append(
                    sample_windows_resources(args, f"during-{resource_sample_index}")
                )
                resource_sample_index += 1
                next_resource_sample += args.resource_sample_interval_s
    except KeyboardInterrupt:
        interrupted = True
        for proc in processes.values():
            if proc.poll() is None:
                proc.terminate()
    except Exception as exc:  # noqa: BLE001
        launch_error = str(exc)
        for proc in processes.values():
            if proc.poll() is None:
                proc.terminate()
    finally:
        for handle in log_handles:
            handle.close()
        for proc in processes.values():
            if proc.poll() is None:
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=10)

    resource_samples.append(sample_windows_resources(args, "after"))
    live_metrics = resolve_live_metrics(args=args, output_root=output_root, dry_run=False)
    agent_results: dict[str, dict[str, Any]] = {}
    exit_code = 0
    for agent_id in agents:
        proc = processes.get(agent_id)
        artifact = load_agent_artifact(agent_dir / f"{agent_id}.json")
        artifact["returncode"] = proc.returncode if proc is not None else None
        agent_results[agent_id] = artifact
        if proc is None:
            exit_code = exit_code or 1
        elif proc.returncode != 0:
            exit_code = proc.returncode or 1
        if artifact.get("artifact_error") or artifact.get("artifact_missing"):
            exit_code = exit_code or 1

    if launch_error is not None:
        exit_code = exit_code or 1
    if interrupted:
        exit_code = exit_code or 130
    if live_metrics.get("required") and not live_metrics.get("ok"):
        exit_code = exit_code or 1

    summary = summarize(
        args=args,
        root=root,
        started_at=started_at,
        ended_at=utc_now_iso(),
        output_root=output_root,
        commands=commands,
        agent_results=agent_results,
        resource_samples=resource_samples,
        live_metrics=live_metrics,
        dry_run=False,
    )
    if launch_error is not None:
        summary["launch_error"] = launch_error
    if interrupted:
        summary["interrupted"] = True
    path = write_summary(output_root, summary)
    print(f"soak summary: {path}")
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
