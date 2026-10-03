# tze_hud dev harness — reproduces the CI gate commands locally.
#
# Prerequisites: just (https://github.com/casey/just), protobuf-compiler (protoc).
#
# Usage:
#   just           # run the default gate (check)
#   just fmt       # format check
#   just clippy    # lint check
#   just test      # unit tests (workspace, excludes integration)
#   just test-gpu  # compositor + pixel_readback GPU tests on llvmpipe
#   just test-integration   # integration headless suites
#   just production-boot    # vertical_slice production config boot
#   just canonical-app-boot # canonical app production config boot
#   just deps-unused        # cargo machete: unused dependencies (blocking in CI)
#   just dead-code <crate>  # advisory list of dead pub items in one crate
#   just dev-mode-guard     # verify dev-mode is not in release default features
#   just idle-efficiency-checker # fail-closed idle artifact contract tests
#   just ci        # full CI gate (all jobs in dependency order, excluding Windows-only; test-gpu is separate)
#
# Every recipe that builds a GPU device pins the Vulkan loader to Mesa llvmpipe
# when its ICD is installed: with a hardware ICD (e.g. NVIDIA) next to it,
# concurrent device construction can wedge the driver. `just test-gpu` (compositor
# + pixel_readback, fails if llvmpipe is missing) is the strict GPU lane. Do not
# run bare `cargo test -p tze_hud_compositor` on a host with a hardware GPU ICD.
#
# Building needs protoc >= 3.15; if /usr/bin/protoc is older, set PROTOC=/path/to/protoc.

# Mesa llvmpipe Vulkan ICD (mesa-vulkan-drivers); GPU recipes use it when present.
lvp := "/usr/share/vulkan/icd.d/lvp_icd.x86_64.json"

# Default recipe: fast compilation gate
default: check

# ── Fast fail ────────────────────────────────────────────────────────────────

# cargo check: fast compilation gate (no codegen)
check:
    cargo check --workspace

# cargo fmt --check: formatting gate (mirror CI fmt job)
fmt:
    cargo fmt --check

# Apply formatting (non-CI helper; not part of CI gate)
fmt-fix:
    cargo fmt

# cargo clippy: lint gate — all targets, deny warnings (mirror CI clippy job)
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# ── Tests ────────────────────────────────────────────────────────────────────

# Unit and crate tests — excludes integration package (mirror CI test-unit job)
# Requires Mesa llvmpipe (libvulkan1 + mesa-vulkan-drivers) for GPU compositor tests.
# Uses the llvmpipe ICD when installed so a hardware ICD is never loaded.
test:
    if [ -f {{lvp}} ]; then export VK_ICD_FILENAMES={{lvp}}; fi; \
    HEADLESS_FORCE_SOFTWARE=1 TZE_HUD_REQUIRE_GPU=1 \
        cargo test \
            --workspace \
            --all-targets \
            --exclude integration

# GPU tests on llvmpipe only: compositor render tests and runtime pixel_readback.
# Fails if the llvmpipe ICD is missing; GPU tests must run, never skip.
# Builds first so the 15-minute timeout bounds test execution, not compilation.
test-gpu:
    test -f {{lvp}} || { echo "missing {{lvp}} (install mesa-vulkan-drivers)"; exit 1; }
    cargo test -p tze_hud_compositor --all-targets --no-run
    cargo test -p tze_hud_runtime --test pixel_readback --features dev-mode --no-run
    VK_ICD_FILENAMES={{lvp}} HEADLESS_FORCE_SOFTWARE=1 LLVMPIPE_CI=1 TZE_HUD_REQUIRE_GPU=1 \
        timeout 900 cargo test -p tze_hud_compositor --all-targets
    VK_ICD_FILENAMES={{lvp}} HEADLESS_FORCE_SOFTWARE=1 LLVMPIPE_CI=1 TZE_HUD_REQUIRE_GPU=1 \
        timeout 900 cargo test -p tze_hud_runtime --test pixel_readback --features dev-mode

# Pure-Python contract tests for the versioned idle artifact gate and its
# startup-atomic Windows launcher.
idle-efficiency-checker:
    python3 scripts/ci/test_check_idle_efficiency.py
    python3 scripts/ci/test_run_quiescent_efficiency_script.py

# Integration headless suites (mirror CI test-integration job)
# Runs every integration target.
test-integration:
    if [ -f {{lvp}} ]; then export VK_ICD_FILENAMES={{lvp}}; fi; \
    HEADLESS_FORCE_SOFTWARE=1 cargo test -p integration --tests

# Pure-Python suites (mirror CI user-test-python-suite job and scripts/ci tests)
# Needs: pip install grpcio protobuf pillow blake3 pytest
test-python:
    python3 -m pytest \
        .claude/skills/user-test/tests/ \
        .claude/skills/user-test/scripts/test_hud_grpc_client.py \
        scripts/tests/ \
        -q
    python3 -m unittest discover -s scripts/ci

# Deterministic LLM-facing token-footprint gate (mirror CI test-integration job)
token-footprint:
    mkdir -p test_results/token-footprint
    if [ -f {{lvp}} ]; then export VK_ICD_FILENAMES={{lvp}}; fi; \
    HEADLESS_FORCE_SOFTWARE=1 cargo run -p benchmark --features headless \
        --bin token_footprint_calibration -- \
        --output test_results/token-footprint/measurement.json
    if [ -f {{lvp}} ]; then export VK_ICD_FILENAMES={{lvp}}; fi; \
    HEADLESS_FORCE_SOFTWARE=1 cargo run -p benchmark --features headless \
        --bin token_footprint_calibration -- \
        --output test_results/token-footprint/repeat.json
    cmp test_results/token-footprint/measurement.json test_results/token-footprint/repeat.json
    python3 scripts/ci/check_token_footprint.py \
        --measurement test_results/token-footprint/measurement.json \
        --baseline scripts/ci/token_footprint_baseline.json \
        --output test_results/token-footprint/gate-report.json

# vertical_slice production config boot (mirror CI production-boot-vertical-slice job)
production-boot:
    if [ -f {{lvp}} ]; then export VK_ICD_FILENAMES={{lvp}}; fi; \
    HEADLESS_FORCE_SOFTWARE=1 \
        cargo test \
            -p vertical_slice \
            --test production_boot \
            -- --nocapture

# Canonical app production config boot (mirror CI canonical-app-production-boot job)
canonical-app-boot:
    if [ -f {{lvp}} ]; then export VK_ICD_FILENAMES={{lvp}}; fi; \
    HEADLESS_FORCE_SOFTWARE=1 \
        cargo test \
            -p tze_hud_app \
            --test production_boot \
            -- --nocapture

# ── Static analysis ──────────────────────────────────────────────────────────

# Verify dev-mode feature is not in release default features (mirror CI dev-mode-guard job)
# Cargo metadata only; the release build is covered by windows.yml.
dev-mode-guard:
    cargo metadata --format-version 1 --no-deps \
        | python3 scripts/ci/check_dev_mode_defaults.py

# Unused dependencies via cargo-machete (cargo install --locked cargo-machete).
# Needs no build. False positives go in [package.metadata.cargo-machete] ignored.
deps-unused:
    cargo machete

# Advisory dead-item list for one crate: narrows its pub items to pub(crate) in a
# temp copy (keeping names other crates use) and prints rustc dead_code warnings.
# Example: just dead-code tze_hud_telemetry
dead-code crate:
    python3 scripts/dead_code.py {{crate}}

# ── Full local CI sweep ───────────────────────────────────────────────────────

# Run all CI gates that are feasible locally (excludes Windows perf budget and
# GPU pixel-readback, which need specific hardware or Mesa llvmpipe + GPU).
# Runs in the same logical order as CI: fast-fail gates first, then tests.
ci: check fmt clippy deps-unused dev-mode-guard idle-efficiency-checker test test-integration test-python token-footprint production-boot canonical-app-boot
