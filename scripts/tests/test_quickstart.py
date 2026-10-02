#!/usr/bin/env python3
"""Regression tests for quickstart MCP client-config emission."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import shlex
import stat
import subprocess
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
QUICKSTART = REPO_ROOT / "scripts" / "quickstart.sh"


class QuickstartMcpConfigTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir_obj = tempfile.TemporaryDirectory(prefix="quickstart-mcp-config-")
        self.temp_dir = Path(self.temp_dir_obj.name)

    def tearDown(self) -> None:
        self.temp_dir_obj.cleanup()

    def run_quickstart(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                "bash",
                str(QUICKSTART),
                "--config",
                str(self.temp_dir / "tze_hud.toml"),
                "--psk-file",
                str(self.temp_dir / "tze_hud.psk"),
                "--bin",
                "/bin/true",
                *args,
            ],
            cwd=self.temp_dir,
            capture_output=True,
            text=True,
            check=False,
        )

    @staticmethod
    def emitted_json(stdout: str) -> dict[str, object]:
        return json.loads(stdout)

    def test_bare_option_emits_wired_ready_to_merge_json(self) -> None:
        secret = "operator-secret"

        result = self.run_quickstart(
            "--psk",
            secret,
            "--host",
            "hud.test",
            "--mcp-port",
            "9191",
            "--emit-mcp-config",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        config = self.emitted_json(result.stdout)
        server = config["mcpServers"]["tze-hud-runtime"]
        self.assertEqual(server["type"], "url")
        self.assertEqual(server["url"], "http://hud.test:9191/mcp")
        self.assertEqual(
            server["headers"]["Authorization"], f"Bearer {secret}"
        )

    def test_path_option_writes_mode_0600_without_printing_secret(self) -> None:
        secret = "file-only-secret"
        output_path = self.temp_dir / "tze-hud.mcp.json"

        result = self.run_quickstart(
            "--psk",
            secret,
            "--bin",
            str(self.temp_dir / "missing-tze-hud"),
            "--print-attach-info",
            f"--emit-mcp-config={output_path}",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(secret, result.stdout)
        self.assertNotIn(secret, result.stderr)
        config = json.loads(output_path.read_text(encoding="utf-8"))
        authorization = config["mcpServers"]["tze-hud-runtime"]["headers"][
            "Authorization"
        ]
        self.assertEqual(authorization, f"Bearer {secret}")
        mode = stat.S_IMODE(output_path.stat().st_mode)
        self.assertEqual(mode, 0o600)

    def test_path_option_is_emission_only_and_needs_no_binary(self) -> None:
        output_path = self.temp_dir / "headless.mcp.json"

        result = self.run_quickstart(
            "--psk",
            "headless-file-secret",
            "--bin",
            str(self.temp_dir / "missing-tze-hud"),
            f"--emit-mcp-config={output_path}",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(output_path.is_file())
        self.assertNotIn("No usable tze_hud binary", result.stderr)

    def test_redacted_headless_mode_rejects_bare_secret_emission(self) -> None:
        secret = "must-not-be-printed"

        result = self.run_quickstart(
            "--psk",
            secret,
            "--print-attach-info",
            "--emit-mcp-config",
        )

        self.assertEqual(result.returncode, 1)
        self.assertNotIn(secret, result.stdout)
        self.assertNotIn(secret, result.stderr)
        self.assertIn("--emit-mcp-config=<path>", result.stderr)

    def test_emitted_json_escapes_operator_provided_psk(self) -> None:
        secret = 'quote" slash\\ unicode ☃ controls\b\f\n\r\t\x01 remain valid'

        result = self.run_quickstart(
            "--psk",
            secret,
            "--emit-mcp-config",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        config = self.emitted_json(result.stdout)
        authorization = config["mcpServers"]["tze-hud-runtime"]["headers"][
            "Authorization"
        ]
        self.assertEqual(authorization, f"Bearer {secret}")
        self.assertNotIn(secret, result.stderr)

    def test_emit_rejects_disabled_or_invalid_mcp_ports(self) -> None:
        for port in ("0", "not-a-port", "65536"):
            with self.subTest(port=port):
                result = self.run_quickstart(
                    "--psk",
                    "port-validation-secret",
                    "--mcp-port",
                    port,
                    "--emit-mcp-config",
                )

                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertNotIn("port-validation-secret", result.stderr)
                self.assertIn("--mcp-port must be an integer from 1 to 65535", result.stderr)

    def test_path_option_preserves_existing_client_config(self) -> None:
        output_path = self.temp_dir / "existing.json"
        original = '{"keep": true}\n'
        output_path.write_text(original, encoding="utf-8")

        result = self.run_quickstart(
            "--psk",
            "replacement-secret",
            f"--emit-mcp-config={output_path}",
        )

        self.assertEqual(result.returncode, 1)
        self.assertEqual(output_path.read_text(encoding="utf-8"), original)
        self.assertIn("refusing to overwrite", result.stderr)

    def test_path_option_refuses_existing_symlink_without_touching_target(self) -> None:
        target_path = self.temp_dir / "target.json"
        target_path.write_text('{"keep": "target"}\n', encoding="utf-8")
        output_path = self.temp_dir / "linked.json"
        output_path.symlink_to(target_path)

        result = self.run_quickstart(
            "--psk",
            "symlink-secret",
            f"--emit-mcp-config={output_path}",
        )

        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            target_path.read_text(encoding="utf-8"), '{"keep": "target"}\n'
        )
        self.assertTrue(output_path.is_symlink())
        self.assertNotIn("symlink-secret", result.stdout)
        self.assertNotIn("symlink-secret", result.stderr)

    def test_existing_print_attach_info_mode_stays_redacted_without_binary(self) -> None:
        secret = "existing-headless-secret"

        result = self.run_quickstart(
            "--psk",
            secret,
            "--bin",
            str(self.temp_dir / "missing-tze-hud"),
            "--print-attach-info",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ATTACH INFO", result.stdout)
        self.assertIn("Authorization: Bearer <your agent's PSK>", result.stdout)
        self.assertNotIn(secret, result.stdout)
        self.assertNotIn(secret, result.stderr)

    def test_launch_mode_forwards_runtime_args_and_pairs_only_the_psk_hash(self) -> None:
        args_path = self.temp_dir / "runtime.args"
        psk_path = self.temp_dir / "runtime.psk"
        fake_binary = self.temp_dir / "fake-tze-hud"
        fake_binary.write_text(
            "#!/usr/bin/env bash\n"
            f"printf '%s\\n' \"$@\" > {shlex.quote(str(args_path))}\n"
            f"printf '%s' \"${{TZE_HUD_PSK:-}}\" > {shlex.quote(str(psk_path))}\n",
            encoding="utf-8",
        )
        fake_binary.chmod(0o700)
        secret = "legacy-launch-secret"

        result = self.run_quickstart(
            "--psk",
            secret,
            "--bin",
            str(fake_binary),
            "--window-mode",
            "overlay",
            "--mcp-port",
            "9191",
            "--grpc-port",
            "5252",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            args_path.read_text(encoding="utf-8").splitlines(),
            [
                "--config",
                str(self.temp_dir / "tze_hud.toml"),
                "--window-mode",
                "overlay",
                "--mcp-port",
                "9191",
                "--grpc-port",
                "5252",
            ],
        )
        # The runtime gets no PSK; agents.toml beside the config pairs its hash.
        self.assertEqual(psk_path.read_text(encoding="utf-8"), "")
        agents = (self.temp_dir / "agents.toml").read_text(encoding="utf-8")
        self.assertIn(hashlib.sha256(secret.encode()).hexdigest(), agents)
        self.assertNotIn(secret, agents)
        self.assertNotIn(secret, result.stdout)
        self.assertNotIn(secret, result.stderr)

    def test_help_documents_both_emit_forms_without_scaffolding(self) -> None:
        result = subprocess.run(
            ["bash", str(QUICKSTART), "--help"],
            cwd=self.temp_dir,
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--emit-mcp-config[=path]", result.stdout)
        self.assertIn("bare form writes JSON to stdout", result.stdout)
        self.assertIn("existing MCP config is never overwritten", result.stdout)
        self.assertEqual(result.stderr, "")
        self.assertEqual(list(self.temp_dir.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
