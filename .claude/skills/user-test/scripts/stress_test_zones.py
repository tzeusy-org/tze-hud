#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""
MCP hud_publish stress test with performance telemetry.

Exercises the MCP hud_publish endpoint at varying load levels while
collecting client-side latency percentiles and throughput. Host CPU, memory and
GPU telemetry are unavailable in this tool; use hud_admin.py status separately
with an admin-paired key for the CPU metric exposed by the HUD.

Connection defaults:
  MCP URL : http://$HUD_HOST:9090/mcp
  PSK     : ~/.config/tze-hud/$HUD_HOST.psk (see hud_pair.py)

CLI flags (see parse_args() for full reference):
  --url           MCP endpoint (default: from HUD_HOST)
  --psk-env       Env var holding the PSK (default: the paired file)
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
    ) -> list[dict[str, Any]]:
        """
        Return a sorted list of per-second time-series entries.

        Field names match the spec exactly:
          ts                  - unix epoch float for the start of this bucket
          latency_p50_ms      - p50 latency (NaN when no successful requests)
          latency_p99_ms      - p99 latency
          reqs_in_interval    - total requests that completed in this second
          errors_in_interval  - count of failed requests
          host_cpu_pct        - unavailable (null)
          host_private_mem_mb - unavailable (null)

        The p95 percentile is kept as an extra field for diagnostics.
        ``profile_wall_start_epoch`` is the wall-clock time.time() at which the
        profile started (used to compute ``ts`` from the monotonic bucket index).
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

        entries: list[dict[str, Any]] = []
        for idx in sorted(snapshot.keys()):
            b = snapshot[idx]
            lats = sorted(b["latencies"])
            ts = profile_wall_start_epoch + idx if profile_wall_start_epoch > 0 else float(idx)
            entries.append(
                {
                    "ts": ts,
                    "latency_p50_ms": percentile(lats, 50),
                    "latency_p99_ms": percentile(lats, 99),
                    "reqs_in_interval": b["successes"] + b["failures"],
                    "errors_in_interval": b["failures"],
                    "host_cpu_pct": None,
                    "host_private_mem_mb": None,
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
    # Host metrics are explicitly unavailable; retain null report fields.
    telemetry_samples: list = field(default_factory=list)  # list[dict] serialized
    telemetry_avg_cpu_pct: float | None = None
    telemetry_avg_pmem_mb: float | None = None
    telemetry_status: str = "unavailable"
    telemetry_reason: str = "Host CPU, memory and GPU sampling is not supported by this tool."
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

    No concurrent load or host telemetry. Isolates network + server dispatch
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

    print(f"  [{profile_name}] Host telemetry unavailable: {result.telemetry_reason}", flush=True)

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

    result.time_series = bucketer.to_time_series(profile_wall_start_epoch=result.start_ts)

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
    print("  Host telemetry: unavailable (client latency and throughput only)")
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
