// build.rs — tze_hud_app
//
// On Windows: embed the DPI-awareness application manifest so the overlay covers
// the full physical display at any DPI scaling level, including when launched
// from a scheduled task without an interactive console session.
//
// The manifest (`tze_hud.manifest`) declares `PerMonitorV2` DPI awareness.
// This is processed by the Windows OS loader before any code runs, which
// guarantees that:
//   - `MonitorHandle::size()` returns physical pixels (not logical/virtualised)
//   - `window.inner_size()` returns physical pixels
//   - `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` returns the real monitor DPI
//
// Without this manifest, processes launched by the Task Scheduler may start
// in a DPI-unaware context, causing virtualised pixel dimensions (e.g. a
// 2560×1440 display at 125% DPI reports 2048×1152 to the process).
//
// See: hud-22by — Overlay window doesn't cover full display at >100% DPI scaling
//
// Git SHA embed:
// Captures `git rev-parse --short HEAD` at build time and emits it as the
// TZE_HUD_GIT_SHA cargo env var. The binary's `--version` output appends this
// so that deployed binaries carry their provenance without requiring a bump to
// the workspace version field (frozen at 0.1.0 during pre-release).
//
// Falls back to "unknown" when:
//   - The build directory is not inside a git checkout (e.g. vendored builds)
//   - `git` is not on PATH
//   - The worktree is in a detached HEAD state with no commit reachable

use std::process::Command;

fn capture_git_sha(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn main() {
    // Emit the git SHA so the binary can include it in --version output.
    // Re-run whenever HEAD changes (the .git/HEAD file is updated on every
    // commit, checkout, and merge).
    println!("cargo::rerun-if-changed=../../.git/HEAD");
    println!("cargo::rerun-if-changed=../../.git/refs");
    // embed-resource delegates to windres, which consumes an RC wrapper and
    // then embeds the XML manifest it references. Track both inputs so a
    // manifest-only edit cannot leave a stale Windows executable behind.
    println!("cargo::rerun-if-changed=tze_hud.rc");
    println!("cargo::rerun-if-changed=tze_hud.manifest");
    let sha = capture_git_sha(&["rev-parse", "--short", "HEAD"]);
    println!("cargo::rustc-env=TZE_HUD_GIT_SHA={sha}");
    // Full SHA and release channel for /admin/status. CI sets TZE_HUD_CHANNEL
    // (`dev` on main, the tag name on tags); local builds report `local`.
    let full = capture_git_sha(&["rev-parse", "HEAD"]);
    println!("cargo::rustc-env=TZE_HUD_GIT_SHA_FULL={full}");
    println!("cargo::rerun-if-env-changed=TZE_HUD_CHANNEL");
    let channel = std::env::var("TZE_HUD_CHANNEL")
        .ok()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "local".to_owned());
    println!("cargo::rustc-env=TZE_HUD_CHANNEL={channel}");

    // Build scripts compile for the host, so use Cargo's target metadata rather
    // than a compile-time cfg when cross-building the Windows artifact on Linux.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")
        .expect("Cargo must set CARGO_CFG_TARGET_OS for build scripts");
    if target_os == "windows" {
        embed_resource::compile("tze_hud.rc", embed_resource::NONE)
            .manifest_required()
            .expect("failed to compile required Windows PerMonitorV2 DPI-awareness manifest");
    }
}
