#!/usr/bin/env python3
"""
Benchmark widget Publish throughput over one gRPC bidirectional Session stream.

This script measures stream-level performance (send phase and end-to-end
completion). Per-request RTT is not reported.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import time
from typing import Any

import grpc

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
if _SCRIPT_DIR not in sys.path:
    sys.path.insert(0, _SCRIPT_DIR)

from proto_gen import session_pb2, session_pb2_grpc, types_pb2

_REFERENCE_DIR = os.path.normpath(os.path.join(_SCRIPT_DIR, "..", "reference"))
DEFAULT_TARGETS_FILE = os.path.join(_REFERENCE_DIR, "targets.json")
DEFAULT_RESULTS_CSV = os.path.join(_REFERENCE_DIR, "results.csv")

SCRIPT_VERSION = "1.0"


def now_wall_us() -> int:
    return int(time.time() * 1_000_000)


class SessionClient:
    def __init__(self, target: str):
        self.target = target
        self.channel: grpc.aio.Channel | None = None
        self.stream = None
        self.send_queue: asyncio.Queue[Any] = asyncio.Queue()
        self.recv_queue: asyncio.Queue[Any] = asyncio.Queue()
        self.seq = 0
        self.reader_task: asyncio.Task[Any] | None = None

        self.bytes_out_total = 0
        self.bytes_out_publish_only = 0
        self.bytes_in_total = 0
        self.payload_counts: dict[str, int] = {}

    def next_seq(self) -> int:
        self.seq += 1
        return self.seq

    async def __aenter__(self) -> "SessionClient":
        self.channel = grpc.aio.insecure_channel(self.target)
        stub = session_pb2_grpc.HudSessionStub(self.channel)
        self.stream = stub.Session(self.request_iter())
        self.reader_task = asyncio.create_task(self.reader_loop())
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.send_queue.put(None)
        if self.reader_task:
            self.reader_task.cancel()
            try:
                await self.reader_task
            except asyncio.CancelledError:
                pass
            except Exception:  # noqa: BLE001
                pass
        if self.channel:
            await self.channel.close()

    async def request_iter(self):
        while True:
            msg = await self.send_queue.get()
            if msg is None:
                return
            yield msg

    async def reader_loop(self) -> None:
        try:
            async for msg in self.stream:
                self.bytes_in_total += int(msg.ByteSize())
                which = msg.WhichOneof("payload") or "none"
                self.payload_counts[which] = self.payload_counts.get(which, 0) + 1
                await self.recv_queue.put(msg)
        except asyncio.CancelledError:
            return
        except Exception as exc:  # noqa: BLE001
            await self.recv_queue.put(exc)

    async def send(self, **payload_kwargs: Any) -> int:
        seq = self.next_seq()
        msg = session_pb2.ClientMessage(
            sequence=seq,
            timestamp_wall_us=now_wall_us(),
            **payload_kwargs,
        )

        size = int(msg.ByteSize())
        self.bytes_out_total += size
        if "publish" in payload_kwargs:
            self.bytes_out_publish_only += size

        await self.send_queue.put(msg)
        return seq

    async def wait_for_payload(self, payload_name: str, timeout_s: float) -> Any:
        deadline = time.monotonic() + timeout_s
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for {payload_name}")
            msg = await asyncio.wait_for(self.recv_queue.get(), timeout=remaining)
            if isinstance(msg, Exception):
                raise RuntimeError(f"stream reader error: {msg}")
            which = msg.WhichOneof("payload")
            if which == payload_name:
                return msg
            if which == "session_error":
                err = msg.session_error
                raise RuntimeError(f"session_error: {err.code} {err.message} ({err.hint})")


def default_benchmark_name(args: argparse.Namespace) -> str:
    return f"grpc-widget-{args.widget_name}-n{args.count}-d{args.duration_ms}"


async def run(args: argparse.Namespace) -> dict[str, Any]:
    target_id, resolved_target, target_meta = resolve_target_endpoint(
        targets_file=args.targets_file,
        target_id=args.target_id,
        direct_endpoint=args.target,
        endpoint_key="grpc_target",
    )
    psk = hud_env.load_psk(resolved_target, args.psk_env)

    benchmark_name = args.benchmark_name or default_benchmark_name(args)

    async with SessionClient(resolved_target) as client:
        await client.send(
            session_init=session_pb2.SessionInit(
                agent_id=args.agent_id,
                auth_credential=session_pb2.AuthCredential(
                    pre_shared_key=session_pb2.PreSharedKeyCredential(key=psk),
                ),
                initial_subscriptions=[],
                min_protocol_version=1000,
                max_protocol_version=1000,
            )
        )
        established = await client.wait_for_payload("session_established", timeout_s=8.0)

        first_send_ts = time.perf_counter()
        interval_s = 0.0
        if args.duration_ms > 0:
            interval_s = (float(args.duration_ms) / 1000.0) / float(args.count)

        for i in range(1, args.count + 1):
            if args.count <= 1:
                value = args.end_value
            else:
                t = float(i - 1) / float(args.count - 1)
                value = args.start_value + (args.end_value - args.start_value) * t
            pct = int(round(value * 100.0))
            label = args.label_template.format(i=i, count=args.count, value=value, pct=pct)

            if interval_s > 0.0:
                deadline = first_send_ts + interval_s * float(i - 1)
                now = time.perf_counter()
                if deadline > now:
                    await asyncio.sleep(deadline - now)

            await client.send(
                publish=session_pb2.Publish(
                    surface=f"widget:{args.widget_name}",
                    params=[
                        types_pb2.WidgetParameterValueProto(param_name="progress", f32_value=float(value)),
                        types_pb2.WidgetParameterValueProto(param_name="label", string_value=label),
                    ],
                    transition_ms=int(args.transition_ms),
                    key=args.merge_key,
                )
            )

        send_done_ts = time.perf_counter()

        result_count = 0
        rejected_count = 0
        result_errors: list[str] = []
        if args.expect_results:
            deadline = time.monotonic() + (float(args.result_timeout_ms) / 1000.0)
            while result_count < args.count and time.monotonic() < deadline:
                remaining = deadline - time.monotonic()
                try:
                    msg = await asyncio.wait_for(client.recv_queue.get(), timeout=max(0.01, remaining))
                except asyncio.TimeoutError:
                    break

                if isinstance(msg, Exception):
                    result_errors.append(str(msg))
                    break

                which = msg.WhichOneof("payload")
                if which == "request_result":
                    result_count += 1
                    res = msg.request_result
                    if not res.ok:
                        rejected_count += 1
                        result_errors.append(f"{res.code}: {res.hint}")
                elif which == "session_error":
                    err = msg.session_error
                    result_errors.append(f"session_error {err.code}: {err.message}")
                    break

        end_ts = time.perf_counter()

        send_phase_ms = (send_done_ts - first_send_ts) * 1000.0
        total_ms = (end_ts - first_send_ts) * 1000.0
        result_drain_ms = (end_ts - send_done_ts) * 1000.0

        missing_results = max(0, args.count - result_count) if args.expect_results else 0
        error_count = rejected_count + missing_results + len(
            [e for e in result_errors if e.startswith("session_error")]
        )
        success_count = max(0, args.count - error_count)
        error_rate = (error_count / args.count) if args.count > 0 else 0.0

        throughput_rps = (success_count / (total_ms / 1000.0)) if total_ms > 0 else 0.0
        send_rps = (args.count / (send_phase_ms / 1000.0)) if send_phase_ms > 0 else 0.0
        end_to_end_rps = (args.count / (total_ms / 1000.0)) if total_ms > 0 else 0.0

        bytes_out_per_success = (client.bytes_out_total / success_count) if success_count > 0 else None
        bytes_in_per_success = (client.bytes_in_total / success_count) if success_count > 0 else None

        primary_key_fields = {
            "schema_version": SCHEMA_VERSION,
            "transport": "grpc_bidi",
            "mode": "widget",
            "target_id": target_id,
            "endpoint": resolved_target,
            "widget_name": args.widget_name,
            "count": args.count,
            "duration_ms_requested": args.duration_ms,
            "transition_ms": int(args.transition_ms),
            "start_value": args.start_value,
            "end_value": args.end_value,
            "merge_key": args.merge_key,
            "label_template_hash": sha256_hex(args.label_template),
            "expect_results": bool(args.expect_results),
            "result_timeout_ms": int(args.result_timeout_ms),
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
            "transport": "grpc_bidi",
            "mode": "widget",
            "target_id": target_id,
            "target": resolved_target,
            "target_description": target_meta.get("description", ""),
            "target_network_scope": target_meta.get("network_scope", ""),
            "benchmark_name": benchmark_name,
            "primary_key": primary_key,
            "count": args.count,
            "duration_ms_requested": args.duration_ms,
            "expect_results": args.expect_results,
            "session": {
                "namespace": established.session_established.namespace,
            },
            "timing": {
                "e2e_latency_ms": round(total_ms, 2),
                "send_phase_ms": round(send_phase_ms, 2),
                "result_drain_ms": round(result_drain_ms, 2),
                "throughput_rps": round(throughput_rps, 2),
                "send_rps": round(send_rps, 2),
                "end_to_end_rps": round(end_to_end_rps, 2),
            },
            "results": {
                "success_count": success_count,
                "error_count": error_count,
                "error_rate": round(error_rate, 4),
                "received": result_count,
                "missing": missing_results,
                "rejected": rejected_count,
                "sample_errors": result_errors[:10],
            },
            "bytes": {
                "bytes_out": client.bytes_out_total,
                "bytes_in": client.bytes_in_total,
                "bytes_out_publish_only": client.bytes_out_publish_only,
                "bytes_out_per_success": round(bytes_out_per_success, 2) if bytes_out_per_success is not None else None,
                "bytes_in_per_success": round(bytes_in_per_success, 2) if bytes_in_per_success is not None else None,
                "note": "Protobuf payload bytes (ByteSize), not full TCP/TLS wire bytes.",
            },
            "payload_counts": client.payload_counts,
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
                "script": "grpc_widget_publish_perf.py",
                "script_version": SCRIPT_VERSION,
                "git_commit": git_commit_short(),
                "benchmark_name": benchmark_name,
                "primary_key": primary_key,
                "transport": "grpc_bidi",
                "mode": "widget",
                "target_id": target_id,
                "endpoint": resolved_target,
                "target_description": target_meta.get("description", ""),
                "target_network_scope": target_meta.get("network_scope", ""),
                "namespace": established.session_established.namespace,
                "widget_name": args.widget_name,
                "count": args.count,
                "success_count": success_count,
                "error_count": error_count,
                "duration_ms_requested": args.duration_ms,
                "e2e_latency_ms": round(total_ms, 2),
                "send_phase_ms": round(send_phase_ms, 2),
                "result_drain_ms": round(result_drain_ms, 2),
                "throughput_rps": round(throughput_rps, 2),
                "send_rps": round(send_rps, 2),
                "end_to_end_rps": round(end_to_end_rps, 2),
                "bytes_out": client.bytes_out_total,
                "bytes_in": client.bytes_in_total,
                "bytes_out_per_success": round(bytes_out_per_success, 2) if bytes_out_per_success is not None else "",
                "bytes_in_per_success": round(bytes_in_per_success, 2) if bytes_in_per_success is not None else "",
                "transition_ms": args.transition_ms,
                "start_value": args.start_value,
                "end_value": args.end_value,
                "merge_key": args.merge_key,
                "result_timeout_ms": args.result_timeout_ms,
                "expect_results": bool(args.expect_results),
                "warnings_count": 0,
                "sample_error": result_errors[0] if result_errors else "",
                "label_template_hash": sha256_hex(args.label_template),
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

        return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="gRPC widget Publish performance benchmark")
    parser.add_argument("--target", default=None, help="Direct gRPC target override (host:port)")
    parser.add_argument("--target-id", default=None, help="Target id from targets file")
    parser.add_argument("--targets-file", default=DEFAULT_TARGETS_FILE, help="Target registry JSON")

    parser.add_argument("--psk-env", help="Read the PSK from this environment variable instead of the paired file")
    parser.add_argument("--agent-id", default="user-test-performance-agent", help="Session agent_id (must be a paired agent in agents.toml with a matching allow)")

    parser.add_argument("--widget-name", default="main-progress", help="Widget instance name")
    parser.add_argument("--count", type=int, default=100, help="Publish count")
    parser.add_argument("--duration-ms", type=int, default=0, help="Target total send duration")
    parser.add_argument("--transition-ms", type=int, default=0, help="Widget transition duration")
    parser.add_argument("--merge-key", default="", help="Merge key for merge-by-key contention")

    parser.add_argument("--start-value", type=float, default=0.01, help="Start value for progress")
    parser.add_argument("--end-value", type=float, default=1.0, help="End value for progress")
    parser.add_argument("--label-template", default="{pct}%", help="Label template")

    expect_group = parser.add_mutually_exclusive_group()
    expect_group.add_argument("--expect-results", action="store_true", dest="expect_results", help="Wait for RequestResult messages")
    expect_group.add_argument("--no-expect-results", action="store_false", dest="expect_results", help="Do not wait for results")
    parser.set_defaults(expect_results=True)

    parser.add_argument("--result-timeout-ms", type=int, default=8000, help="Result drain timeout")

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
    if args.expected_error_rate_max is not None and not (0.0 <= args.expected_error_rate_max <= 1.0):
        raise SystemExit("--expected-error-rate-max must be between 0.0 and 1.0")
    return args


def main() -> int:
    args = parse_args()
    result = asyncio.run(run(args))
    print(json.dumps(result, ensure_ascii=True, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
