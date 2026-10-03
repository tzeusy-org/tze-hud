# VerticalFlow Reference-Windows Pixel Proof

This runbook captures the hardware-gated half of `hud-yglp4`. It runs the
production `SceneGraph -> HeadlessRuntime -> compositor -> pixel readback` path
on the tagged TzeHouse Windows reference host and restores the production HUD
to its prior running state.

Do not run the live phase while another GPU lane owns the reference host. The
coordinator or operator must explicitly grant the exclusive window first. The
offline build and CPU-only contract tests are safe while that lane is occupied;
none of the SSH, SCP, helper, controller, or proof commands below are.

## Acceptance Contract

The proof exits nonzero and writes a failed JSON verdict if any required fact is
missing or false:

- the hardware tag is exactly `TzeHouse`, hostname/GPU/driver/OS are present,
  and the wgpu adapter that produced the frame matches that claimed host GPU
  (a CPU/WARP adapter cannot be relabelled as reference-GPU evidence);
- the claimed Windows display is 4096x2160 and independently equals both the
  runtime-observed HeadlessSurface and the 4096x2160 render/readback extent,
  with exactly one RGBA8 frame;
- three flowed child background bands are present at runtime-resolved `y`
  coordinates and do not overlap;
- each child has glyph pixels contained within its own resolved background;
- every pixel in both inter-child gaps and in the full deliberately wrong
  source-`y` child band remains the runtime clear color.

The fixture is local to `render_artifacts`; do not add it to or modify the
canonical `TestSceneRegistry`.

## 1. Offline Controller Preparation

From the assigned worktree, record the exact source and cross-build the isolated
proof executable. This compiles only; do not run the executable on Linux.

```bash
SOURCE_COMMIT=$(git rev-parse HEAD)
cargo build --release --target x86_64-pc-windows-gnu \
  -p render_artifacts --bin vertical-flow-readback
PROOF_EXE=target/x86_64-pc-windows-gnu/release/vertical-flow-readback.exe
sha256sum "$PROOF_EXE"
```

Run the offline gates before requesting the GPU window:

```bash
cargo test -p render_artifacts --lib vertical_flow_proof
cargo test -p render_artifacts \
  --bin vertical-flow-readback
python3 -m unittest scripts.ci.test_vertical_flow_readback_controller -v
```

## 2. Acquire Authority And Resolve The Target

Wait for the current GPU-lane owner to confirm release. Do not infer availability
from a scheduled task name. The controller will fail closed on a live foreign
`C:\ProgramData\tze_hud\gpu.lock`, but that check is a final guard rather than
permission to interrupt another run.

After authorization, resolve the private host/users/key through the canonical
helper. Its output is secret-bearing environment state; do not commit or log it.

```bash
eval "$(.claude/skills/user-test/scripts/tzehouse_env.sh)"
```

Stage the proof and controller under a separate directory. Never overwrite
`C:\tze_hud\tze_hud.exe`, its config, task definition, or PSK state.

```bash
REMOTE_ROOT='C:/tze_hud/proofs/hud-yglp4'
ssh -i "$HUD_SSH_KEY" -o IdentitiesOnly=yes -o BatchMode=yes \
  "$WIN_ADMIN_USER@$WIN_HOST" \
  'powershell -NoProfile -NonInteractive -Command "New-Item -ItemType Directory -Force C:\tze_hud\proofs\hud-yglp4 | Out-Null"'
scp -i "$HUD_SSH_KEY" -o IdentitiesOnly=yes -o BatchMode=yes \
  "$PROOF_EXE" \
  "$WIN_FILE_USER@$WIN_HOST:$REMOTE_ROOT/vertical-flow-readback.exe"
scp -i "$HUD_SSH_KEY" -o IdentitiesOnly=yes -o BatchMode=yes \
  scripts/windows/run_vertical_flow_readback_proof.ps1 \
  "$WIN_FILE_USER@$WIN_HOST:$REMOTE_ROOT/run_vertical_flow_readback_proof.ps1"
```

## 3. Execute Under The Restoration Controller

Use a new timestamped output directory so stale artifacts cannot satisfy the
evidence check. `-AllowProductionStop` records the explicit authority boundary:
the controller verifies that `TzeHudOverlay` executes the exact production path
and that the exact-path production PID owns both canonical listeners before it
stops anything; it then rejects foreign live GPU locks, stops only that PID,
removes only locks whose recorded owner is confirmed dead, acquires its own
PID-owned lock, runs the proof, releases only its own lock, and restarts
`TzeHudOverlay` only if production was running before takeover.

```bash
set -o pipefail
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
REMOTE_OUTPUT="C:\\tze_hud\\proofs\\hud-yglp4\\$STAMP"
LOCAL_OUTPUT="test_results/vertical-flow-readback/$STAMP"
mkdir -p "$LOCAL_OUTPUT"
ssh -i "$HUD_SSH_KEY" -o IdentitiesOnly=yes -o BatchMode=yes \
  "$WIN_ADMIN_USER@$WIN_HOST" \
  "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\\tze_hud\\proofs\\hud-yglp4\\run_vertical_flow_readback_proof.ps1 -ProofExe C:\\tze_hud\\proofs\\hud-yglp4\\vertical-flow-readback.exe -OutputDir $REMOTE_OUTPUT -SourceCommit $SOURCE_COMMIT -AllowProductionStop" \
  2>&1 | tee "$LOCAL_OUTPUT/vertical-flow-controller.log"
PROOF_EXIT=${PIPESTATUS[0]}
```

An exit code of `0` means the pixel contract passed and restoration passed. `1`
means the pixel proof failed but restoration completed. `2` means controller or
capture setup failed. `3` means restoration or controller-evidence writing
failed; treat that as an operational incident and restore the HUD before doing
anything else.

## 4. Collect And Verify Evidence

Copy the timestamped directory back without transforming its source files.

```bash
scp -r -i "$HUD_SSH_KEY" -o IdentitiesOnly=yes -o BatchMode=yes \
  "$WIN_FILE_USER@$WIN_HOST:C:/tze_hud/proofs/hud-yglp4/$STAMP/*" \
  "$LOCAL_OUTPUT/"
sha256sum "$LOCAL_OUTPUT"/*
jq -e '.verdict == "pass" and (.checks | all(.passed))' \
  "$LOCAL_OUTPUT/vertical-flow-readback.json"
jq -e --arg source "$SOURCE_COMMIT" \
  '.source_commit == $source
   and .production_task_action_verified == true
   and ((.production_was_running == false) or (.production_listener_ownership_confirmed == true))
   and .production_restored == true
   and .proof_exit_code == 0' \
  "$LOCAL_OUTPUT/vertical-flow-controller.json"
test -s "$LOCAL_OUTPUT/vertical-flow-readback.ppm"
test "$PROOF_EXIT" -eq 0
```

The required evidence set is:

- `vertical-flow-readback.json` — fail-closed pixel observations and checks;
- `vertical-flow-readback.ppm` — the unmodified 4096x2160 readback frame;
- `vertical-flow-controller.json` — source commit, proof binary checksum,
  original production state, proof exit, and restoration result;
- `vertical-flow-controller.log` — controller stdout/stderr captured locally.

## 5. Restoration Verification

The controller verifies that a previously running production HUD returns under
`TzeHudOverlay` and that its new PID owns both ports 50051 and 9090. Independently
repeat the canonical post-run gates after the controller exits:

```bash
eval "$(.claude/skills/user-test/scripts/tzehouse_env.sh)"
for port in 50051 9090; do
  timeout 6 bash -lc "cat < /dev/null > /dev/tcp/$WIN_HOST/$port"
done
```

If the controller exits `3`, the controller report says
`production_restored=false`, or either port is absent, stop evidence handling and
follow `docs/operations/tzehouse-windows-recovery.md`. Never delete a live GPU
lock or start a second HUD to make the proof green.
