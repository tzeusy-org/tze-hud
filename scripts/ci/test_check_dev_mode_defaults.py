"""Tests for check_dev_mode_defaults.py against fixture `cargo metadata --no-deps` JSON.

The clean workspace mirrors the real shape: app -> runtime -> protocol, with
dev-dependency edges that enable dev-mode (which must not count) and a
tests-only crate. Each mutation must make the guard fail.
"""
import contextlib
import copy
import io
import unittest

import check_dev_mode_defaults as guard


def dep(name, kind=None, features=(), default=True, optional=False):
    return {
        "name": name,
        "kind": kind,
        "features": list(features),
        "uses_default_features": default,
        "optional": optional,
        "path": f"/ws/{name}",
        "rename": None,
    }


def pkg(name, deps=(), features=None, kinds=("lib",)):
    return {
        "name": name,
        "manifest_path": f"/ws/{name}/Cargo.toml",
        "features": features if features is not None else {},
        "dependencies": list(deps),
        "targets": [{"name": name, "kind": list(kinds)}],
    }


def clean():
    return {
        "packages": [
            pkg("tze_hud_app", [dep("tze_hud_runtime")], kinds=("bin",)),
            pkg(
                "tze_hud_runtime",
                [dep("tze_hud_protocol"), dep("tze_hud_protocol", "dev", ["dev-mode"])],
                {"dev-mode": []},
            ),
            pkg("tze_hud_protocol", [], {"dev-mode": []}),
            pkg("extra_tool", [dep("tze_hud_runtime")], kinds=("bin",)),
            # tests-only crate enabling dev-mode via normal deps: not shippable.
            pkg("integration", [dep("tze_hud_runtime", features=["dev-mode"])], kinds=("test",)),
        ]
    }


def by_name(meta, name):
    return next(p for p in meta["packages"] if p["name"] == name)


def run(meta):
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        rc = guard.main(meta)
    return rc, out.getvalue()


class DevModeGuardTest(unittest.TestCase):
    def assertFails(self, meta, needle):
        rc, out = run(meta)
        self.assertEqual(rc, 1, out)
        self.assertIn(needle, out)

    def test_clean_passes(self):
        rc, out = run(clean())
        self.assertEqual(rc, 0, out)
        self.assertIn("tze_hud_app (shipped binary)", out)

    def test_protocol_default_dev_mode_fails(self):
        m = clean()
        by_name(m, "tze_hud_protocol")["features"]["default"] = ["dev-mode"]
        self.assertFails(m, "FAIL: dev-mode is enabled by default for tze_hud_app (shipped binary)")

    def test_app_to_runtime_feature_edge_fails(self):
        m = clean()
        by_name(m, "tze_hud_app")["dependencies"][0]["features"] = ["dev-mode"]
        self.assertFails(m, "tze_hud_app (shipped binary) on: tze_hud_runtime")

    def test_feature_indirection_fails(self):
        m = clean()
        rt = by_name(m, "tze_hud_runtime")
        rt["features"].update({"default": ["dev"], "dev": ["dev-mode"]})
        self.assertFails(m, "tze_hud_app (shipped binary)")

    def test_forwarded_dep_feature_fails(self):
        m = clean()
        rt = by_name(m, "tze_hud_runtime")
        rt["features"].update({"default": ["tze_hud_protocol/dev-mode"]})
        self.assertFails(m, "on: tze_hud_protocol")

    def test_package_outside_app_closure_fails(self):
        m = clean()
        by_name(m, "extra_tool")["features"].update(
            {"dev-mode": [], "default": ["dev-mode"]}
        )
        rc, out = run(m)
        self.assertEqual(rc, 1, out)
        self.assertIn("PASS: dev-mode is not enabled by default for tze_hud_app", out)
        self.assertIn("FAIL: dev-mode is enabled by default for extra_tool", out)

    def test_optional_dep_not_activated_passes(self):
        m = clean()
        app = by_name(m, "tze_hud_app")
        app["dependencies"].append(dep("tze_hud_protocol", features=["dev-mode"], optional=True))
        rc, out = run(m)
        self.assertEqual(rc, 0, out)

    def test_second_target_specific_edge_enabling_dev_mode_fails(self):
        m = clean()
        app = by_name(m, "tze_hud_app")
        # First edge is featureless; cargo emits a separate entry per target.
        windows = dep("tze_hud_runtime", features=["dev-mode"])
        windows["target"] = "cfg(windows)"
        app["dependencies"].append(windows)
        self.assertFails(m, "tze_hud_app (shipped binary) on: tze_hud_runtime")

    def test_second_build_kind_edge_enabling_dev_mode_fails(self):
        m = clean()
        app = by_name(m, "tze_hud_app")
        app["dependencies"].append(dep("tze_hud_runtime", "build", ["dev-mode"]))
        self.assertFails(m, "tze_hud_app (shipped binary) on: tze_hud_runtime")

    def test_multiple_clean_edges_to_same_crate_pass(self):
        m = clean()
        app = by_name(m, "tze_hud_app")
        windows = dep("tze_hud_runtime")
        windows["target"] = "cfg(windows)"
        app["dependencies"] += [windows, dep("tze_hud_runtime", "build")]
        rc, out = run(m)
        self.assertEqual(rc, 0, out)

    def test_missing_shipped_package_fails(self):
        m = clean()
        m["packages"] = [p for p in m["packages"] if p["name"] != "tze_hud_app"]
        self.assertFails(m, "shipped package tze_hud_app not found")


if __name__ == "__main__":
    unittest.main()
