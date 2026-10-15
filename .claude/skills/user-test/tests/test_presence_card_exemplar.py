from __future__ import annotations

import asyncio
import contextlib
import io
import json
import sys
import tempfile
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

SCRIPT_DIR = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPT_DIR))

import presence_card_exemplar as exemplar  # noqa: E402
from hud_grpc_client import HudClient  # noqa: E402
from proto_gen import session_pb2  # noqa: E402


class FakePairedClient(HudClient):
    """In-memory paired stream; real claim/card/update helpers build the wire."""

    instances: list[FakePairedClient] = []
    fail_phase = ""
    key = "private-fixture-key"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.index = len(self.instances)
        self.instances.append(self)
        self.closed = False
        self.dropped = False
        self.released = []
        self.release_attempts = []
        self.mutations = []
        self.heartbeats = 0
        self.tile_id = uuid.UUID(int=1000 + self.index).bytes

    async def connect(self):
        if self.agent_id not in ("", "paired-agent") or (self.fail_phase == "auth" and self.index == 1):
            raise RuntimeError(f"AUTH_FAILED: identity mismatch {self.key}")
        self.namespace = "paired-agent"
        self.heartbeat_interval_ms = 5000
        self.scene_display_area = (2560.0, 1440.0)

    async def upload_avatar_png(self, png):
        return b"a" * 32

    async def _request(self, **payload):
        self.claim = payload["claim_tile"]
        return session_pb2.RequestResult(
            ok=True, lease_id=uuid.UUID(int=2000 + self.index).bytes,
            ids=[self.tile_id, *[uuid.UUID(int=10000 + self.index * 100 + n).bytes for n in range(13)]],
            ttl_ms=120000,
        )

    async def submit_mutation_batch(self, lease_id, mutations, **kwargs):
        self.mutations.extend(mutations)
        if mutations[0].WhichOneof("mutation") == "update_node_content":
            if self.fail_phase == "update":
                raise RuntimeError(f"NOT_ALLOWED: rejected body update {self.key}")
            if self.fail_phase == "interrupt":
                raise asyncio.CancelledError
            assert kwargs["retries"] == 0
        return session_pb2.RequestResult(ok=True, code="", hint="")

    async def send_heartbeat(self):
        self.heartbeats += 1

    async def wait_for_click(self, *args, **kwargs):
        await asyncio.Event().wait()

    async def release_tile(self, tile_id):
        self.release_attempts.append(tile_id)
        if self.fail_phase == "clear_denied":
            raise RuntimeError(f"NOT_ALLOWED: scoped Clear denied {self.key}")
        if self.fail_phase == "clear_timeout":
            raise TimeoutError("scoped Clear acknowledgement timed out")
        self.released.append(tile_id)

    async def close(self, **kwargs):
        self.closed = True

    async def drop_connection(self):
        self.dropped = True


class PresenceCardExemplarTests(unittest.IsolatedAsyncioTestCase):
    def test_format_last_active_humanizes_elapsed_time(self) -> None:
        for seconds, label in ((0, "now"), (29, "29s ago"), (30, "30s ago"), (60, "1m ago"), (125, "2m ago")):
            with self.subTest(seconds=seconds):
                self.assertEqual(exemplar.format_last_active(seconds), label)

    async def run_fixture(self, *, agent="", fail_phase=""):
        FakePairedClient.instances = []
        FakePairedClient.fail_phase = fail_phase
        async def no_wait(delay):
            # Exercise survivor liveness after the hard drop without real waits.
            for client in FakePairedClient.instances:
                if not client.closed and not client.dropped:
                    await client.send_heartbeat()
        async def heartbeat(client, interval):
            await client.send_heartbeat()
            await asyncio.Event().wait()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "receipt.json"
            args = exemplar.parse_args(["--host", "paired.invalid", "--agent", agent,
                                        "--transcript-out", str(output)])
            stdout = io.StringIO()
            with patch.object(exemplar, "HudClient", FakePairedClient), \
                    patch.object(exemplar.hud_env, "load_psk", return_value=FakePairedClient.key), \
                    patch.object(exemplar.asyncio, "sleep", side_effect=no_wait), \
                    patch.object(exemplar, "heartbeat_loop", side_effect=heartbeat), \
                    contextlib.redirect_stdout(stdout):
                code = await exemplar.run_scenario(args)
            raw = output.read_text()
            self.assertNotIn(FakePairedClient.key, raw + stdout.getvalue())
            return code, json.loads(raw), FakePairedClient.instances

    async def test_build_step_plan_tracks_create_update_disconnect_cleanup(self) -> None:
        code, receipt, clients = await self.run_fixture()
        self.assertEqual(code, 0)
        self.assertEqual(len(clients), 3)
        self.assertEqual([client.agent_id for client in clients], [""] * 3)
        self.assertTrue(all(client.namespace == "paired-agent" for client in clients))
        self.assertTrue(clients[-1].dropped)
        self.assertFalse(clients[-1].closed)
        self.assertEqual(clients[-1].released, [])
        for client in clients[:-1]:
            self.assertTrue(client.closed)
            self.assertEqual(client.released, [client.tile_id])
            self.assertGreater(client.heartbeats, 0)
        acknowledged = [step for step in receipt["steps"] if step["code"] == "create" and step["status"] == "acknowledged"]
        self.assertEqual(len(acknowledged), 3)
        self.assertEqual(acknowledged[0]["display_area"], [2560.0, 1440.0])
        for client, step in zip(clients, acknowledged, strict=True):
            mutations = [m.update_node_content for m in client.mutations if m.WhichOneof("mutation") == "update_node_content"]
            self.assertEqual(mutations[0].node_id, uuid.UUID(step["node_ids"]["body"]).bytes)
            self.assertIn("30s ago", mutations[0].text_markdown.content)
        self.assertTrue(all(step["visual_observation"] == "unverified" for step in receipt["steps"]))
        final = next(step for step in receipt["steps"] if step["status"] == "protocol_complete")
        self.assertEqual(final["orphan_reclaim_observation"], "unverified")

    async def test_paired_assertion_and_partial_setup_failure(self) -> None:
        for agent, fail_phase, expected_count in (("paired-agent", "", 3), ("wrong-agent", "", 1), ("", "auth", 2)):
            with self.subTest(agent=agent, fail_phase=fail_phase):
                code, receipt, clients = await self.run_fixture(agent=agent, fail_phase=fail_phase)
                self.assertEqual(len(clients), expected_count)
                self.assertEqual(code, 0 if not fail_phase and agent == "paired-agent" else 1)
                if code:
                    self.assertTrue(all(client.closed for client in clients))
                    self.assertFalse(any(step["status"] == "protocol_complete" for step in receipt["steps"]))
                    failed = next(step for step in receipt["steps"] if step["status"] == "failed")
                    self.assertIn("AUTH_FAILED", failed["detail"])
                    self.assertFalse(any(m.WhichOneof("mutation") == "update_node_content" for c in clients for m in c.mutations))

    async def test_rejected_update_and_interrupt_leave_failed_receipt_and_cleanup(self) -> None:
        for failure, exit_code in (("update", 1), ("interrupt", 130), ("clear_denied", 1), ("clear_timeout", 1)):
            with self.subTest(failure=failure):
                code, receipt, clients = await self.run_fixture(fail_phase=failure)
                self.assertEqual(code, exit_code)
                if failure.startswith("clear_"):
                    self.assertTrue(all(client.closed for client in clients[:-1]))
                    self.assertTrue(all(client.release_attempts == [client.tile_id] for client in clients[:-1]))
                    self.assertTrue(all(client.released == [] for client in clients))
                    self.assertTrue(clients[-1].dropped)
                    self.assertEqual(clients[-1].release_attempts, [])
                    expected_failures = ["cleanup", "cleanup"]
                else:
                    self.assertTrue(all(client.closed for client in clients))
                    self.assertTrue(all(client.released == [client.tile_id] for client in clients))
                    expected_failures = ["update"]
                self.assertEqual([step["code"] for step in receipt["steps"] if step["status"] == "failed"], expected_failures)
                self.assertEqual(receipt["steps"][-1]["status"], "failed")
                self.assertFalse(any(step["status"] == "protocol_complete" for step in receipt["steps"]))


if __name__ == "__main__":
    unittest.main()
