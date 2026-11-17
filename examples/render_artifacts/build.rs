//! Record the source the render-scene binary was compiled from, so its
//! manifest can tell a fresh build from a stale binary in a shared target.

use std::process::Command;

/// Everything the rendered pixels compile from: the crates, the files they
/// embed (themes, widget bundles, fonts), dependency versions, and this
/// example. Relative to this package.
const SOURCES: &[&str] = &[
    "../../crates",
    "../../assets",
    "../../fonts",
    "../../Cargo.toml",
    "../../Cargo.lock",
    ".",
];

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
}

fn main() {
    // Any rendering source change re-runs this script (cargo scans these
    // directories recursively), as do commits and checkouts. Paths come from
    // git so linked worktrees, whose `.git` is a file, are tracked too.
    for path in SOURCES {
        println!("cargo::rerun-if-changed={path}");
    }
    for path in ["HEAD", "index", "packed-refs", "refs"] {
        if let Some(p) = git(&["rev-parse", "--path-format=absolute", "--git-path", path]) {
            println!("cargo::rerun-if-changed={p}");
        }
    }
    let head = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    // Uncommitted edits to the rendering sources at build time.
    let mut status = vec!["status", "--porcelain", "--"];
    status.extend(SOURCES);
    let dirty = git(&status).map_or("unknown", |s| if s.is_empty() { "false" } else { "true" });
    println!("cargo::rustc-env=RENDER_SCENE_BUILD_HEAD={head}");
    println!("cargo::rustc-env=RENDER_SCENE_BUILD_DIRTY={dirty}");
}
