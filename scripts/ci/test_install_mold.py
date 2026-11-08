"""Real archive/hash/install behavior of the pinned CI linker bootstrap."""
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import os
import platform
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

_spec = importlib.util.spec_from_file_location("install_mold", Path(__file__).with_name("install_mold.py"))
installer = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(installer)


@unittest.skipUnless(platform.system() == "Linux" and platform.machine() == "x86_64", "Linux CI linker installer")
class MoldInstall(unittest.TestCase):
    def test_verified_install_and_failures_leave_only_authorized_artifacts(self):
        # Fake only the external download boundary. Real SHA256, tar safety,
        # executable version validation and PATH registration run in every case.
        for case in ("download-fails", "wrong-hash", "wrong-version", "unsafe-archive", "verified"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                runner_temp = root / "runner"
                runner_temp.mkdir()
                github_path = root / "github-path"
                github_path.write_text("already-present\n")
                fake_bin = root / "fake-bin"
                fake_bin.mkdir()
                gh = fake_bin / "gh"
                gh.write_text("#!/usr/bin/env python3\nimport os,sys\nfrom pathlib import Path\n"
                              "sys.stdout.buffer.write(Path(os.environ['FIXTURE_ARCHIVE']).read_bytes())\n"
                              "sys.exit(7 if os.environ.get('FIXTURE_DOWNLOAD_FAIL') else 0)\n")
                gh.chmod(0o755)
                version = "2.42.10" if case == "wrong-version" else installer.VERSION
                binary = ('#!/bin/sh\n[ "$#" = 1 ] && [ "$1" = --version ] || exit 11\n'
                          'printf "executed\\n" >> "$FIXTURE_EXECUTED"\n'
                          f'printf "mold {version} (fixture)\\n"\n').encode()
                fixture = root / "fixture.tar.gz"
                with tarfile.open(fixture, "w:gz") as archive:
                    member = tarfile.TarInfo(f"mold-{installer.VERSION}-x86_64-linux/bin/mold")
                    member.mode, member.size = 0o755, len(binary)
                    archive.addfile(member, io.BytesIO(binary))
                    if case in {"wrong-hash", "unsafe-archive"}:
                        member = tarfile.TarInfo("../escaped")
                        member.size = 1
                        archive.addfile(member, io.BytesIO(b"x"))
                expected = "0" * 64 if case == "wrong-hash" else hashlib.sha256(fixture.read_bytes()).hexdigest()
                executed = root / "executed"
                environment = {"PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                               "GH_TOKEN": "synthetic-fixture-token", "FIXTURE_ARCHIVE": str(fixture),
                               "FIXTURE_EXECUTED": str(executed),
                               "FIXTURE_DOWNLOAD_FAIL": "yes" if case == "download-fails" else ""}
                with mock.patch.dict(os.environ, environment), contextlib.redirect_stdout(io.StringIO()):
                    if case == "verified":
                        bin_dir = installer.install_mold(runner_temp, github_path, expected_sha256=expected)
                        self.assertTrue((bin_dir / "mold").is_file())
                        self.assertEqual(github_path.read_text(), f"already-present\n{bin_dir}\n")
                        self.assertEqual(executed.read_text(), "executed\n")
                        self.assertEqual(list(runner_temp.rglob("*.tar.gz")), [])
                    else:
                        failure = {
                            "download-fails": installer.subprocess.CalledProcessError,
                            "wrong-hash": RuntimeError,
                            "wrong-version": RuntimeError,
                            "unsafe-archive": tarfile.FilterError,
                        }[case]
                        # The wrong-hash archive also contains an unsafe member:
                        # hash rejection must precede tar's FilterError.
                        with self.assertRaises(failure) as raised:
                            installer.install_mold(runner_temp, github_path, expected_sha256=expected)
                        if case == "download-fails":
                            self.assertEqual(raised.exception.returncode, 7)
                        self.assertEqual(github_path.read_text(), "already-present\n")
                        self.assertEqual(list(runner_temp.iterdir()), [])
                        self.assertFalse((runner_temp / "escaped").exists())
                        self.assertEqual(executed.exists(), case == "wrong-version")


if __name__ == "__main__":
    unittest.main()
