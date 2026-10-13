#!/usr/bin/env python3
"""Hermetic ref-selection behavior; fake just is not a compiler/hook proof."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SELECTOR = Path(__file__).resolve().parents[1] / "pre_push.py"
SOURCE_GUARD = SELECTOR.with_name("cargo_source_guard.py")


class PrePushTests(unittest.TestCase):
    def test_ref_selection_and_checker_failure(self) -> None:
        # Owned fixtures stay under actual HOME; no HOME/USERPROFILE override.
        fixture_base = Path.home() / ".cache" / "tze-hud-test-fixtures"
        fixture_base.mkdir(mode=0o700, parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="pre-push-", dir=fixture_base) as temporary:
            root = Path(temporary)
            repo = root / "repo"
            repo.mkdir()
            bin_dir = root / "bin"
            bin_dir.mkdir()
            ledger = root / "checks.jsonl"
            env = {
                name: value for name, value in os.environ.items()
                if not name.startswith("GIT_") and name not in {
                    "TZE_HUD_SHARED_TARGET_GUARD", "CARGO_TARGET_DIR",
                }
            }
            env.update(
                GIT_CONFIG_NOSYSTEM="1",
                GIT_CONFIG_GLOBAL=os.devnull,
                PATH=str(bin_dir) + os.pathsep + env.get("PATH", ""),
                FAKE_PRE_PUSH_LEDGER=str(ledger),
            )

            def git(*args: str) -> str:
                result = subprocess.run(
                    ["git", *args], cwd=repo, env=env,
                    capture_output=True, text=True, check=True,
                )
                return result.stdout.strip()

            def commit(path: str, content: str) -> str:
                (repo / path).write_text(content)
                git("add", path)
                git("commit", "-qm", "fixture")
                return git("rev-parse", "HEAD")

            git("init", "-q", "-b", "main", "--template=")
            git("config", "user.name", "Pre-push fixture")
            git("config", "user.email", "pre-push@example.invalid")
            base = commit("README.md", "base\n")
            git("update-ref", "refs/remotes/origin/main", base)
            docs = commit("README.md", "docs\n")
            rust = commit("lib.rs", "pub fn example() {}\n")
            cargo = commit("Cargo.toml", '[package]\nname = "fixture"\nversion = "0.1.0"\n')
            git("tag", "-a", "rust-tag", "-m", "fixture")
            tag = git("rev-parse", "refs/tags/rust-tag")
            git("checkout", "-qb", "other-rust", base)
            other_rust = commit("other.rs", "pub fn other() {}\n")
            git("checkout", "-qb", "other-docs", base)
            other_docs = commit("other.md", "other docs\n")
            git("checkout", "-q", "main")

            fake = bin_dir / "fake_just.py"
            fake.write_text(
                "import json, os, pathlib, subprocess, sys\n"
                "with open(os.environ['FAKE_PRE_PUSH_LEDGER'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "if os.environ.get('FAKE_PRE_PUSH_DIRTY'):\n"
                "    pathlib.Path('README.md').write_text('changed during checks\\n')\n"
                "if os.environ.get('FAKE_SHARED_GUARD'):\n"
                "    sys.exit(subprocess.call([sys.executable, os.environ['SOURCE_GUARD'], '--', 'clippy', '--workspace', '--all-targets', '--', '-D', 'warnings']))\n"
                "sys.exit(int(os.environ.get('FAKE_PRE_PUSH_EXIT', '0')))\n"
            )

            def install_fake(name: str, source: Path) -> None:
                if os.name == "nt":
                    (bin_dir / (name + ".cmd")).write_text(
                        f'@"{sys.executable}" "{source}" %*\n'
                    )
                else:
                    executable = bin_dir / name
                    executable.write_text("#!/usr/bin/env python3\n" + source.read_text())
                    executable.chmod(0o755)

            install_fake("just", fake)

            zero = "0" * len(base)

            def update(oid: str, ref: str = "main", old: str = zero) -> str:
                return f"refs/heads/{ref} {oid} refs/heads/{ref} {old}\n"

            def invoke(stdin: str = "", mode: str = "git-stdin", **controls: str):
                ledger.unlink(missing_ok=True)
                result = subprocess.run(
                    [sys.executable, str(SELECTOR), mode], cwd=repo,
                    env={**env, **controls}, input=stdin,
                    capture_output=True, text=True, timeout=20,
                )
                calls = [json.loads(line) for line in ledger.read_text().splitlines()] if ledger.exists() else []
                return result, calls

            for label, stdin, expected_calls in [
                ("docs new ref", update(docs, "docs"), []),
                ("delete", f"(delete) {zero} refs/heads/gone {cargo}\n", []),
                ("empty input", "", []),
                ("multiple docs heads", update(docs, "docs") + update(other_docs, "other-docs"), []),
                ("current Rust/Cargo head", update(cargo), [["pre-push-check"]]),
                ("Rust and docs heads", update(cargo) + update(other_docs, "docs"), [["pre-push-check"]]),
                ("commit-bearing tag", f"refs/tags/rust-tag {tag} refs/tags/rust-tag {zero}\n", [["pre-push-check"]]),
            ]:
                with self.subTest(label=label):
                    result, calls = invoke(stdin)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(calls, expected_calls)

            for label, stdin in [
                ("uncovered Rust head", update(cargo) + update(other_rust, "other-rust")),
                ("noncurrent Rust ancestor", update(rust, "older-rust")),
                ("missing object", update("f" * len(base))),
                ("malformed tuple", f"refs/heads/main {cargo} refs/heads/main\n"),
                ("invalid object ID", update("not-an-object")),
                ("noncommit object", update(git("rev-parse", "HEAD:README.md"), "blob")),
            ]:
                with self.subTest(label=label):
                    result, calls = invoke(stdin)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(calls, [])

            result, calls = invoke(mode="head")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, [["pre-push-check"]])

            # Each extension triggers checks independently of a .rs difference.
            git("update-ref", "refs/remotes/origin/main", rust)
            result, calls = invoke(update(cargo))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, [["pre-push-check"]])
            git("update-ref", "refs/remotes/origin/main", base)

            git("update-ref", "-d", "refs/remotes/origin/main")
            try:
                result, calls = invoke(update(docs, "docs"))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])
                result, calls = invoke(f"(delete) {zero} refs/heads/gone {cargo}\n")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, [])
            finally:
                git("update-ref", "refs/remotes/origin/main", base)

            result, calls = invoke(update(cargo), FAKE_PRE_PUSH_EXIT="37")
            self.assertEqual(result.returncode, 37, result.stderr)
            self.assertEqual(calls, [["pre-push-check"]])
            (repo / "lib.rs").write_text("uncommitted\n")
            result, calls = invoke(update(cargo))
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])
            git("restore", "lib.rs")
            result, calls = invoke(update(cargo), FAKE_PRE_PUSH_DIRTY="1")
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [["pre-push-check"]])
            git("restore", "README.md")

            # Independent OS lock holder proves bounded hook contention, without
            # invoking a compiler or making this an installed-hook acceptance.
            with (repo / ".git/pre-push-check.lock").open("a+b") as lock:
                if os.name == "nt":
                    import msvcrt

                    if lock.seek(0, os.SEEK_END) == 0:
                        lock.write(b"\0")
                        lock.flush()
                    lock.seek(0)
                    msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
                else:
                    import fcntl

                    fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                try:
                    result, calls = invoke(update(cargo))
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("another pre-push check", result.stderr)
                    self.assertEqual(calls, [])
                finally:
                    if os.name == "nt":
                        lock.seek(0)
                        msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
                    else:
                        fcntl.flock(lock.fileno(), fcntl.LOCK_UN)

            locked = commit("Cargo.lock", "# fixture lockfile\n")
            git("update-ref", "refs/remotes/origin/main", cargo)
            result, calls = invoke(update(locked))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, [["pre-push-check"]])

            # Bootstrap targets Linux/WSL. Native Windows still runs every
            # selector assertion; it has no Linux host-bootstrap contract.
            if os.name != "nt":
                import shutil

                hook_repo = root / "hook-repo"
                (hook_repo / "scripts").mkdir(parents=True)
                (hook_repo / ".githooks").mkdir()
                bootstrap = hook_repo / "scripts/dev-bootstrap.sh"
                shutil.copy2(SELECTOR.with_name("dev-bootstrap.sh"), bootstrap)
                shutil.copy2(
                    SELECTOR.parents[1] / ".githooks/pre-push",
                    hook_repo / ".githooks/pre-push",
                )

                def hook_git(*args: str) -> str:
                    result = subprocess.run(
                        ["git", *args], cwd=hook_repo, env=env,
                        capture_output=True, text=True, check=True,
                    )
                    return result.stdout.strip()

                def hook_bootstrap(*args: str):
                    return subprocess.run(
                        ["bash", str(bootstrap), *args], cwd=hook_repo, env=env,
                        capture_output=True, text=True, timeout=20,
                    )

                hook_git("init", "-q", "-b", "main", "--template=")
                hook_git("config", "user.name", "Pre-push fixture")
                hook_git("config", "user.email", "pre-push@example.invalid")
                hook_git("add", "scripts/dev-bootstrap.sh", ".githooks/pre-push")
                hook_git("commit", "-qm", "exact copied hook/bootstrap")
                config = hook_repo / ".git/config"
                before = config.read_bytes()
                orders = [("--hooks-only", "--check"), ("--check", "--hooks-only")]
                for order in orders:
                    with self.subTest(missing_hook_check_order=order):
                        result = hook_bootstrap(*order)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertEqual(config.read_bytes(), before)
                result = hook_bootstrap("--hooks-only")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(hook_git("config", "--local", "core.hooksPath"), ".githooks")
                installed = config.read_bytes()
                for order in orders:
                    with self.subTest(installed_hook_check_order=order):
                        result = hook_bootstrap(*order)
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(config.read_bytes(), installed)
                result = hook_bootstrap("--hooks-only")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(config.read_bytes(), installed)

                hook_git("config", "--local", "core.hooksPath", "custom-hooks")
                custom_config = config.read_bytes()
                result = hook_bootstrap("--hooks-only")
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(config.read_bytes(), custom_config)
                hook_git("config", "--local", "core.hooksPath", ".githooks")
                common_hooks = hook_repo / ".git/hooks"
                common_hooks.mkdir(exist_ok=True)
                unrelated = common_hooks / "pre-commit"
                unrelated.write_text("#!/bin/sh\nexit 0\n")
                guarded = config.read_bytes()
                result = hook_bootstrap("--hooks-only")
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(config.read_bytes(), guarded)
                self.assertEqual(unrelated.read_text(), "#!/bin/sh\nexit 0\n")

            # Shared-target admission is exercised at the real CLI with a fake
            # compiler/Cargo child. The separate authorized real two-source
            # oracle proves Cargo types and test declarations, not this fixture.
            cargo_ledger = root / "cargo.jsonl"
            fake_rustc = bin_dir / "fake_rustc.py"
            fake_rustc.write_text("print('rustc 1.88.0\\nrelease: 1.88.0\\nhost: fixture')\n")
            fake_cargo = bin_dir / "fake_cargo.py"
            fake_cargo.write_text(
                "import json, os, pathlib, signal, sys\n"
                "with open(os.environ['FAKE_CARGO_LEDGER'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "target = pathlib.Path(os.environ['CARGO_TARGET_DIR'])\n"
                "target.joinpath('owned-artifact').write_text('owned fixture artifact\\n')\n"
                "if os.environ.get('FAKE_CARGO_TERMINATE'):\n"
                "    os.kill(os.getpid(), signal.SIGTERM)\n"
                "if os.environ.get('FAKE_SOURCE_CHANGE'):\n"
                "    pathlib.Path('lib.rs').write_text('changed during Cargo\\n')\n"
                "sys.exit(int(os.environ.get('FAKE_CARGO_EXIT', '0')))\n"
            )
            install_fake("rustc", fake_rustc)
            install_fake("cargo", fake_cargo)
            guard_env = {
                **env, "TZE_HUD_SHARED_TARGET_GUARD": "1",
                "FAKE_CARGO_LEDGER": str(cargo_ledger),
                "SOURCE_GUARD": str(SOURCE_GUARD), "RUSTUP_TOOLCHAIN": "1.88.0",
            }
            # Fixtures inherit no custom compiler/config target or toolchain.
            for name in (
                "RUSTC", "RUSTDOC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_HOME",
                "CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTDOC", "CARGO_BUILD_RUSTC_WRAPPER",
                "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_BUILD_DIR",
            ):
                guard_env.pop(name, None)
            guard_env["CARGO_HOME"] = str(root / "owned-cargo-home")
            for name in list(guard_env):
                if name.startswith("CARGO_TARGET_") and name.endswith("_RUNNER"):
                    guard_env.pop(name)
            target = root / "guard-target"

            def guarded(selected: Path = target, args=("check", "--workspace"), cwd: Path = repo, tool_probe=False, **controls: str):
                cargo_ledger.unlink(missing_ok=True)
                result = subprocess.run(
                    [sys.executable, str(SOURCE_GUARD), *(["--tool-available"] if tool_probe else []), "--", *args], cwd=cwd,
                    env={**guard_env, "CARGO_TARGET_DIR": str(selected), **controls},
                    capture_output=True, text=True, timeout=30,
                )
                calls = [json.loads(line) for line in cargo_ledger.read_text().splitlines()] if cargo_ledger.exists() else []
                return result, calls

            for label, controls, expected in [
                ("fresh target", {}, 0),
                ("warm same source", {}, 0),
                ("first child failure", {"FAKE_CARGO_EXIT": "37"}, 37),
            ]:
                with self.subTest(shared_target=label):
                    result, calls = guarded(**controls)
                    self.assertEqual(result.returncode, expected, result.stderr)
                    self.assertEqual(calls, [["check", "--workspace"]])

            result, calls = guarded(args=("check", "--target-dir", str(target)))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, [["check", "--target-dir", str(target)]])
            result, calls = guarded(args=("check", "--manifest-path", "Cargo.toml"))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, [["check", "--manifest-path", "Cargo.toml"]])

            compiler_source = fake_rustc.read_bytes()
            fake_rustc.write_text("print('rustc 1.88.0\\nrelease: 1.88.0\\nhost: different-fixture')\n")
            install_fake("rustc", fake_rustc)
            result, calls = guarded()
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])
            fake_rustc.write_bytes(compiler_source)
            install_fake("rustc", fake_rustc)

            # Completed absence is distinct from admission/postcheck failure;
            # the latter cannot become cargo-deny's missing-tool SKIPPED path.
            probe_target = root / "probe-target"
            for child_exit, expected in [("0", 0), ("101", 1)]:
                result, calls = guarded(probe_target, args=("deny", "--version"), tool_probe=True, FAKE_CARGO_EXIT=child_exit)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(calls, [["deny", "--version"]])
            result, calls = guarded(probe_target, args=("deny", "--version"), tool_probe=True, FAKE_CARGO_EXIT="101", FAKE_SOURCE_CHANGE="1")
            self.assertEqual(result.returncode, 86)
            self.assertEqual(calls, [["deny", "--version"]])
            self.assertFalse(json.loads((probe_target / ".tze-hud-source-binding.json").read_text())["valid"])
            git("restore", "lib.rs")
            if os.name != "nt":
                terminated_target = root / "terminated-probe-target"
                result, calls = guarded(terminated_target, args=("deny", "--version"), tool_probe=True, FAKE_CARGO_TERMINATE="1")
                self.assertEqual(result.returncode, 86)
                self.assertEqual(calls, [["deny", "--version"]])
                self.assertFalse(json.loads((terminated_target / ".tze-hud-source-binding.json").read_text())["valid"])

            frozen_head = git("rev-parse", "HEAD")
            other_source = root / "other-source"
            git("worktree", "add", "-q", "--detach", str(other_source), frozen_head)
            result, calls = guarded(cwd=other_source)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])
            commit("README.md", "new frozen HEAD\n")
            result, calls = guarded()
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])
            git("reset", "--hard", frozen_head)  # Only this parent's owned fixture.

            # Built-in EOL conversion preserves Git content without changing the
            # user's config. Raw materialized bytes still get their own binding.
            source = repo / "lib.rs"
            original = source.read_bytes()
            git("config", "core.autocrlf", "true")
            try:
                crlf = original.replace(b"\n", b"\r\n")
                source.write_bytes(crlf)
                # Manual conversion is canonically equal but does not establish
                # a clean checkout. Preserve that refusal before real checkout.
                result, calls = guarded(root / "eol-target")
                self.assertEqual(result.returncode, 86)
                self.assertEqual(calls, [])
                self.assertNotEqual(git("status", "--porcelain"), "")
                source.unlink()
                git("restore", "lib.rs")
                self.assertEqual(source.read_bytes(), crlf)
                self.assertEqual(git("status", "--porcelain"), "")
                result, calls = guarded(root / "eol-target")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, [["check", "--workspace"]])
            finally:
                git("config", "--unset", "core.autocrlf")
                source.unlink(missing_ok=True)
                git("restore", "lib.rs")
                self.assertEqual(source.read_bytes(), original)

            marker = target / ".tze-hud-source-binding.json"
            valid_marker = marker.read_bytes()
            marker.write_bytes(b"invalid binding")
            result, calls = guarded()
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])
            marker.write_bytes(valid_marker)
            if os.name != "nt":
                alias = root / "target-alias"
                alias.symlink_to(target, target_is_directory=True)
                result, calls = guarded(alias)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])

            for label, path, content, ignored_path in [
                ("external path dependency", "Cargo.toml", '[package]\nname = "fixture"\nversion = "0.1.0"\n[dependencies]\nforeign = { path = "../outside" }\n', None),
                ("ignored build input", ".gitignore", "src/ignored.rs\n", "src/ignored.rs"),
                ("custom Git filter", ".gitattributes", "*.rs filter=unsupported\n", None),
                ("configured environment", ".cargo/config.toml", '[env]\nFIXTURE = "unsupported"\n', None),
            ]:
                with self.subTest(input_closure=label):
                    (repo / path).parent.mkdir(exist_ok=True)
                    commit(path, content)
                    if ignored_path:
                        ignored = repo / ignored_path
                        ignored.parent.mkdir(exist_ok=True)
                        ignored.write_text("ignored source fixture\n")
                    try:
                        result, calls = guarded(root / (label.replace(" ", "-") + "-target"))
                        self.assertNotEqual(result.returncode, 0)
                        self.assertEqual(calls, [])
                    finally:
                        if ignored_path:
                            ignored.unlink()
                            ignored.parent.rmdir()
                        git("reset", "--hard", frozen_head)
                        if path.startswith(".cargo/") and (repo / ".cargo").exists():
                            (repo / ".cargo").rmdir()

            cargo_home = root / "fixture-cargo-home"
            cargo_home.mkdir()
            (cargo_home / "config.toml").write_text("[build]\njobs = 1\n")
            result, calls = guarded(root / "config-target", CARGO_HOME=str(cargo_home))
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])

            for flag in ("--assume-unchanged", "--skip-worktree"):
                with self.subTest(hidden_index_edit=flag):
                    source = repo / "lib.rs"
                    original, before = source.read_bytes(), source.stat()
                    git("update-index", flag, "lib.rs")
                    try:
                        source.write_bytes(b"hidden source change\n")
                        os.utime(source, ns=(before.st_atime_ns, before.st_mtime_ns))
                        self.assertEqual(git("status", "--porcelain"), "")
                        result, calls = guarded()
                        self.assertNotEqual(result.returncode, 0)
                        self.assertEqual(calls, [])
                    finally:
                        git("update-index", flag.replace("--", "--no-", 1), "lib.rs")
                        source.write_bytes(original)

            for label, path, args, controls in [
                ("dirty", "lib.rs", ("check",), {}),
                ("untracked", "untracked.rs", ("check",), {}),
                ("invalid opt-in", None, ("check",), {"TZE_HUD_SHARED_TARGET_GUARD": "invalid"}),
                ("target escape", None, ("check", "--target-dir", str(root / "other-target")), {}),
                ("external config argument", None, ("check", "--config", "external.toml"), {}),
                ("untracked manifest argument", None, ("check", "--manifest-path", ".handoff/Cargo.toml"), {}),
                ("source-mutating formatter", None, ("fmt",), {}),
                ("compiler override", None, ("check",), {"RUSTC_WRAPPER": "unsupported-fixture"}),
                ("target runner", None, ("test",), {"CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER": "unsupported-fixture"}),
            ]:
                with self.subTest(shared_refusal=label):
                    original = (repo / path).read_bytes() if path and (repo / path).exists() else None
                    if path:
                        (repo / path).write_text("fixture change\n")
                    try:
                        result, calls = guarded(args=args, **controls)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertEqual(calls, [])
                    finally:
                        if path and original is None:
                            (repo / path).unlink()
                        elif path:
                            (repo / path).write_bytes(original)

            legacy = root / "unknown-target"
            legacy.mkdir()
            (legacy / "artifact").write_bytes(b"preserve unknown artifact")
            before = {p.name: p.read_bytes() for p in legacy.iterdir()}
            result, calls = guarded(legacy)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])
            self.assertEqual({p.name: p.read_bytes() for p in legacy.iterdir()}, before)
            result, calls = guarded(legacy, args=("deny", "--version"), tool_probe=True)
            self.assertEqual(result.returncode, 86)
            self.assertEqual(calls, [])
            self.assertEqual({p.name: p.read_bytes() for p in legacy.iterdir()}, before)

            with (target / ".tze-hud-source-binding.lock").open("r+b") as lock:
                if os.name == "nt":
                    import msvcrt
                    msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
                else:
                    import fcntl
                    fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                try:
                    result, calls = guarded()
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(calls, [])
                finally:
                    if os.name == "nt":
                        lock.seek(0)
                        msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
                    else:
                        fcntl.flock(lock.fileno(), fcntl.LOCK_UN)

            # The ordinary selector preserves the caller's target/opt-in. Its
            # fake just descendant executes the actual guard before fake Cargo.
            cargo_ledger.unlink(missing_ok=True)
            result, calls = invoke(
                update(locked), **guard_env, CARGO_TARGET_DIR=str(target), FAKE_SHARED_GUARD="1",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, [["pre-push-check"]])
            self.assertEqual(json.loads(cargo_ledger.read_text()), ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])

            result, calls = guarded(target, FAKE_SOURCE_CHANGE="1", FAKE_CARGO_EXIT="37")
            self.assertEqual(result.returncode, 37, result.stderr)
            self.assertEqual(calls, [["check", "--workspace"]])
            self.assertFalse(json.loads((target / ".tze-hud-source-binding.json").read_text())["valid"])
            git("restore", "lib.rs")
            result, calls = guarded()
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])

            # Git rename detection must not hide removal of a sensitive path.
            # Each proposed HEAD differs only by this exact-content R100 rename.
            for label, sensitive in [
                ("Rust source", "module.rs"),
                ("TOML", "settings.toml"),
                ("lockfile", "Cargo.lock"),
            ]:
                git("checkout", "-qB", "rename-sensitive", base)
                baseline = commit(sensitive, "exact rename fixture\n")
                git("update-ref", "refs/remotes/origin/main", baseline)
                git("mv", sensitive, "notes.md")
                git("commit", "-qm", "rename sensitive path to documentation")
                proposed = git("rev-parse", "HEAD")
                with self.subTest(rename_sensitive_path=sensitive):
                    rename = git("diff", "--name-status", baseline, proposed, "--")
                    self.assertEqual(rename, f"R100\t{sensitive}\tnotes.md")
                    result, calls = invoke(update(proposed))
                    print(json.dumps({
                        "rename_case": label, "sensitive_path": sensitive,
                        "baseline": baseline, "proposed_HEAD": proposed,
                        "actual_name_status": rename,
                        "selector_exit": result.returncode, "checker_calls": calls,
                    }))
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(calls, [["pre-push-check"]])


if __name__ == "__main__":
    unittest.main()
