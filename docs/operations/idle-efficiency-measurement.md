# Idle Efficiency Measurement

The quiescent-runtime gate proves the `efficiency.md` rule that an unchanged
HUD does no recurring GPU work and performs only bounded runtime-driven CPU
wakeups. It implements the `efficiency-budgets` runtime-kernel and validation
requirements; it does not infer idleness from frame time or process CPU usage.

## Version 1 artifact contract

`QuiescentEfficiencyArtifact` is defined in
`crates/tze_hud_telemetry/src/idle_efficiency.rs`. Required identity fields are:

- scenario name and version (`quiescent_static_scene`, version `1`)
- runtime build and window mode
- pacing mode and requested cadence
- renderer backend, adapter identity, and software-renderer flag
- viewport, settling duration, observation duration, and measurement status
- constrained-runner OS, CPU model, two-CPU enforcement mechanism, and optional
  memory limit

Required counters are combined and per-loop runtime-driven wakeups, source
attribution, excluded sampler and operating-system wakeups, GPU queue
submissions, surface acquisitions, and presents. Fields have no serde defaults:
an omitted counter is an invalid artifact, never an inferred zero.

The version 1 budget is evaluated without hardware normalization because it is
an exact count invariant:

| Counter | Required result |
|---|---:|
| GPU queue submissions | `0` |
| Surface acquisitions | `0` |
| Presents | `0` |
| Main + compositor runtime-driven wakeups | `<= 120` in 60 seconds |

The observation begins after at least five seconds of settling. Any fixed
cadence, including a 60 Hz benchmark, is active work and cannot satisfy this
gate. A presentation-relevant event ends the quiescent interval; work caused by
that event is not relabeled as idle work.

## Fail-closed checker

Validate a constrained Windows WARP overlay artifact with:

```bash
python3 scripts/ci/check_idle_efficiency.py \
  --require-constrained \
  --require-window-mode overlay \
  --report test_results/idle-efficiency/gate.json \
  test_results/idle-efficiency/measurement.json
```

The constrained checker requires:

- exactly two logical CPUs in the recorded profile and a non-empty enforcement
  identity;
- Vulkan llvmpipe/lavapipe on Linux, or WARP identity on Windows;
- event-driven pacing with no requested cadence;
- internally consistent combined, per-loop, and per-source wakeup totals; and
- every required identity and counter field.

Checked-in versioned pass/fail fixtures live under
`scripts/ci/fixtures/idle-efficiency/`. Run their contract tests with:

```bash
python3 scripts/ci/test_check_idle_efficiency.py
```

## Runner policy

The weekly `windows-performance-budget` job (`perf-budget.yml`) builds the canonical
`tze_hud.exe` and runs `scripts/ci/windows/run-quiescent-efficiency.ps1`. That
harness requests an overlay, forces the windowed compositor's fallback adapter,
and applies the two-logical-CPU affinity while creating `tze_hud.exe` (before
its first presentation can begin). It then waits for the **runtime itself** to
write `test_results/windows-performance-budget/quiescent-efficiency/`
`quiescent-efficiency.json`. The measurement starts only after an actual surface
presentation, then holds the static scene through the five-second settle and
sixty-second observation period.

The checker runs on that emitted artifact with both `--require-constrained` and
`--require-window-mode overlay`. The artifact must record the adapter returned
by wgpu and the affinity observed by the measurement process; requested flags
or environment variables alone are not proof. Sampler wakeups are recorded
separately and do not count as runtime-driven event-loop wakeups.

Windows WARP uses the identical identity and count contract. It is not a
substitute for the live overlay reference-host resource gate. If an exclusive
Windows GPU lane is unavailable, report WARP as unvalidated rather than
substituting hardware D3D12 or claiming a pass.

Artifacts belong under `test_results/idle-efficiency/` in CI and should be
uploaded even when the checker fails so the missing field, identity mismatch,
or over-budget counter remains diagnosable.
