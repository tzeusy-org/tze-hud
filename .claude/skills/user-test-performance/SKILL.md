---
name: user-test-performance
description: Use when running deep performance and throughput investigations for HUD publishing paths (widgets/zones/tiles), including MCP HTTP benchmarks and gRPC bidi stream benchmarks. The canonical gRPC widget publish-load benchmark is `examples/widget_publish_load_harness` (Rust); the Python `grpc_widget_publish_perf.py` script is a secondary alternative.
---

# User Test Performance

Run focused performance drills for publish throughput, latency, and transport bottlenecks.
This skill is separate from `/user-test` functional validation and is tuned for repeatable measurement.

## Skill-Creator + Brainstorming Contract

Before any run, define and document:

1. Question: what exact performance hypothesis are we testing?
2. Target: which `target_id` are we measuring?
3. Workload identity: what is the benchmark primary key?
4. Success criteria: what threshold/regression signal matters?

This skill is built to make those answers machine-auditable and historically comparable.

## Mandatory Audit Metrics

For every benchmark run, record these numerics:

- End-to-end latency: `e2e_latency_ms`
- Throughput: `throughput_rps` (and transport-specific variants)
- Bytes out: `bytes_out`
- Bytes in: `bytes_in`
- Success/error counts

Additional metrics currently tracked (recommended for regression triage):

- MCP: `min/p50/p95/p99/max/mean/stddev` latency
- gRPC: `send_phase_ms`, `result_drain_ms`, `send_rps`, `end_to_end_rps`
- Byte efficiency: `bytes_out_per_success`, `bytes_in_per_success`

Future extensions worth adding:

- host CPU/GPU/memory at start/end
- network RTT/jitter snapshots
- error taxonomy buckets over time

## Deterministic Primary Key

Every run computes a deterministic `primary_key` from normalized benchmark fields
(target, transport, workload params, pacing, etc.).

- Same primary key across different timestamps = same benchmark config
- This enables trend lines and regression detection over time

## Historical Result Storage

Runs append to:

- `./.claude/skills/user-test-performance/reference/results.csv`

This file is intended for version control. Use timestamped rows grouped by
`primary_key` to compare historical performance.

## Target Registry

Targets are defined in:

- `./.claude/skills/user-test-performance/reference/targets.json`

Set `HUD_HOST` (and pair once with `/user-test`'s `hud_pair.py`) and the MCP and
gRPC scripts target that host with no `--target-id`; the PSK is read from
`~/.config/tze-hud/<host>.psk` (`--psk-env NAME` overrides). A `--target-id`
or `--url`/`--target` takes precedence. Add more targets (for example, a remote
MacBook) under new `target_id` keys.

## Scripts

- `scripts/mcp_publish_perf.py`
  - Benchmarks MCP publishes for `widget` or `zone` modes.
  - Supports count, concurrency, pacing, target registry, traceability tags, thresholds, and CSV recording.
- `examples/widget_publish_load_harness` (Rust — **canonical gRPC widget benchmark**)
  - Compiled Rust binary; build with `cargo build --release -p widget_publish_load_harness`.
  - Supports `--mode burst|paced`, `--publish-count`, `--duration-s`, `--target-rate-rps`,
    `--target-p99-rtt-us`, `--target-throughput-rps`, `--normalization-mapping-approved`, and full target registry via
    `--targets-file` (default: `./targets/publish_load_targets.toml`).
  - Outputs a JSON artifact to `benchmarks/publish-load/` by default.
- `scripts/grpc_widget_publish_perf.py` (Python — secondary alternative)
  - Benchmarks `WidgetPublish` on one gRPC bidi stream.
  - Supports pacing, target registry, byte accounting, traceability tags, thresholds, and CSV recording.
  - Uses local `scripts/proto_gen/` stubs (self-contained inside this skill).
- `scripts/widget_soak_runner.py`
  - Runs the Rust gRPC widget harness concurrently for `agent-alpha`, `agent-beta`, and
    `agent-gamma` by default.
  - Defaults to a 60-minute paced soak (`--duration-s 3600`) and writes per-agent
    artifacts plus `soak_summary.json` under `benchmarks/soak/<timestamp>/`.
  - Needs the benchmark Windows config (`app/tze_hud_app/config/benchmark.toml`)
    and a host-local launch; it has not been ported to the pairing flow yet.
- `scripts/compare_results.py`
  - Compares candidate vs baseline runs from `reference/results.csv`.
  - Reports metric deltas and threshold pass/fail for regression gates.

## Run Selection (Progressive Discovery)

Use the minimum run shape that answers the current hypothesis:

1. Transport bottleneck hypothesis (`per-request overhead`, `HTTP connection churn`) -> `mcp_publish_perf.py`
2. Stream throughput hypothesis (`single bidi stream`, `drain/result pacing`) -> `examples/widget_publish_load_harness` (Rust, canonical); fallback: `scripts/grpc_widget_publish_perf.py`
3. Regression hypothesis (`did we get better/worse than prior runs?`) -> `compare_results.py`

If uncertain, start with one fast MCP run (`--count 20`) and one fast gRPC run (`--count 20`), then deepen only the path that regresses.

## Quick Commands

### 1) MCP widget: 100 publishes as fast as possible

```bash
python3 .claude/skills/user-test-performance/scripts/mcp_publish_perf.py \
  --target-id user-test-windows-tailnet \
  --mode widget \
  --widget-name main-progress \
  --count 100 \
  --concurrency 1 \
  --transition-ms 0
```

### 2) MCP zone: 100 publishes over 5 seconds

```bash
python3 .claude/skills/user-test-performance/scripts/mcp_publish_perf.py \
  --target-id user-test-windows-tailnet \
  --mode zone \
  --zone-name subtitle \
  --count 100 \
  --duration-ms 5000
```

### 3) gRPC widget stream (Rust — canonical): 1000 burst publishes

```bash
cargo run --release -p widget_publish_load_harness -- \
  --target-id user-test-windows-tailnet \
  --widget-name main-progress \
  --mode burst \
  --publish-count 1000
```

### 4) gRPC widget stream (Rust — canonical): 100 publishes paced over 5 seconds

```bash
cargo run --release -p widget_publish_load_harness -- \
  --target-id user-test-windows-tailnet \
  --widget-name main-progress \
  --mode paced \
  --duration-s 5 \
  --target-rate-rps 20
```

### 3a) gRPC widget stream (Python — secondary): 100 publishes on one bidi connection

```bash
python3 .claude/skills/user-test-performance/scripts/grpc_widget_publish_perf.py \
  --target-id user-test-windows-tailnet \
  --widget-name main-progress \
  --count 100
```

### 4a) gRPC widget stream (Python — secondary): 100 publishes over 5 seconds

```bash
python3 .claude/skills/user-test-performance/scripts/grpc_widget_publish_perf.py \
  --target-id user-test-windows-tailnet \
  --widget-name main-progress \
  --count 100 \
  --duration-ms 5000
```

### 5) Compare latest run vs prior baseline for same primary key

```bash
python3 .claude/skills/user-test-performance/scripts/compare_results.py \
  --results-csv .claude/skills/user-test-performance/reference/results.csv \
  --target-id user-test-windows-tailnet \
  --transport mcp_http \
  --mode widget
```

## Traceability and Threshold Flags

The Python MCP and gRPC scripts (`mcp_publish_perf.py`, `grpc_widget_publish_perf.py`) support:

- Traceability: `--trace-spec-ref`, `--trace-rfc-ref`, `--trace-doctrine-ref`, `--trace-budget-ref`
- Thresholds: `--expected-e2e-ms-max`, `--expected-p95-ms-max`, `--expected-p99-ms-max`, `--expected-throughput-rps-min`, `--expected-error-rate-max`

These fields are persisted in `results.csv` for auditable historical comparisons.

The Rust harness (`examples/widget_publish_load_harness`) uses structured thresholds via
`--target-p99-rtt-us` and `--target-throughput-rps`, with traceability embedded in the
emitted JSON artifact.

## Notes

- MCP runtime path in this repo is currently one-request-per-connection (no keep-alive), so high-rate streams are transport-limited.
- gRPC byte stats are protobuf payload bytes (`ByteSize`) and not full wire bytes with transport framing.
