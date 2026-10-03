#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""
MCP hud_publish stress test with performance telemetry.

Exercises the MCP hud_publish endpoint at varying load levels while
collecting latency percentiles and host resource telemetry via SSH.

Connection defaults:
  MCP URL : http://$HUD_HOST:9090/mcp
  PSK     : ~/.config/tze-hud/$HUD_HOST.psk (see hud_pair.py)
  SSH host: admin-user@windows-host.example
  SSH key : ~/.ssh/hud-ssh-key

CLI flags (see parse_args() for full reference):
  --url           MCP endpoint (default: from HUD_HOST)
  --psk-env       Env var holding the PSK (default: the paired file)
  --ssh-host      SSH hostname for telemetry (user@host format)
  --ssh-key       SSH private key path
  --output        JSON report output path (default: stress_report_<ISO8601>.json)
  --concurrency   Override per-profile concurrency
  --profiles      Comma-separated subset of profiles to run (default: all)
  --duration      Override per-profile duration in seconds
  --short-ttl     Use 1s TTL instead of default 60s; exercises TTL housekeeping
  --large-payloads  Rotate payload sizes across publishes (100B/1KB/10KB/60KB)
  --verbose       Print each request result
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime
import json
import math
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import asdict, dataclass, field
from typing import Any

import hud_env

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

DEFAULT_SSH_USER = "admin-user"
DEFAULT_SSH_HOST = "windows-host.example"
DEFAULT_SSH_KEY = os.path.expanduser("~/.ssh/hud-ssh-key")
# Spec: --output default is stress_report_{ISO8601_compact}.json (generated at startup)
DEFAULT_NAMESPACE = "stress-test"
DEFAULT_TTL_US = 60_000_000   # 60 seconds (spec default)
SHORT_TTL_US   = 1_000_000    # 1 second  (--short-ttl)

# Payload size targets for --large-payloads (StreamText zones only).
# Sizes rotate across requests: small -> medium -> large -> max -> small -> ...
_LARGE_PAYLOAD_SIZES: list[int] = [100, 1_024, 10_240, 60_000]

# MergeByKey rotation constants for the status-bar zone.
# The first half of each profile fills rotating keys; the second half reuses them.
_STATUS_BAR_MERGE_KEYS: int = 32  # max map depth per spec

# All 6 default zones with per-zone media-type metadata.
# ``media_type`` drives payload generation; ``contention`` is informational.
ZONES: list[dict[str, Any]] = [
    {
        "zone_name": "subtitle",
        "media_type": "StreamText",
        "contention": "LatestWins",
        "merge_key": "stress-subtitle",
    },
    {
        "zone_name": "status-bar",
        "media_type": "KeyValuePairs",
        "contention": "MergeByKey",
        "merge_key": "stress-status",
    },
    {
        "zone_name": "notification-area",
        "media_type": "ShortTextWithIcon",
        "contention": "Stack",
        "merge_key": "stress-notification",
    },
    {
        "zone_name": "alert-banner",
        "media_type": "StreamText",
        "contention": "Replace",
        "merge_key": "stress-alert",
    },
    {
        "zone_name": "pip",
        "media_type": "SolidColor",
        "contention": "Replace",
        "merge_key": "stress-pip",
    },
    {
        "zone_name": "ambient-background",
        "media_type": "SolidColor",
        "contention": "Replace",
        "merge_key": "stress-ambient",
    },
]

# Load profiles: (name, rate_per_sec, concurrency, duration_sec)
# Spec §Load Profiles table.
PROFILES: list[tuple[str, float, int, int]] = [
    ("idle",   1,  1, 30),
    ("low",    5,  1, 30),
    ("medium", 20, 4, 30),
    ("high",   50, 8, 30),
    ("burst", 100, 16, 10),
]

# ---------------------------------------------------------------------------
# Per-zone-type payload generators
# ---------------------------------------------------------------------------


def _stream_text_payload(req_id: int, zone_name: str, large_payload_index: int | None) -> str:
    """
    Generate a StreamText content string.

    When ``large_payload_index`` is not None, the payload is padded to one of
    the four escalating target sizes defined in ``_LARGE_PAYLOAD_SIZES``,
    rotating by index. This exercises how content size affects latency and
    whether the server handles near-limit payloads.
    """
    base = f"Stress test message {req_id}"
    if large_payload_index is None:
        return base
    target_size = _LARGE_PAYLOAD_SIZES[large_payload_index % len(_LARGE_PAYLOAD_SIZES)]
    if len(base) >= target_size:
        return base
    pad_char = "x"
    padding = pad_char * (target_size - len(base) - 1)  # -1 for separator space
    return f"{base} {padding}"


def _key_value_pairs_payload(req_id: int, total_requests_in_profile: int, req_index: int) -> dict[str, Any]:
    """
    Generate a KeyValuePairs payload for the status-bar zone.

    Implements MergeByKey contention behaviour:
    - First half of each profile: rotating keys ``key-0`` through ``key-31``
      to fill the map (exercises the insert-or-replace-by-key path).
    - Second half of each profile: reuse existing keys to exercise the
      replace-by-key path on an already-full map.
    """
    half = max(total_requests_in_profile // 2, 1)
    if req_index < half:
        # Fill phase: rotate through all 32 key slots
        key = f"key-{req_index % _STATUS_BAR_MERGE_KEYS}"
    else:
        # Reuse phase: repeat the same key rotation to hit existing entries
        key = f"key-{(req_index - half) % _STATUS_BAR_MERGE_KEYS}"
    return {"type": "status_bar", "entries": {key: f"value-{req_id}"}}


def _short_text_with_icon_payload(req_id: int) -> dict[str, Any]:
    """
    Generate a ShortTextWithIcon payload for the notification-area zone.

    The notification-area uses a Stack with max depth 8. Publishing faster than
    the stack drains (during medium/high/burst profiles) intentionally triggers
    eviction of the oldest entries.
    """
    return {"type": "notification", "text": f"Alert {req_id}", "icon": "warning"}


def _solid_color_payload(zone_name: str) -> dict[str, Any]:
    """
    Generate a SolidColor payload for pip and ambient-background zones.

    Uses fixed representative colours per zone so comparisons across runs
    are meaningful.
    """
    if zone_name == "pip":
        return {"type": "solid_color", "r": 0.2, "g": 0.5, "b": 0.8, "a": 1.0}
    # ambient-background: semi-transparent black overlay
    return {"type": "solid_color", "r": 0.0, "g": 0.0, "b": 0.0, "a": 0.5}


def build_content(
    zone: dict[str, Any],
    req_id: int,
    large_payload_index: int | None,
    total_requests_in_profile: int,
    req_index: int,
) -> Any:
    """
    Return the correct content value for a zone based on its media type.

    Dispatches to the appropriate per-media-type generator so each zone
    exercises its real contention path rather than receiving a generic string.

    Args:
        zone: Zone descriptor from ``ZONES``.
        req_id: Monotonic request counter used for unique content.
        large_payload_index: If not None, selects a large-payload size for
            StreamText zones (rotating through ``_LARGE_PAYLOAD_SIZES``).
        total_requests_in_profile: Estimated total requests for this profile
            (used to split MergeByKey fill vs reuse halves).
        req_index: Zero-based index of this request within the profile (used
            for MergeByKey key selection).
    """
    media_type = zone["media_type"]
    zone_name = zone["zone_name"]

    if media_type == "StreamText":
        return _stream_text_payload(req_id, zone_name, large_payload_index)

    if media_type == "KeyValuePairs":
        return _key_value_pairs_payload(req_id, total_requests_in_profile, req_index)

    if media_type == "ShortTextWithIcon":
        return _short_text_with_icon_payload(req_id)

    if media_type == "SolidColor":
        return _solid_color_payload(zone_name)

    # Fallback: plain string — should not happen with current ZONES definition
    return f"Stress test message {req_id}"


# ---------------------------------------------------------------------------
# RPC helper: one MCP tools/call (same pattern as publish_zone_batch.py)
# ---------------------------------------------------------------------------


def rpc_call(
    url: str,
    token: str,
    method: str,
    params: dict[str, Any],
    request_id: int,
    timeout: float = 20.0,
) -> dict[str, Any]:
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": method, "arguments": params},
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        url=url,
        data=body,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {token}",
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        envelope = json.loads(resp.read().decode("utf-8"))
    if "error" in envelope:
        return {"error": envelope["error"]}
    result = envelope["result"]
    decoded = json.loads(result["content"][0]["text"])
    return {"error": decoded} if result.get("isError") else {"result": decoded}


# ---------------------------------------------------------------------------
# Telemetry: background SSH sampling thread
# ---------------------------------------------------------------------------


@dataclass
class TelemetrySample:
    """One per-second snapshot from the remote host."""

    wall_ts: float               # time.time() when sample was taken
    cpu_total_sec: float | None  # raw Get-Process .CPU (cumulative seconds)
    cpu_pct: float | None        # delta CPU% (computed from prev; None for first)
    ws_mb: float | None          # working set MB
    private_mb: float | None     # private memory MB
    gpu_util_pct: float | None
    gpu_mem_mb: float | None


def _parse_proc_line(line: str) -> tuple[float | None, float | None, float | None]:
    """Parse 'cpu_sec ws_mb priv_mb' from PowerShell output."""
    parts = line.strip().split()
    if len(parts) >= 3:
        try:
            return float(parts[0]), float(parts[1]), float(parts[2])
        except ValueError:
            pass
    return None, None, None


def _parse_nvidia_line(line: str) -> tuple[float | None, float | None]:
    """Parse 'util_pct, mem_mb' from nvidia-smi output."""
    parts = [p.strip() for p in line.split(",")]
    if len(parts) >= 2:
        try:
            return float(parts[0]), float(parts[1])
        except ValueError:
            pass
    return None, None


class TelemetryThread:
    """
    Background thread that SSH-samples host metrics every 1 second.

    Maintains a single persistent SSH connection running a remote loop that
    emits one line per second containing process CPU seconds, working set MB,
    private memory MB, GPU utilization %, and GPU memory MB.

    Usage::

        thr = TelemetryThread(user, host, key)
        ok = thr.start()          # False if SSH connection fails
        # ... run load profile ...
        thr.stop()                # join(timeout=5), kill SSH if needed
        samples = thr.samples     # list[TelemetrySample]
        avg_cpu = thr.avg_cpu_pct # float | None

    If SSH fails at start(), returns False and the profile continues with
    telemetry_status="incomplete".
    """

    # Remote script emits one line per second:
    #   "<cpu_sec> <ws_mb> <priv_mb>|<gpu_util>,<gpu_mem>"
    # The '|' separates process stats from GPU stats for unambiguous parsing.
    _REMOTE_LOOP = (
        "while true; do "
        "CPU=$(powershell -NoProfile -NonInteractive -Command \""
        "try { $p = Get-Process tze_hud -ErrorAction Stop | Select-Object -First 1; "
        "Write-Output \\\"$($p.CPU) "
        "$([math]::Round($p.WorkingSet64/1MB,2)) "
        "$([math]::Round($p.PrivateMemorySize64/1MB,2))\\\" "
        "} catch { Write-Output 'none none none' }"
        "\"); "
        "GPU=$(nvidia-smi --query-gpu=utilization.gpu,memory.used "
        "--format=csv,noheader,nounits 2>/dev/null || echo '0,0'); "
        "echo \"${CPU}|${GPU}\"; "
        "sleep 1; "
        "done"
    )

    def __init__(self, user: str, host: str, key: str) -> None:
        self._user = user
        self._host = host
        self._key = key
        self._stop_event = threading.Event()
        self._thread: threading.Thread | None = None
        self._proc: subprocess.Popen | None = None  # type: ignore[type-arg]
        self.samples: list[TelemetrySample] = []
        self._lock = threading.Lock()

    def start(self) -> bool:
        """
        Spawn the background SSH process and start the sampling thread.

        Returns True on success; False if SSH fails to produce output within
        12 seconds (auth error, unreachable host, etc.). On False the caller
        should set telemetry_status="incomplete" and continue the profile.
        """
        try:
            self._proc = subprocess.Popen(
                [
                    "ssh",
                    "-i", self._key,
                    "-o", "BatchMode=yes",
                    "-o", "StrictHostKeyChecking=no",
                    "-o", "ConnectTimeout=8",
                    f"{self._user}@{self._host}",
                    self._REMOTE_LOOP,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
        except Exception as exc:
            print(
                f"WARNING: telemetry SSH launch failed: {exc}",
                file=sys.stderr,
            )
            return False

        # Probe: wait up to 12s for the first output line to confirm connection.
        first_line_event = threading.Event()
        first_line: list[str] = []

        def _read_first() -> None:
            assert self._proc is not None
            assert self._proc.stdout is not None
            line = self._proc.stdout.readline()
            first_line.append(line)
            first_line_event.set()

        probe = threading.Thread(target=_read_first, daemon=True)
        probe.start()
        got_first = first_line_event.wait(timeout=12.0)

        if not got_first or not first_line or not first_line[0].strip():
            stderr_data = ""
            if self._proc.stderr:
                try:
                    stderr_data = self._proc.stderr.read(512)
                except Exception:
                    pass
            print(
                f"WARNING: telemetry SSH produced no output within 12s "
                f"(stderr: {stderr_data.strip()!r}). "
                "Profile will continue without telemetry.",
                file=sys.stderr,
            )
            try:
                self._proc.kill()
            except Exception:
                pass
            return False

        # Seed samples with the first line already read.
        first_sample = self._parse_line(first_line[0], prev_sample=None)
        with self._lock:
            self.samples.append(first_sample)

        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        return True

    def _parse_line(
        self,
        line: str,
        prev_sample: TelemetrySample | None,
    ) -> TelemetrySample:
        """Parse one output line from the remote loop into a TelemetrySample."""
        wall_ts = time.time()
        cpu_total_sec: float | None = None
        ws_mb: float | None = None
        private_mb: float | None = None
        gpu_util: float | None = None
        gpu_mem: float | None = None

        line = line.strip()
        if "|" in line:
            proc_part, gpu_part = line.split("|", 1)
        else:
            proc_part = line
            gpu_part = ""

        cpu_total_sec, ws_mb, private_mb = _parse_proc_line(proc_part)
        if gpu_part:
            gpu_util, gpu_mem = _parse_nvidia_line(gpu_part)

        # Instantaneous CPU% = delta_cpu_seconds / delta_wall_seconds * 100
        cpu_pct: float | None = None
        if (
            prev_sample is not None
            and prev_sample.cpu_total_sec is not None
            and cpu_total_sec is not None
        ):
            dt_wall = wall_ts - prev_sample.wall_ts
            dt_cpu = cpu_total_sec - prev_sample.cpu_total_sec
            if dt_wall > 0 and dt_cpu >= 0:
                cpu_pct = (dt_cpu / dt_wall) * 100.0

        return TelemetrySample(
            wall_ts=wall_ts,
            cpu_total_sec=cpu_total_sec,
            cpu_pct=cpu_pct,
            ws_mb=ws_mb,
            private_mb=private_mb,
            gpu_util_pct=gpu_util,
            gpu_mem_mb=gpu_mem,
        )

    def _run(self) -> None:
        """Thread body: read lines from SSH stdout, build sample list."""
        assert self._proc is not None
        assert self._proc.stdout is not None

        while not self._stop_event.is_set():
            try:
                line = self._proc.stdout.readline()
            except Exception:
                break
            if not line:
                # SSH process exited unexpectedly
                break
            with self._lock:
                prev = self.samples[-1] if self.samples else None
                sample = self._parse_line(line, prev_sample=prev)
                self.samples.append(sample)

    def stop(self) -> None:
        """
        Signal the thread to stop and join with a 5-second timeout.

        If the thread does not stop within 5 seconds (e.g. blocked on
        readline), kill the SSH subprocess to unblock it.
        """
        self._stop_event.set()

        if self._thread is not None:
            self._thread.join(timeout=5.0)
            if self._thread.is_alive():
                if self._proc is not None:
                    try:
                        self._proc.kill()
                    except Exception:
                        pass
                self._thread.join(timeout=1.0)
        elif self._proc is not None:
            # start() probing failed but proc was launched; clean up.
            try:
                self._proc.kill()
            except Exception:
                pass

    @property
    def avg_cpu_pct(self) -> float | None:
        """
        Average CPU% across all samples with valid per-sample cpu_pct.

        Averages individual delta-based cpu_pct values, which correctly handles
        counter resets (process restarts) and gaps in data collection.
        Returns None if no samples with valid cpu_pct exist.
        """
        with self._lock:
            pcts = [s.cpu_pct for s in self.samples if s.cpu_pct is not None and not math.isnan(s.cpu_pct)]
        if not pcts:
            return None
        return sum(pcts) / len(pcts)

    def to_dict_list(self) -> list[dict[str, Any]]:
        """Serialize samples to a list of dicts for the JSON report."""
        with self._lock:
            return [
                {
                    "wall_ts": s.wall_ts,
                    "cpu_pct": s.cpu_pct,
                    "ws_mb": s.ws_mb,
                    "private_mb": s.private_mb,
                    "gpu_util_pct": s.gpu_util_pct,
                    "gpu_mem_mb": s.gpu_mem_mb,
                }
                for s in self.samples
            ]


# ---------------------------------------------------------------------------
# Telemetry: single-shot SSH-based host metrics (kept for backward compat)
# ---------------------------------------------------------------------------


def _ssh_cmd(
    user: str,
    host: str,
    key: str,
    remote_cmd: str,
    timeout: float = 10.0,
) -> str:
    """Run a command on the remote host via SSH and return stdout."""
    result = subprocess.run(
        [
            "ssh",
            "-i", key,
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=no",
            "-o", "ConnectTimeout=8",
            f"{user}@{host}",
            remote_cmd,
        ],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return result.stdout.strip()


def collect_host_metrics(
    user: str,
    host: str,
    key: str,
) -> dict[str, Any]:
    """
    Collect CPU, memory, and GPU metrics from the Windows host.

    Returns a dict with keys: cpu_percent, mem_percent, gpu_util_percent,
    gpu_mem_percent. Values are floats or None on failure.
    """
    metrics: dict[str, Any] = {
        "cpu_percent": None,
        "mem_percent": None,
        "gpu_util_percent": None,
        "gpu_mem_percent": None,
        "error": None,
    }

    # CPU + memory via PowerShell (works on Windows with OpenSSH)
    ps_cpu_mem = (
        "powershell -NoProfile -NonInteractive -Command \""
        "$cpu = (Get-CimInstance Win32_Processor | "
        "Measure-Object -Property LoadPercentage -Average).Average; "
        "$os = Get-CimInstance Win32_OperatingSystem; "
        "$memPct = [math]::Round(100 * ($os.TotalVisibleMemorySize - "
        "$os.FreePhysicalMemory) / $os.TotalVisibleMemorySize, 1); "
        "Write-Output \\\"$cpu $memPct\\\"\""
    )

    # GPU via nvidia-smi (outputs: utilization.gpu, utilization.memory)
    nvidia_cmd = (
        "nvidia-smi --query-gpu=utilization.gpu,utilization.memory "
        "--format=csv,noheader,nounits"
    )

    try:
        cpu_mem_out = _ssh_cmd(user, host, key, ps_cpu_mem)
        parts = cpu_mem_out.split()
        if len(parts) >= 2:
            metrics["cpu_percent"] = float(parts[0])
            metrics["mem_percent"] = float(parts[1])
    except Exception as exc:
        metrics["error"] = f"cpu/mem SSH error: {exc}"

    try:
        gpu_out = _ssh_cmd(user, host, key, nvidia_cmd)
        if gpu_out:
            gpu_parts = [p.strip() for p in gpu_out.split(",")]
            if len(gpu_parts) >= 2:
                metrics["gpu_util_percent"] = float(gpu_parts[0])
                metrics["gpu_mem_percent"] = float(gpu_parts[1])
    except Exception as exc:
        existing = metrics.get("error")
        metrics["error"] = (
            f"{existing}; gpu SSH error: {exc}" if existing else f"gpu SSH error: {exc}"
        )

    return metrics


# ---------------------------------------------------------------------------
# Latency statistics
# ---------------------------------------------------------------------------


def percentile(sorted_data: list[float], p: float) -> float:
    """Compute percentile p (0-100) from a pre-sorted list."""
    if not sorted_data:
        return float("nan")
    idx = (p / 100.0) * (len(sorted_data) - 1)
    lo = int(idx)
    hi = lo + 1
    if hi >= len(sorted_data):
        return sorted_data[-1]
    frac = idx - lo
    return sorted_data[lo] + frac * (sorted_data[hi] - sorted_data[lo])


class _BucketAccumulator:
    """
    Per-second bucketer for load-side time-series data.

    Each request completion records its latency (ms) and outcome into the
    bucket keyed by ``int(completed_at - profile_wall_start)``.  After the
    profile finishes, ``to_time_series()`` converts every bucket into a
    spec-compliant dict entry with the exact field names required by the
    mcp-stress-testing spec (ts, latency_p50_ms, latency_p99_ms,
    reqs_in_interval, errors_in_interval, host_cpu_pct, host_private_mem_mb).

    Thread-safe: ``record()`` acquires a lock, so it is safe to call from
    concurrent executor threads.
    """

    def __init__(self, wall_start: float) -> None:
        self._wall_start = wall_start
        # bucket_idx -> {"latencies": [...], "successes": int, "failures": int}
        self._buckets: dict[int, dict[str, Any]] = {}
        self._lock = threading.Lock()

    def record(self, completed_at: float, latency_ms: float | None, success: bool) -> None:
        """Record one completed request into the appropriate 1-second bucket."""
        bucket_idx = int(completed_at - self._wall_start)
        with self._lock:
            if bucket_idx not in self._buckets:
                self._buckets[bucket_idx] = {"latencies": [], "successes": 0, "failures": 0}
            bucket = self._buckets[bucket_idx]
            if success and latency_ms is not None:
                bucket["latencies"].append(latency_ms)
                bucket["successes"] += 1
            else:
                bucket["failures"] += 1

    def to_time_series(
        self,
        profile_wall_start_epoch: float = 0.0,
        telemetry_samples: list | None = None,
    ) -> list[dict[str, Any]]:
        """
        Return a sorted list of per-second time-series entries.

        Field names match the spec exactly:
          ts                  - unix epoch float for the start of this bucket
          latency_p50_ms      - p50 latency (NaN when no successful requests)
          latency_p99_ms      - p99 latency
          reqs_in_interval    - total requests that completed in this second
          errors_in_interval  - count of failed requests
          host_cpu_pct        - CPU% from telemetry for this second (or null)
          host_private_mem_mb - private memory MB from telemetry (or null)

        The p95 percentile is kept as an extra field for diagnostics.
        ``profile_wall_start_epoch`` is the wall-clock time.time() at which the
        profile started (used to compute ``ts`` from the monotonic bucket index).
        ``telemetry_samples`` is the list of dicts from TelemetryThread.to_dict_list();
        entries are matched to buckets by nearest-neighbor on wall_ts.
        """
        with self._lock:
            # Deep-copy latency lists inside the lock so concurrent record()
            # calls cannot mutate them after we release.
            snapshot = {
                idx: {
                    "latencies": list(b["latencies"]),
                    "successes": b["successes"],
                    "failures": b["failures"],
                }
                for idx, b in self._buckets.items()
            }

        # Build a quick lookup: bucket_second_idx -> telemetry sample (floor join).
        # telemetry_samples are dicts with "wall_ts" (absolute epoch float).
        # We map them to bucket indices via int(floor) to match request bucketing
        # (requests use int(completed_at - wall_start)), so a telemetry sample
        # taken at 1.7s falls into bucket 1, same as requests from 1.0-1.999s.
        telem_by_bucket: dict[int, dict] = {}
        if telemetry_samples and profile_wall_start_epoch > 0:
            for s in telemetry_samples:
                wall_ts = s.get("wall_ts")
                if wall_ts is not None:
                    bucket_idx = int(wall_ts - profile_wall_start_epoch)
                    if bucket_idx not in telem_by_bucket:
                        telem_by_bucket[bucket_idx] = s

        entries: list[dict[str, Any]] = []
        for idx in sorted(snapshot.keys()):
            b = snapshot[idx]
            lats = sorted(b["latencies"])
            ts = profile_wall_start_epoch + idx if profile_wall_start_epoch > 0 else float(idx)
            telem = telem_by_bucket.get(idx)
            entries.append(
                {
                    "ts": ts,
                    "latency_p50_ms": percentile(lats, 50),
                    "latency_p99_ms": percentile(lats, 99),
                    "reqs_in_interval": b["successes"] + b["failures"],
                    "errors_in_interval": b["failures"],
                    "host_cpu_pct": telem.get("cpu_pct") if telem else None,
                    "host_private_mem_mb": telem.get("private_mb") if telem else None,
                }
            )
        return entries


@dataclass
class ProfileResult:
    profile_name: str
    rate_per_sec: float
    duration_sec: int
    total_requests: int = 0
    success_count: int = 0
    error_count: int = 0
    latencies_ms: list = field(default_factory=list)
    host_metrics_start: dict = field(default_factory=dict)
    host_metrics_end: dict = field(default_factory=dict)
    start_ts: float = 0.0
    end_ts: float = 0.0
    # Concurrency and achieved throughput
    concurrency: int = 1
    achieved_rate: float | None = None  # successful_requests / elapsed_wall_time
    # Background telemetry time-series
    telemetry_samples: list = field(default_factory=list)  # list[dict] serialized
    telemetry_avg_cpu_pct: float | None = None
    telemetry_avg_pmem_mb: float | None = None
    telemetry_status: str = "ok"  # "ok" | "incomplete"
    # Per-second load-side time-series (from _BucketAccumulator)
    time_series: list = field(default_factory=list)  # list[dict]

    @property
    def error_rate(self) -> float:
        if self.total_requests == 0:
            return float("nan")
        return self.error_count / self.total_requests

    def latency_stats(self) -> dict[str, float]:
        if not self.latencies_ms:
            return {k: float("nan") for k in ("p50", "p95", "p99", "max", "mean")}
        s = sorted(self.latencies_ms)
        return {
            "p50": percentile(s, 50),
            "p95": percentile(s, 95),
            "p99": percentile(s, 99),
            "max": s[-1],
            "mean": sum(s) / len(s),
        }

    def to_dict(self) -> dict[str, Any]:
        d = asdict(self)
        d["error_rate"] = self.error_rate
        d["latency_stats_ms"] = self.latency_stats()
        # Drop raw latency list from report to keep file compact
        d.pop("latencies_ms", None)
        return d


# ---------------------------------------------------------------------------
# Preflight gate
# ---------------------------------------------------------------------------


def preflight_check(url: str, token: str) -> set[str] | None:
    """
    Call hud_surfaces to verify the MCP endpoint is reachable and discover available zones.

    Returns a set of zone names on success, or None on any failure (connection
    refused, DNS error, HTTP non-200, JSON-RPC error, or timeout). Callers
    should filter the ZONES list against the returned set and warn/skip missing zones.
    """
    try:
        resp = rpc_call(url, token, "hud_surfaces", {}, request_id=0, timeout=5.0)
    except urllib.error.URLError as exc:
        print(
            f"MCP endpoint unreachable at {url}: {exc.reason}",
            file=sys.stderr,
        )
        return None
    except OSError as exc:
        print(
            f"MCP endpoint unreachable at {url}: {exc}",
            file=sys.stderr,
        )
        return None
    except Exception as exc:  # noqa: BLE001
        print(
            f"MCP endpoint unreachable at {url}: {exc}",
            file=sys.stderr,
        )
        return None

    if "error" in resp:
        err = resp["error"]
        print(
            f"MCP endpoint unreachable at {url}: JSON-RPC error {err}",
            file=sys.stderr,
        )
        return None

    surfaces = resp.get("result", {}).get("surfaces", [])
    return {
        s["s"].removeprefix("zone:")
        for s in surfaces
        if isinstance(s, dict) and str(s.get("s", "")).startswith("zone:")
    }


# ---------------------------------------------------------------------------
# Load driver
# ---------------------------------------------------------------------------


class RateController:
    """Emits ticks at a fixed rate using token-bucket logic."""

    def __init__(self, rate_per_sec: float) -> None:
        self._interval = 1.0 / rate_per_sec
        self._next_tick = time.monotonic()

    def wait_for_next(self) -> None:
        now = time.monotonic()
        wait = self._next_tick - now
        if wait > 0:
            time.sleep(wait)
        self._next_tick += self._interval


# ---------------------------------------------------------------------------
# Baseline phases
# ---------------------------------------------------------------------------


def run_network_baseline(url: str, token: str) -> dict[str, Any]:
    """
    Run 100 sequential hud_surfaces JSON-RPC calls at 1/s to measure bare HTTP round-trip.

    No concurrent load, no SSH telemetry. Isolates network + server dispatch
    latency from publish overhead. Results appear as top-level key
    network_baseline in the JSON report.

    Returns a dict with keys: calls, successes, errors, latency_stats_ms.
    latency_stats_ms contains p50, p95, p99, max (all in milliseconds).
    """
    CALLS = 100
    RATE = 1.0  # calls per second
    latencies_ms: list[float] = []
    errors = 0

    print("--- Network baseline: 100x hud_surfaces at 1/s ---", flush=True)
    controller = RateController(RATE)
    for i in range(CALLS):
        controller.wait_for_next()
        t0 = time.monotonic()
        try:
            resp = rpc_call(url, token, "hud_surfaces", {}, request_id=i, timeout=10.0)
            if "error" in resp:
                errors += 1
                print(f"  network_baseline call {i} error: JSON-RPC error {resp['error']}", file=sys.stderr)
                continue
            latencies_ms.append((time.monotonic() - t0) * 1000.0)
        except Exception as exc:
            errors += 1
            print(f"  network_baseline call {i} error: {exc}", file=sys.stderr)

    s = sorted(latencies_ms)
    stats: dict[str, float] = {
        "p50": percentile(s, 50),
        "p95": percentile(s, 95),
        "p99": percentile(s, 99),
        "max": s[-1] if s else float("nan"),
    }
    print(
        f"  network_baseline: calls={CALLS} ok={len(latencies_ms)} err={errors} "
        f"p50={_fmt(stats['p50'])}ms p95={_fmt(stats['p95'])}ms "
        f"p99={_fmt(stats['p99'])}ms max={_fmt(stats['max'])}ms",
        flush=True,
    )
    return {
        "calls": CALLS,
        "successes": len(latencies_ms),
        "errors": errors,
        "latency_stats_ms": stats,
    }


def run_publish_baseline(url: str, token: str) -> dict[str, Any]:
    """
    Run 10 sequential hud_publish calls at 1/s across all 6 default zones.

    One call per second, one zone per call (cycling through ZONES), no
    concurrent load. Measures single-publish latency without load-profile
    pressure. Results appear as top-level key publish_baseline in the JSON
    report.

    Returns a dict with keys: calls, successes, errors, latency_stats_ms.
    latency_stats_ms contains p50, p95, p99, max (all in milliseconds).
    """
    CALLS = 100
    RATE = 1.0  # calls per second
    latencies_ms: list[float] = []
    errors = 0

    print("--- Publish baseline: 100x hud_publish at 1/s ---", flush=True)
    controller = RateController(RATE)
    for i in range(CALLS):
        controller.wait_for_next()
        zone = ZONES[i % len(ZONES)]
        params: dict[str, Any] = {
            "surface": f"zone:{zone['zone_name']}",
            "content": build_content(zone, req_id=i, large_payload_index=None, total_requests_in_profile=CALLS, req_index=i),
            "ttl_ms": DEFAULT_TTL_US // 1000,
            "key": zone["merge_key"],
        }
        t0 = time.monotonic()
        try:
            resp = rpc_call(url, token, "hud_publish", params, request_id=i, timeout=10.0)
            if "error" in resp:
                raise RuntimeError(f"JSON-RPC error: {resp['error']}")
            latencies_ms.append((time.monotonic() - t0) * 1000.0)
        except Exception as exc:
            errors += 1
            print(
                f"  publish_baseline call {i} ({zone['zone_name']}) error: {exc}",
                file=sys.stderr,
            )

    s = sorted(latencies_ms)
    stats: dict[str, float] = {
        "p50": percentile(s, 50),
        "p95": percentile(s, 95),
        "p99": percentile(s, 99),
        "max": s[-1] if s else float("nan"),
    }
    print(
        f"  publish_baseline: calls={CALLS} ok={len(latencies_ms)} err={errors} "
        f"p50={_fmt(stats['p50'])}ms p95={_fmt(stats['p95'])}ms "
        f"p99={_fmt(stats['p99'])}ms max={_fmt(stats['max'])}ms",
        flush=True,
    )
    return {
        "calls": CALLS,
        "successes": len(latencies_ms),
        "errors": errors,
        "latency_stats_ms": stats,
    }


def _dispatch_one(
    url: str,
    token: str,
    req_id: int,
    zone: dict,
    verbose: bool,
    ttl_us: int,
    large_payload_index: int | None,
    total_requests_in_profile: int,
    req_index: int,
) -> tuple[float | None, bool, float]:
    """
    Dispatch a single hud_publish request and return (latency_ms, success, completed_at).

    Thread-safe: uses only immutable arguments and the thread-local stack.
    ``completed_at`` is a monotonic timestamp captured immediately after the
    RPC call returns (or raises), used by the caller to bucket the result
    into the correct 1-second time-series slot.
    Returns (latency_ms, True, completed_at) on success;
    (None, False, completed_at) on any error.

    Args:
        url: MCP endpoint URL.
        token: Bearer auth token.
        req_id: Monotonic request counter (appears in content for traceability).
        zone: Zone descriptor from ``ZONES`` (includes media_type and contention).
        verbose: Print each request result to stdout when True.
        ttl_us: TTL in microseconds (DEFAULT_TTL_US or SHORT_TTL_US).
        large_payload_index: If not None, selects a large payload size for
            StreamText zones. Rotates through ``_LARGE_PAYLOAD_SIZES``.
        total_requests_in_profile: Estimated total for MergeByKey phase split.
        req_index: Zero-based request index within the profile.
    """
    content = build_content(
        zone=zone,
        req_id=req_id,
        large_payload_index=large_payload_index,
        total_requests_in_profile=total_requests_in_profile,
        req_index=req_index,
    )
    params: dict[str, Any] = {
        "surface": f"zone:{zone['zone_name']}",
        "content": content,
        "ttl_ms": ttl_us // 1000,
        "key": zone["merge_key"],
    }
    t0 = time.monotonic()
    try:
        resp = rpc_call(url, token, "hud_publish", params, req_id, timeout=5.0)
        completed_at = time.monotonic()
        if "error" in resp:
            raise RuntimeError(f"JSON-RPC error: {resp['error']}")
        latency_ms = (completed_at - t0) * 1000.0
        if verbose:
            print(
                f"    req={req_id} zone={zone['zone_name']}"
                f" media={zone['media_type']}"
                f" lat={latency_ms:.1f}ms ok"
            )
        return latency_ms, True, completed_at
    except Exception as exc:
        completed_at = time.monotonic()
        latency_ms = (completed_at - t0) * 1000.0
        if verbose:
            print(
                f"    req={req_id} zone={zone['zone_name']}"
                f" media={zone['media_type']}"
                f" lat={latency_ms:.1f}ms ERR: {exc}"
            )
        return None, False, completed_at


def run_profile(
    profile_name: str,
    rate_per_sec: float,
    duration_sec: int,
    url: str,
    token: str,
    ssh_user: str,
    ssh_host: str,
    ssh_key: str,
    verbose: bool = False,
    concurrency: int = 1,
    ttl_us: int = DEFAULT_TTL_US,
    large_payloads: bool = False,
) -> ProfileResult:
    result = ProfileResult(
        profile_name=profile_name,
        rate_per_sec=rate_per_sec,
        duration_sec=duration_sec,
        concurrency=concurrency,
    )

    # Estimate total requests so MergeByKey generators can split fill/reuse phases.
    # This is a best-effort estimate; the actual count may differ due to timing.
    estimated_total = int(rate_per_sec * duration_sec)

    # --- Start background telemetry thread ---
    telem = TelemetryThread(ssh_user, ssh_host, ssh_key)
    print(f"  [{profile_name}] Starting background telemetry thread...", flush=True)
    telem_ok = telem.start()
    if not telem_ok:
        result.telemetry_status = "incomplete"
        print(
            f"  [{profile_name}] WARNING: telemetry unavailable; "
            "profile continues without host metrics.",
            file=sys.stderr,
            flush=True,
        )

    result.start_ts = time.time()
    wall_start = time.monotonic()
    deadline = wall_start + duration_sec
    controller = RateController(rate_per_sec)
    req_id_counter = 1
    zone_cycle = 0
    req_index = 0  # zero-based index within this profile for MergeByKey phase split
    bucketer = _BucketAccumulator(wall_start)

    print(
        f"  [{profile_name}] Running {rate_per_sec}/s "
        f"(concurrency={concurrency}, ttl={'short' if ttl_us == SHORT_TTL_US else 'default'}"
        f"{', large-payloads' if large_payloads else ''}) for {duration_sec}s...",
        flush=True,
    )

    if concurrency > 1:
        # Concurrent dispatch: submit one future per tick, collect results
        # as they complete. Each thread records its own latency independently.
        #
        # large_payload_index and req_index are captured at submission time so
        # the correct phase is associated with each request even under concurrency.
        futures: list[concurrent.futures.Future] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
            while time.monotonic() < deadline:
                controller.wait_for_next()
                if time.monotonic() >= deadline:
                    break
                zone = ZONES[zone_cycle % len(ZONES)]
                zone_cycle += 1
                req_id = req_id_counter
                req_id_counter += 1
                lp_index = req_index if large_payloads else None
                ri = req_index
                req_index += 1
                fut = executor.submit(
                    _dispatch_one,
                    url, token, req_id, zone, verbose,
                    ttl_us, lp_index, estimated_total, ri,
                )
                futures.append(fut)
            # Collect all results (executor waits for running futures on exit)
            for fut in futures:
                try:
                    latency_ms, success, completed_at = fut.result()
                except Exception:
                    latency_ms, success, completed_at = None, False, time.monotonic()
                bucketer.record(completed_at, latency_ms, success)
                result.total_requests += 1
                if success and latency_ms is not None:
                    result.latencies_ms.append(latency_ms)
                    result.success_count += 1
                else:
                    result.error_count += 1
    else:
        # Single-threaded dispatch (concurrency == 1)
        while time.monotonic() < deadline:
            controller.wait_for_next()
            if time.monotonic() >= deadline:
                break
            zone = ZONES[zone_cycle % len(ZONES)]
            zone_cycle += 1
            req_id = req_id_counter
            req_id_counter += 1
            lp_index = req_index if large_payloads else None
            ri = req_index
            req_index += 1
            latency_ms, success, completed_at = _dispatch_one(
                url, token, req_id, zone, verbose,
                ttl_us, lp_index, estimated_total, ri,
            )
            bucketer.record(completed_at, latency_ms, success)
            result.total_requests += 1
            if success and latency_ms is not None:
                result.latencies_ms.append(latency_ms)
                result.success_count += 1
            else:
                result.error_count += 1

    result.end_ts = time.time()
    elapsed_wall = time.monotonic() - wall_start
    if elapsed_wall > 0:
        result.achieved_rate = result.success_count / elapsed_wall

    # --- Stop background telemetry thread ---
    print(f"  [{profile_name}] Stopping telemetry thread...", flush=True)
    telem.stop()

    if telem_ok:
        result.telemetry_samples = telem.to_dict_list()
        result.telemetry_avg_cpu_pct = telem.avg_cpu_pct

    # Merge telemetry into the 1s time-series buckets (spec §Report Output).
    # result.start_ts is time.time() at profile start; used to map bucket indices
    # (monotonic offsets) to absolute timestamps and to match telemetry samples.
    result.time_series = bucketer.to_time_series(
        profile_wall_start_epoch=result.start_ts,
        telemetry_samples=result.telemetry_samples if telem_ok else None,
    )

    # Compute average private memory MB from the raw telemetry samples.
    # Mirrors telemetry_avg_cpu_pct: mean of non-null values, None if all null.
    # Uses raw samples (not bucketed time_series) so gaps and process restarts
    # are handled consistently with how avg_cpu_pct is computed.
    _pmem_vals = [
        s["private_mb"]
        for s in result.telemetry_samples
        if s.get("private_mb") is not None
        and not math.isnan(s["private_mb"])
    ]
    result.telemetry_avg_pmem_mb = (
        sum(_pmem_vals) / len(_pmem_vals) if _pmem_vals else None
    )

    stats = result.latency_stats()
    avg_cpu_str = (
        f"{result.telemetry_avg_cpu_pct:.1f}%"
        if result.telemetry_avg_cpu_pct is not None
        else "n/a"
    )
    n_samples = len(result.telemetry_samples)
    achieved_str = (
        f"{result.achieved_rate:.1f}/s"
        if result.achieved_rate is not None
        else "n/a"
    )
    print(
        f"  [{profile_name}] Done: "
        f"total={result.total_requests} "
        f"ok={result.success_count} "
        f"err={result.error_count} "
        f"tgt={rate_per_sec}/s "
        f"got={achieved_str} "
        f"p50={stats['p50']:.1f}ms "
        f"p99={stats['p99']:.1f}ms "
        f"avg_cpu={avg_cpu_str} "
        f"telem_samples={n_samples}",
        flush=True,
    )

    return result


# ---------------------------------------------------------------------------
# Summary table
# ---------------------------------------------------------------------------


def _fmt(v: Any, decimals: int = 1) -> str:
    if v is None or (isinstance(v, float) and math.isnan(v)):
        return "n/a"
    return f"{v:.{decimals}f}"


def print_summary_table(results: list[ProfileResult]) -> None:
    header = (
        f"{'Profile':<12} {'Tgt/s':>6} {'Got/s':>6} {'Total':>7} {'Errors':>7} "
        f"{'ErrRate':>8} {'p50ms':>7} {'p95ms':>7} {'p99ms':>7} {'maxms':>7} "
        f"{'AvgCPU%':>8} {'PMem MB':>8} {'Samples':>8}"
    )
    sep = "-" * len(header)
    print()
    print(sep)
    print("  MCP Zone Publish Stress Test -- Results")
    print(sep)
    print(header)
    print(sep)

    for r in results:
        stats = r.latency_stats()
        err_pct = (
            "n/a"
            if math.isnan(r.error_rate)
            else f"{r.error_rate * 100:.1f}%"
        )
        avg_cpu = _fmt(r.telemetry_avg_cpu_pct)
        n_samples = len(r.telemetry_samples)
        got_rate = _fmt(r.achieved_rate)
        print(
            f"{r.profile_name:<12} {r.rate_per_sec:>6.0f} {got_rate:>6} "
            f"{r.total_requests:>7} {r.error_count:>7} {err_pct:>8} "
            f"{_fmt(stats['p50']):>7} {_fmt(stats['p95']):>7} "
            f"{_fmt(stats['p99']):>7} {_fmt(stats['max']):>7} "
            f"{avg_cpu:>8} {_fmt(r.telemetry_avg_pmem_mb):>8} {n_samples:>8}"
        )

    print(sep)
    print()


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def _default_output_path() -> str:
    """Generate the default report filename with ISO8601 compact timestamp."""
    ts = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S")
    return f"stress_report_{ts}.json"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="MCP hud_publish stress test with performance telemetry",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument(
        "--url",
        default=None,
        help="MCP URL (default: derived from HUD_HOST)",
    )
    parser.add_argument(
        "--psk-env",
        default=None,
        help="Read the PSK from this environment variable instead of the paired file",
    )
    parser.add_argument(
        "--ssh-user",
        default=DEFAULT_SSH_USER,
        # Intentional: spec uses combined --ssh-host (user@host); we keep them
        # split for TelemetryThread which accepts user and host separately.
        # The combined form is accepted as --ssh-host with user@ prefix.
        help=f"SSH username for host metrics (default: {DEFAULT_SSH_USER})",
    )
    parser.add_argument(
        "--ssh-host",
        default=DEFAULT_SSH_HOST,
        # Spec default: admin-user@windows-host.example.
        # If the value contains '@', it is split into user and host below.
        help=(
            f"SSH hostname (or user@host) for telemetry "
            f"(default: {DEFAULT_SSH_HOST})"
        ),
    )
    parser.add_argument(
        "--ssh-key",
        default=DEFAULT_SSH_KEY,
        help=f"SSH private key path (default: {DEFAULT_SSH_KEY})",
    )
    # Spec §Connection Parameters: --output with dynamic default.
    # We also accept --report as a backward-compatible alias.
    parser.add_argument(
        "--output",
        "--report",
        dest="output",
        default=None,  # resolved to _default_output_path() in main() if not set
        metavar="PATH",
        help=(
            "Path to write JSON report "
            "(default: stress_report_<ISO8601compact>.json)"
        ),
    )
    parser.add_argument(
        "--profiles",
        default="all",
        metavar="PROFILES",
        help=(
            "Comma-separated load profiles to run: "
            + ",".join(p[0] for p in PROFILES)
            + " (default: all)"
        ),
    )
    parser.add_argument(
        "--concurrency",
        type=int,
        default=None,
        metavar="N",
        help="Override per-profile concurrency with a fixed worker count",
    )
    # Spec §Load Profiles: duration override (default 30s per profile).
    parser.add_argument(
        "--duration",
        type=int,
        default=None,
        metavar="SECONDS",
        help=(
            "Override duration in seconds for ALL profiles including burst "
            "(default: per-profile — idle/low/medium/high=30s, burst=10s)"
        ),
    )
    # Spec §TTL Variation: short-ttl flag.
    parser.add_argument(
        "--short-ttl",
        action="store_true",
        default=False,
        help=(
            f"Set publish TTL to {SHORT_TTL_US // 1_000_000}s instead of "
            f"{DEFAULT_TTL_US // 1_000_000}s. "
            "Exercises TTL expiry housekeeping; report includes ttl_mode=short."
        ),
    )
    parser.add_argument(
        "--large-payloads",
        action="store_true",
        default=False,
        help=(
            "Generate StreamText payloads at escalating sizes "
            f"({', '.join(str(s) + 'B' for s in _LARGE_PAYLOAD_SIZES)}, rotating). "
            "Tests how content size affects latency and near-limit behaviour."
        ),
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="Print each request result",
    )
    return parser.parse_args()


def main() -> int:
    global ZONES  # may be filtered by preflight zone discovery
    args = parse_args()

    # Warn if the deprecated --report alias was used.
    if "--report" in sys.argv:
        print(
            "WARNING: --report is deprecated; use --output instead.",
            file=sys.stderr,
        )

    # Resolve output path (dynamic default: ISO8601 compact timestamp).
    output_path = args.output if args.output is not None else _default_output_path()

    # Resolve effective TTL.
    # Spec §TTL Variation: --short-ttl → 1s; default → 60s.
    ttl_us = SHORT_TTL_US if args.short_ttl else DEFAULT_TTL_US
    ttl_mode = "short" if args.short_ttl else "default"

    # Split user@host if the spec-style combined form was given to --ssh-host.
    ssh_host_raw = args.ssh_host
    if "@" in ssh_host_raw:
        ssh_user, ssh_host = ssh_host_raw.split("@", 1)
    else:
        ssh_user = args.ssh_user
        ssh_host = ssh_host_raw

    try:
        args.url, token = hud_env.resolve(args.url, args.psk_env)
    except hud_env.HudEnvError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2

    # Resolve which profiles to run.
    # Spec §Connection Parameters: --profiles is a comma-separated subset.
    raw_profiles = args.profiles.strip()
    if raw_profiles == "all":
        selected_names = {p[0] for p in PROFILES}
    else:
        selected_names = {name.strip() for name in raw_profiles.split(",") if name.strip()}
    valid_names = {p[0] for p in PROFILES}
    unknown = selected_names - valid_names
    if unknown:
        print(
            f"ERROR: unknown profile(s): {', '.join(sorted(unknown))}. "
            f"Valid: {', '.join(p[0] for p in PROFILES)}",
            file=sys.stderr,
        )
        return 1
    # Preserve canonical order from PROFILES.
    profiles_to_run = [p for p in PROFILES if p[0] in selected_names]

    # Apply --duration override if provided.
    # Spec §Load Profiles: --duration overrides the duration of ALL profiles,
    # including burst. The burst default of 10s is just its per-profile default.
    if args.duration is not None:
        profiles_to_run = [
            (name, rate, conc, args.duration)
            for name, rate, conc, _dur in profiles_to_run
        ]

    print("MCP Stress Test")
    print(f"  Target  : {args.url}")
    print(f"  SSH     : {ssh_user}@{ssh_host} (key: {args.ssh_key})")
    print(f"  Output  : {output_path}")
    print(f"  Zones   : {len(ZONES)}")
    print(f"  Profiles: {[p[0] for p in profiles_to_run]}")
    print(f"  TTL mode: {ttl_mode} ({ttl_us // 1_000_000}s)")
    print(f"  Large payloads: {'yes' if args.large_payloads else 'no'}")
    if args.concurrency is not None:
        print(f"  Concurrency override: {args.concurrency} (overrides per-profile defaults)")
    print()

    # Preflight: verify MCP endpoint is reachable and discover available zones.
    # Fails fast with exit code 1 on any connectivity error. Missing zones are
    # skipped with a warning (spec: "skip with a warning rather than failing").
    available_zone_names = preflight_check(args.url, token)
    if available_zone_names is None:
        sys.exit(1)

    original_zones = ZONES
    ZONES = [z for z in ZONES if z["zone_name"] in available_zone_names]
    for z in original_zones:
        if z["zone_name"] not in available_zone_names:
            print(
                f"WARNING: zone '{z['zone_name']}' not found on server — skipping",
                file=sys.stderr,
            )
    if not ZONES:
        print("ERROR: no zones available on server; aborting", file=sys.stderr)
        sys.exit(1)

    run_id = str(uuid.uuid4())
    run_ts = datetime.datetime.utcnow().isoformat() + "Z"

    # Baseline phases: run before load profiles to establish single-call latency
    # reference points. These appear as top-level keys in the JSON report.
    network_baseline = run_network_baseline(args.url, token)
    publish_baseline = run_publish_baseline(args.url, token)

    results: list[ProfileResult] = []

    for prof in profiles_to_run:
        name, rate, profile_concurrency, dur = prof
        # CLI --concurrency overrides the per-profile default
        effective_concurrency = (
            args.concurrency if args.concurrency is not None else profile_concurrency
        )
        print(
            f"--- Profile: {name} ({rate}/s, concurrency={effective_concurrency}, {dur}s) ---",
            flush=True,
        )
        result = run_profile(
            profile_name=name,
            rate_per_sec=rate,
            duration_sec=dur,
            url=args.url,
            token=token,
            ssh_user=ssh_user,
            ssh_host=ssh_host,
            ssh_key=args.ssh_key,
            verbose=args.verbose,
            concurrency=effective_concurrency,
            ttl_us=ttl_us,
            large_payloads=args.large_payloads,
        )
        results.append(result)
        # 3-second cooldown between profiles to let the host settle
        if prof != profiles_to_run[-1]:
            print("  (cooling down 3s...)", flush=True)
            time.sleep(3)

    print_summary_table(results)

    # Write JSON report.
    # Spec §Report Output: include timestamp, target URL, profiles run,
    # total duration, and flags (short-ttl, large-payloads, concurrency).
    report: dict[str, Any] = {
        "run_id": run_id,
        "timestamp": run_ts,
        "mcp_url": args.url,
        "ssh_host": f"{ssh_user}@{ssh_host}",
        "zones_tested": [z["zone_name"] for z in ZONES],
        "network_baseline": network_baseline,
        "publish_baseline": publish_baseline,
        "flags": {
            "short_ttl": args.short_ttl,
            "large_payloads": args.large_payloads,
            "concurrency_override": args.concurrency,
            "duration_override": args.duration,
        },
        "profiles": [r.to_dict() for r in results],
    }
    # Spec §TTL Variation: include ttl_mode: "short" in report when --short-ttl.
    if args.short_ttl:
        report["ttl_mode"] = "short"

    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(report, f, indent=2, ensure_ascii=True)
    print(f"Report written to: {output_path}", flush=True)

    # Exit non-zero if any profile had errors
    return 1 if any(r.error_count > 0 for r in results) else 0


if __name__ == "__main__":
    raise SystemExit(main())
