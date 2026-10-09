# tze_hud dev harness — reproduces the CI gate commands locally.
#
# Prerequisites: just (https://github.com/casey/just), protobuf-compiler (protoc).
#
# Usage:
#   just           # run the default gate (check)
#   just fmt       # format check
#   just clippy    # lint check
#   just test      # workspace tests (excludes integration), incl. GPU + pixel_readback tests
#   just test-gpu  # GPU subset only (compositor + pixel_readback), llvmpipe-pinned, with timeouts
#   just test-integration   # integration headless suites
#   just test-python        # pure-Python suites (pytest + scripts/ci unittest)
#   just token-footprint    # deterministic LLM-facing token-footprint gate
#   just production-boot    # vertical_slice production config boot
#   just canonical-app-boot # canonical app production config boot
#   just deps-unused        # cargo machete: unused dependencies (blocking in CI)
#   just dead-code <crate>  # advisory list of dead pub items in one crate
#   just dev-mode-guard     # verify dev-mode is not enabled in any package's default build
#   just idle-efficiency-checker # fail-closed idle artifact contract tests
#   just clippy-windows-gnu # clippy on the windows-gnu target (skips if target/mingw missing)
#   just cargo-deny         # advisories/licenses/bans/sources (skips if cargo-deny missing)
#   just overlay-harness-contract # pwsh overlay-harness contract test (skips if pwsh missing)
#   just bootstrap # install/report dev-host deps (scripts/dev-bootstrap.sh; --check to report only)
#   just build-windows # cross-build tze_hud.exe for x86_64-pc-windows-gnu
#   just ci        # full local gate sweep (see the `ci` recipe; no Windows-only jobs)
#
# GPU tests (compositor render tests + runtime pixel_readback) already run inside
# `just test` and therefore `just ci`, as they do in the blocking CI test-unit job
# (the explicit feature enables tze_hud_runtime/dev-mode, so pixel_readback
# is built and run). `just test-gpu` runs just that GPU subset and fails if the
# llvmpipe ICD is missing (the strict GPU lane).
#
# Every recipe that builds a GPU device pins the Vulkan loader to Mesa llvmpipe
# when its ICD is installed: with a hardware ICD (e.g. NVIDIA) next to it,
# concurrent device construction can wedge the driver. Do not run bare
# `cargo test -p tze_hud_compositor` on a host with a hardware GPU ICD.
#
# Building needs protoc >= 3.15; if /usr/bin/protoc is older, set PROTOC=/path/to/protoc.

# Mesa llvmpipe Vulkan ICD (mesa-vulkan-drivers); GPU recipes use it when present.
# Older Mesa names it lvp_icd.x86_64.json; Mesa 25+ (Ubuntu 26.04) lvp_icd.json.
lvp := if path_exists("/usr/share/vulkan/icd.d/lvp_icd.x86_64.json") == "true" { "/usr/share/vulkan/icd.d/lvp_icd.x86_64.json" } else { "/usr/share/vulkan/icd.d/lvp_icd.json" }

# Python for the pytest suites: the bootstrap venv when present (CI has none).
py := if path_exists(".venv/bin/python3") == "true" { ".venv/bin/python3" } else { "python3" }

# Default recipe: fast compilation gate
default: check

# Offline authoring: the real windowed build/capture seam, without a HUD listener.
# Arguments go unchanged to the example; compilation is separate from warm timing.
render-scene *args:
    #!/usr/bin/env bash
    set -euo pipefail
    [[ -f "{{lvp}}" ]] || { echo 'required llvmpipe ICD is missing' >&2; exit 1; }
    export VK_ICD_FILENAMES="{{lvp}}" HEADLESS_FORCE_SOFTWARE=1 TZE_HUD_REQUIRE_GPU=1
    cargo run --quiet -p render_artifacts --bin render-scene -- {{args}}

# CPU retained WidgetRenderPlan/resvg mode of the same authoring binary.
render-widget *args:
    cargo run --quiet -p render_artifacts --bin render-scene -- {{args}}

# ── Fast fail ────────────────────────────────────────────────────────────────

# Keep local execution receipts out of Git (mirror the early CI fmt guard).
untracked-evidence:
    bash scripts/ci/check_untracked_evidence.sh

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
# Includes the GPU compositor tests and runtime pixel_readback (the latter via
# the explicit tze_hud_runtime/dev-mode feature). Requires Mesa llvmpipe (libvulkan1 +
# mesa-vulkan-drivers).
# Uses the llvmpipe ICD when installed so a hardware ICD is never loaded.
test:
    #!/usr/bin/env bash
    set -euo pipefail
    for control in SKIP_GPU_TESTS TZE_HUD_SKIP_GPU_TESTS RUST_TEST_THREADS NEXTEST_RETRIES NEXTEST_TEST_THREADS NEXTEST_PROFILE TZE_HUD_PERF_ASSERT TZE_HUD_TEST_BUDGET_SLACK; do
        if [[ -v "$control" ]]; then printf 'unexpected control: %s\n' "$control" >&2; exit 1; fi
    done
    cargo nextest --version | grep -Eq '^cargo-nextest 0\.9\.114([[:space:]]|$)'
    if [ -f {{lvp}} ]; then export VK_ICD_FILENAMES={{lvp}}; fi
    HEADLESS_FORCE_SOFTWARE=1 TZE_HUD_REQUIRE_GPU=1 \
        cargo nextest run \
            --workspace \
            --all-targets \
            --exclude integration \
            --features tze_hud_runtime/dev-mode

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
# Needs scripts/requirements-dev.txt; `just bootstrap` installs it into .venv.
test-python:
    {{py}} -m pytest \
        .claude/skills/user-test/tests/ \
        .claude/skills/user-test/scripts/test_hud_grpc_client.py \
        scripts/tests/ \
        -q
    {{py}} -m unittest discover -s scripts/ci

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

# Verify dev-mode is not enabled in any package's default build, incl. the release binary closure (mirror CI dev-mode-guard job)
# Cargo metadata only; the release build is covered by windows.yml.
dev-mode-guard:
    cargo metadata --format-version 1 --no-deps \
        | python3 scripts/ci/check_dev_mode_defaults.py

# Unused dependencies via cargo-machete (cargo install --locked cargo-machete).
# Needs no build. False positives go in [package.metadata.cargo-machete] ignored.
deps-unused:
    cargo machete

# Tool-gated gates. If the tool is missing the recipe prints "SKIPPED: <reason>"
# and exits 0 (so `just ci` still runs on machines without it); CI always has
# the tool, so these remain blocking there.

# clippy on the windows-gnu cross-target (mirror CI clippy-windows-gnu job).
# Needs `rustup target add x86_64-pc-windows-gnu` and mingw-w64.
clippy-windows-gnu:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! rustup target list --installed | grep -qx x86_64-pc-windows-gnu; then
        echo "SKIPPED: clippy-windows-gnu needs the rustup target (rustup target add x86_64-pc-windows-gnu)"; exit 0
    fi
    if ! command -v x86_64-w64-mingw32-gcc >/dev/null; then
        echo "SKIPPED: clippy-windows-gnu needs the MinGW cross toolchain (apt install mingw-w64)"; exit 0
    fi
    cargo clippy \
        -p tze_hud_runtime \
        -p tze_hud_compositor \
        -p tze_hud_config \
        --target x86_64-pc-windows-gnu \
        -- -D warnings

# Dependency advisory/license policy (mirror CI cargo-deny job).
cargo-deny:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! cargo deny --version >/dev/null 2>&1; then
        echo "SKIPPED: cargo-deny not installed (cargo install --locked cargo-deny)"; exit 0
    fi
    cargo deny check advisories licenses bans sources

# Fullscreen-vs-overlay harness PowerShell contract test (mirror CI check job step).
overlay-harness-contract:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v pwsh >/dev/null; then
        echo "SKIPPED: overlay-harness-contract needs PowerShell (pwsh)"; exit 0
    fi
    pwsh -File ./scripts/ci/windows/test-windowed-fullscreen-overlay-perf.ps1

# Advisory dead-item list for one crate: narrows its pub items to pub(crate) in a
# temp copy (keeping names other crates use) and prints rustc dead_code warnings.
# Example: just dead-code tze_hud_telemetry
dead-code crate:
    python3 scripts/dead_code.py {{crate}}

# ── Dev host ─────────────────────────────────────────────────────────────────

# Idempotent; re-run after adding deps. `just bootstrap --check` reports only.
# Includes mold and native-Linux Cargo flags, preserving existing user settings.
# Cross-builds keep their own target linker/flags; no system ld is replaced.
# Install or report everything this dev host needs
bootstrap *args:
    scripts/dev-bootstrap.sh {{args}}

# Output: target/x86_64-pc-windows-gnu/release/tze_hud.exe. Pass e.g. `-j 8` on low-RAM hosts.
# Cross-build the HUD exe from Linux/WSL (needs `just bootstrap`)
build-windows *args:
    cargo build --release --target x86_64-pc-windows-gnu -p tze_hud_app --bin tze_hud {{args}}

# ── Full local CI sweep ───────────────────────────────────────────────────────

# Run all CI gates that are feasible locally. GPU and pixel_readback tests are
# included via `test` (they need Mesa llvmpipe). clippy-windows-gnu, cargo-deny and
# overlay-harness-contract are tool-gated: they print "SKIPPED: <reason>" and
# pass when their tool (windows-gnu target + mingw, cargo-deny, pwsh) is absent.
# Excluded by design: the Windows-only jobs (windows.yml), the informational
# test-gpu-pixel-readback job (covered by `test`), and the weekly perf lanes.
# Runs in the same logical order as CI: fast-fail gates first, then tests.
ci: untracked-evidence overlay-harness-contract check fmt clippy clippy-windows-gnu cargo-deny deps-unused dev-mode-guard idle-efficiency-checker test test-integration test-python token-footprint production-boot canonical-app-boot
