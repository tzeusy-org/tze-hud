# tze_hud dev harness — reproduces the CI gate commands locally.
#
# Prerequisites: just (https://github.com/casey/just), protobuf-compiler (protoc).
#
# Usage:
#   just           # run the default gate (check)
#   just fmt       # format check
#   just clippy    # lint check
#   just test      # unit tests (workspace, excludes integration)
#   just test-integration   # integration headless suites
#   just production-boot    # vertical_slice production config boot
#   just canonical-app-boot # canonical app production config boot
#   just dev-mode-guard     # verify dev-mode is not in release default features
#   just idle-efficiency-checker # fail-closed idle artifact contract tests
#   just ci        # full CI gate (all jobs in dependency order, excluding GPU/Windows-only)
#
# GPU / pixel-readback tests (test-gpu-pixel-readback in CI) are intentionally
# excluded from `just ci` because they require Mesa llvmpipe or a hardware GPU
# and are marked informational-only (continue-on-error) in CI anyway.
# Run explicitly with:
#   HEADLESS_FORCE_SOFTWARE=1 LLVMPIPE_CI=1 TZE_HUD_REQUIRE_GPU=1 \
#     cargo test -p tze_hud_compositor --test pixel_readback --features headless,dev-mode
#
# WARNING: do NOT run `cargo test -p tze_hud_compositor` directly — the
# pixel_readback GPU test deadlocks on headless systems without llvmpipe.
# Use the explicit --test flag shown above to select a specific test binary.

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
# Set HEADLESS_FORCE_SOFTWARE=1 to use software GPU adapter (llvmpipe).
test:
    HEADLESS_FORCE_SOFTWARE=1 TZE_HUD_REQUIRE_GPU=1 \
        cargo test \
            --workspace \
            --all-targets \
            --exclude integration

# Pure-Python contract tests for the versioned idle artifact gate and its
# startup-atomic Windows launcher.
idle-efficiency-checker:
    python3 scripts/ci/test_check_idle_efficiency.py
    python3 scripts/ci/test_run_quiescent_efficiency_script.py

# Integration headless suites (mirror CI test-integration job)
# Excludes: soak (wall-clock, opt-in via TZE_HUD_SOAK_SECS).
test-integration:
    HEADLESS_FORCE_SOFTWARE=1 \
        cargo test \
            -p integration \
            --test multi_agent \
            --test presence_card_tile \
            --test disconnect_orphan \
            --test presence_card_coexistence \
            --test dashboard_tile_creation \
            --test dashboard_tile_input \
            --test dashboard_tile_lifecycle \
            --test subtitle_streaming \
            --test text_stream_portal_surface \
            --test text_stream_portal_adapter \
            --test text_stream_portal_coalescing \
            --test text_stream_portal_governance \
            --test drag_reposition \
            --test movable_elements_e2e

# vertical_slice production config boot (mirror CI production-boot-vertical-slice job)
production-boot:
    HEADLESS_FORCE_SOFTWARE=1 \
        cargo test \
            -p vertical_slice \
            --test production_boot \
            -- --nocapture

# Canonical app production config boot (mirror CI canonical-app-production-boot job)
canonical-app-boot:
    HEADLESS_FORCE_SOFTWARE=1 \
        cargo test \
            -p tze_hud_app \
            --test production_boot \
            -- --nocapture

# ── Static analysis ──────────────────────────────────────────────────────────

# Verify dev-mode feature is not in release default features (mirror CI dev-mode-guard job)
# Note: this only checks Cargo metadata; it does not run a release build.
# For the full belt-and-suspenders release-build check, run the CI job.
dev-mode-guard:
    cargo metadata --format-version 1 --no-deps 2>/dev/null \
        | python3 scripts/ci/check_dev_mode_defaults.py

# ── Full local CI sweep ───────────────────────────────────────────────────────

# Run all CI gates that are feasible locally (excludes Windows perf budget and
# GPU pixel-readback, which need specific hardware or Mesa llvmpipe + GPU).
# Runs in the same logical order as CI: fast-fail gates first, then tests.
ci: check fmt clippy dev-mode-guard idle-efficiency-checker test test-integration production-boot canonical-app-boot
