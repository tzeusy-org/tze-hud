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


class PrePushTests(unittest.TestCase):
    def test_ref_selection_and_checker_failure(self) -> None:
        # Stay in the user's home and isolate Git from personal hooks/config.
        with tempfile.TemporaryDirectory(prefix="pre-push-", dir=Path.home()) as temporary:
            root = Path(temporary)
            repo = root / "repo"
            repo.mkdir()
            bin_dir = root / "bin"
            bin_dir.mkdir()
            ledger = root / "checks.jsonl"
            env = {
                name: value for name, value in os.environ.items()
                if not name.startswith("GIT_")
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
                "import json, os, pathlib, sys\n"
                "with open(os.environ['FAKE_PRE_PUSH_LEDGER'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "if os.environ.get('FAKE_PRE_PUSH_DIRTY'):\n"
                "    pathlib.Path('README.md').write_text('changed during checks\\n')\n"
                "sys.exit(int(os.environ.get('FAKE_PRE_PUSH_EXIT', '0')))\n"
            )
            if os.name == "nt":
                (bin_dir / "just.cmd").write_text(
                    f'@"{sys.executable}" "{fake}" %*\n'
                )
            else:
                executable = bin_dir / "just"
                executable.write_text("#!/usr/bin/env python3\n" + fake.read_text())
                executable.chmod(0o755)

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


if __name__ == "__main__":
    unittest.main()
