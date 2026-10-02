# Constrained-Envelope Benchmark Lane

The weekly `constrained-envelope-budget` job (`perf-budget.yml`) is a low-power proxy for catching
regressions under a deliberately small execution envelope. It is not glasses,
VR, wearable-device, or production-hardware qualification. Windows remains the
only product target; this Linux lane exists solely as a repeatable CI proxy.

## Execution contract

The lane runs on GitHub-hosted `ubuntu-latest` with Mesa llvmpipe and constrains
the release benchmark process to exactly two logical CPUs using Linux scheduler
affinity (`taskset`). It runs the existing versioned CPU, GPU, and upload
calibration vector and the same 180-frame benchmark corpus as the Windows
performance gate:

```bash
scripts/ci/run_constrained_envelope.sh
```

For a faster local runner-path smoke, the frame count may be reduced without
changing any budget:

```bash
CONSTRAINED_ENVELOPE_FRAMES=30 scripts/ci/run_constrained_envelope.sh
```

That reduced run is intentionally non-gating: the checker rejects any corpus
other than the canonical 180 frames per scenario, so the command exits nonzero
after still producing diagnostic artifacts. A passing constrained-envelope
result always requires the full corpus.

The runner chooses the first two CPUs from the invoking process's current
`Cpus_allowed_list`; it does not assume CPUs `0-1` are available. The benchmark
then reads its own effective affinity from `/proc/self/status`, so a failed or
ignored `taskset` constraint cannot silently pass. The gate also requires the
Linux lane label and enforcement identity to agree with that observed
`taskset` execution rather than trusting a generic non-empty label.

## Artifact and gate

`test_results/constrained-envelope-budget/benchmark.json` records:

- OS name, version, architecture, and CPU model;
- effective logical-CPU count, allowed CPU list, and enforcement mechanism;
- memory limit and mechanism when a cgroup limit is imposed;
- requested software-renderer state and the actual wgpu backend, adapter name,
  device type, driver identity/details, vendor, and device identifiers;
- 1920x1080 viewport and calibration vector version;
- the canonical four-scenario, 180-frame-per-scenario corpus and frame samples;
- raw CPU/GPU/upload calibration factors and raw benchmark samples.

`scripts/ci/check_constrained_envelope.py` emits `budget-gate.json` with the raw
factors, normalized observations, and normalized ceilings. It imports the
reference factors, metric set, correctness counters, and locked ceilings from
`scripts/ci/check_windows_perf_budgets.py`; the constrained lane has no separate
threshold table to drift or widen. Normalization is:

```text
normalized observation = raw observation * reference factor / current factor
```

The normalized observation must remain within the unchanged reference ceiling.
A slower proxy renderer therefore changes the calibration factor, not the
accepted normalized budget.

The gate fails closed when execution identity is missing, affinity is not
exactly two logical CPUs, software rendering was not requested and verified,
the selected adapter is not Vulkan llvmpipe/softpipe (or Windows DX12 WARP), or
any calibration factor is missing, non-finite, zero, or negative. It also
rejects a truncated benchmark corpus, a non-canonical viewport, incomplete
adapter identity, contradictory or unverified memory-limit metadata, or an
OS/lane/enforcement identity mismatch. The checker is allowed to emit its
diagnostic artifact after the benchmark reports a definitive validation failure,
but the runner preserves that non-zero benchmark status.

## Rollback and waivers

Do not raise or duplicate a ceiling to make this lane green. Diagnose changes
in the raw samples, calibration factors, adapter selection, and execution
identity first. A temporary infrastructure waiver must identify the unavailable
runner or renderer and preserve the failed artifact; it cannot be represented
as a passing performance result. Changes to the reference budgets follow the
approval and evidence rules in
[`about/craft-and-care/engineering-bar.md`](../../about/craft-and-care/engineering-bar.md).
