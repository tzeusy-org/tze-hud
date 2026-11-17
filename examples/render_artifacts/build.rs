//! Record the source the render-scene binary was compiled from, so its
//! manifest can tell a fresh build from a stale binary in a shared target.

use std::process::Command;

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
    println!("cargo::rerun-if-changed=../../crates");
    println!("cargo::rerun-if-changed=src");
    for path in ["HEAD", "index", "packed-refs", "refs"] {
        if let Some(p) = git(&["rev-parse", "--path-format=absolute", "--git-path", path]) {
            println!("cargo::rerun-if-changed={p}");
        }
    }
    let head = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    // Uncommitted edits to the rendering sources at build time.
    let dirty = git(&[
        "status",
        "--porcelain",
        "--",
        "../../crates",
        "../../examples/render_artifacts",
    ])
    .map_or("unknown", |s| if s.is_empty() { "false" } else { "true" });
    println!("cargo::rustc-env=RENDER_SCENE_BUILD_HEAD={head}");
    println!("cargo::rustc-env=RENDER_SCENE_BUILD_DIRTY={dirty}");
}
