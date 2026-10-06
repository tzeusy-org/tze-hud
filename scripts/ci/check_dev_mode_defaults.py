#!/usr/bin/env python3
"""Fail if `dev-mode` is enabled in what a default build of any workspace package ships.

Input: `cargo metadata --format-version 1 --no-deps` JSON on stdin (the same
command `just dev-mode-guard` and the CI dev-mode-guard job run; no build, no
network, no `resolve` graph needed).

Design. `dev-mode` is only defined by workspace crates, so we re-resolve
features ourselves over the workspace dependency edges that `--no-deps`
reports (each dependency carries `kind`, `features`, `uses_default_features`,
`optional` and, for workspace crates, a `path`). For every workspace package P
we simulate `cargo build -p P` with default features:

  * start with P's `default` feature set;
  * follow feature indirection (`a = ["b"]`, `dep:x`, `x/f`, `x?/f`);
  * follow NORMAL and BUILD dependency edges only (dev-dependencies never reach
    a shipped binary, so they must not count), applying each edge's
    `features = [...]` and `default-features = false`;
  * fail if the feature `dev-mode` ends up enabled on ANY package in P's closure.

Why per-package and not one workspace-wide unification: `cargo metadata`'s
`resolve` unifies features across every member, dev-dependencies included
(integration tests enable dev-mode on runtime), so it over-reports for the
release binary. Per-package closures are exactly what a `-p P` build unifies.

`tze_hud_app` (the `tze_hud` release binary) is the shipped artifact and is
labelled as such. Every other workspace package is checked too, so one that is
outside the app closure today cannot silently grow a default dev-mode.
Packages are checked when they have a production binary or library target
(bin, lib, rlib, cdylib, dylib, staticlib or proc-macro). Tests-only packages
(such as `integration`, which enables dev-mode on purpose) and example-only
packages are skipped.
Third-party crates are not followed: none defines `dev-mode`.
"""
import json
import sys

DEV_FEATURE = "dev-mode"
SHIPPED_PACKAGE = "tze_hud_app"


def _dir_of(manifest_path):
    return manifest_path.rsplit("/", 1)[0] if "/" in manifest_path else manifest_path


class Workspace:
    def __init__(self, meta):
        self.pkgs = {p["name"]: p for p in meta["packages"]}
        self.by_dir = {_dir_of(p["manifest_path"]): p["name"] for p in meta["packages"]}

    def edges(self, pkg):
        """Normal/build edges to workspace crates: {dep key: (target name, edge)}."""
        out = {}
        for d in self.pkgs[pkg]["dependencies"]:
            if d.get("kind") == "dev" or not d.get("path"):
                continue
            target = self.by_dir.get(d["path"].rstrip("/"))
            if target is not None:
                out.setdefault(d.get("rename") or d["name"], []).append((target, d))
        return out

    def closure_features(self, root):
        """Return {package: enabled feature names} for `cargo build -p root`."""
        enabled = {}
        # Edges turned on, keyed by (parent, id of the dependency entry). cargo
        # emits one entry per kind AND target for the same crate, each with its
        # own features/default-features, so the key must be per entry.
        active = set()
        # Weak forwarding never activates an optional edge. Retain its requests
        # so a later activation can replay them, independent of feature order.
        weak_forwards = {}
        work = [(root, "default")]
        # Non-optional edges are active as soon as their parent is in the closure.
        seen_pkgs = set()

        def add_pkg(name):
            if name in seen_pkgs:
                return
            seen_pkgs.add(name)
            enabled.setdefault(name, set())
            for key, lst in self.edges(name).items():
                for target, d in lst:
                    if not d["optional"]:
                        activate(name, key, target, d)

        def activate(parent, key, target, d):
            if (parent, id(d)) in active:
                return
            active.add((parent, id(d)))
            add_pkg(target)
            if d["uses_default_features"]:
                work.append((target, "default"))
            for f in d["features"]:
                work.append((target, f))
            for f in weak_forwards.get((parent, key), ()):
                work.append((target, f))

        add_pkg(root)
        while work:
            name, feat = work.pop()
            if feat in enabled[name]:
                continue
            enabled[name].add(feat)
            declared = self.pkgs[name]["features"]
            edges = self.edges(name)
            for item in declared.get(feat, []):
                if item.startswith("dep:"):
                    self._turn_on(name, item[4:], edges, work, activate)
                elif "/" in item:
                    dep, sub = item.split("/", 1)
                    weak = dep.endswith("?")
                    dep = dep.rstrip("?")
                    if weak and dep in edges:
                        weak_forwards.setdefault((name, dep), set()).add(sub)
                    if dep in edges and (
                        not weak or any((name, id(d)) in active for _, d in edges[dep])
                    ):
                        self._turn_on(name, dep, edges, work, activate)
                        for target, _ in edges[dep]:
                            work.append((target, sub))
                else:
                    work.append((name, item))
            # An optional dependency is also an implicit feature of the same name.
            if feat not in declared and feat in edges:
                self._turn_on(name, feat, edges, work, activate)
        return enabled

    @staticmethod
    def _turn_on(parent, key, edges, work, activate):
        for target, d in edges.get(key, []):
            activate(parent, key, target, d)


def is_shippable(pkg):
    kinds = {k for t in pkg["targets"] for k in t["kind"]}
    return bool(kinds & {"bin", "lib", "rlib", "cdylib", "dylib", "staticlib", "proc-macro"})


def main(meta):
    ws = Workspace(meta)
    failed = False
    # Shipped binary first so its verdict leads the output.
    names = sorted(ws.pkgs, key=lambda n: (n != SHIPPED_PACKAGE, n))
    for name in names:
        if not is_shippable(ws.pkgs[name]):
            print(f"SKIP: {name} has no lib/bin target (tests only)")
            continue
        bad = sorted(
            pkg for pkg, feats in ws.closure_features(name).items() if DEV_FEATURE in feats
        )
        label = f"{name} (shipped binary)" if name == SHIPPED_PACKAGE else name
        if bad:
            print(f"FAIL: {DEV_FEATURE} is enabled by default for {label} on: {', '.join(bad)}")
            failed = True
        else:
            print(f"PASS: {DEV_FEATURE} is not enabled by default for {label}")
    if SHIPPED_PACKAGE not in ws.pkgs:
        print(f"FAIL: shipped package {SHIPPED_PACKAGE} not found in cargo metadata")
        failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(json.load(sys.stdin)))
