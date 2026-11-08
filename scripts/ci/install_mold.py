#!/usr/bin/env python3
"""Install the byte-pinned Linux CI linker through GitHub's asset API.

The release-download website stalled before returning headers on hosted
runners. The asset API avoids that hop; a fixed digest rejects changed bytes
before extraction or execution. This does not replace the system linker.
"""
from __future__ import annotations

import hashlib
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

VERSION = "2.42.1"
ASSET_ID = 557036001
SHA256 = "6ff270c9bf07d2bec5c98aa324eb7c4daf6a1a4d815c05ff1708049616047855"


def install_mold(runner_temp: Path, github_path: Path, *, expected_sha256: str = SHA256) -> Path:
    """Verify, install and register mold; the digest argument supports local fixtures.

    The CLI always uses the production pin. A failed install leaves neither
    its extraction directory nor a PATH registration behind.
    """
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise RuntimeError("mold CI installation requires Linux x86_64")
    if not os.environ.get("GH_TOKEN"):
        raise RuntimeError("GH_TOKEN is required")
    if not runner_temp.is_dir() or not github_path.is_file():
        raise RuntimeError("RUNNER_TEMP must be a directory and GITHUB_PATH an existing file")
    work = Path(tempfile.mkdtemp(prefix=f"mold-{VERSION}-", dir=runner_temp))
    archive = work / "download.tar.gz"
    try:
        with archive.open("wb") as output:
            subprocess.run(
                ["gh", "api", "--method", "GET", f"repos/rui314/mold/releases/assets/{ASSET_ID}",
                 "-H", "Accept: application/octet-stream", "-H", "X-GitHub-Api-Version: 2022-11-28"],
                stdout=output, stderr=subprocess.PIPE, check=True, timeout=45,
            )
        if hashlib.sha256(archive.read_bytes()).hexdigest() != expected_sha256:
            raise RuntimeError("mold archive SHA256 does not match the pinned digest")
        with tarfile.open(archive, "r:gz") as source:
            source.extractall(work, filter="data")
        bin_dir = work / f"mold-{VERSION}-x86_64-linux" / "bin"
        result = subprocess.run([str(bin_dir / "mold"), "--version"],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, check=True, timeout=10)
        if not re.match(rf"^mold {re.escape(VERSION)}(?:\s|$)", result.stdout):
            raise RuntimeError("installed mold did not report the pinned version")
        with github_path.open("a", encoding="utf-8") as path_file:
            path_file.write(f"{bin_dir}\n")
        print(f"Installed mold {VERSION} from verified release asset {ASSET_ID}")
        return bin_dir
    except BaseException:
        shutil.rmtree(work)
        raise
    finally:
        archive.unlink(missing_ok=True)


def main() -> int:
    try:
        runner_temp, github_path = (os.environ.get(key) for key in ("RUNNER_TEMP", "GITHUB_PATH"))
        if not runner_temp or not github_path:
            raise RuntimeError("RUNNER_TEMP and GITHUB_PATH are required")
        install_mold(Path(runner_temp), Path(github_path))
    except (OSError, RuntimeError, subprocess.SubprocessError, tarfile.TarError) as error:
        # Subprocess exception strings contain only public argv, never captured
        # stderr or the inherited token. Do not dump process environment.
        print(f"mold installation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
