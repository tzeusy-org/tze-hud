#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "blake3>=1.0.0",
#   "grpcio>=1.80.0",
#   "pillow>=11.0.0",
#   "protobuf>=6.31.1",
# ]
# ///
import asyncio
import contextlib
import io
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

try:
    from PIL import Image
except ModuleNotFoundError:  # pragma: no cover - environment dependent
    Image = None

from hud_grpc_client import (
    ClaimedTile,
    HudClient,
    _blake3_digest_bytes,
    _resource_id_bytes,
    avatar_resource_id_from_png,
    build_presence_card_accent_node,
    build_presence_card_avatar_plate_node,
    build_presence_card_root_node,
    build_presence_card_avatar_node,
    build_presence_card_chip_bg_node,
    build_presence_card_chip_text_node,
    build_presence_card_dismiss_bg_node,
    build_presence_card_dismiss_hit_region_node,
    build_presence_card_dismiss_text_node,
    build_presence_card_eyebrow_node,
    build_presence_card_name_node,
    build_presence_card_sheen_node,
    build_presence_card_text_node,
    tile_surface,
    make_avatar_png,
)
from proto_gen import session_pb2, types_pb2


class HudGrpcClientTests(unittest.IsolatedAsyncioTestCase):
    @unittest.skipIf(Image is None, "Pillow is required for PNG avatar tests")
    def test_make_avatar_png_is_32_by_32_png(self):
        png = make_avatar_png((66, 133, 244))
        with Image.open(io.BytesIO(png)) as img:
            self.assertEqual(img.size, (32, 32))
            self.assertEqual(img.format, "PNG")

    @unittest.skipIf(Image is None, "Pillow is required for PNG avatar tests")
    def test_avatar_resource_id_is_32_bytes_and_deterministic(self):
        png = make_avatar_png((52, 168, 83))
        rid1 = avatar_resource_id_from_png(png)
        rid2 = avatar_resource_id_from_png(png)
        self.assertEqual(len(rid1), 32)
        self.assertEqual(rid1, rid2)

    def _assert_blake3_import_failure_hard_errors(self, raised_exc):
        """Force ``import blake3`` to fail with ``raised_exc`` via a meta-path
        finder and assert ``_blake3_digest_bytes`` converts it into an actionable
        ``RuntimeError`` (chained). Guards the contract even when the wheel is
        installed (as it is in CI). Restores the module cache on exit so the
        block never leaks into other tests."""
        import sys

        class _BlockBlake3:
            def find_spec(self, name, path=None, target=None):
                if name == "blake3" or name.startswith("blake3."):
                    raise raised_exc
                return None

        saved = sys.modules.pop("blake3", None)
        finder = _BlockBlake3()
        sys.meta_path.insert(0, finder)
        try:
            with self.assertRaises(RuntimeError) as ctx:
                _blake3_digest_bytes(b"payload")
            self.assertIn("pip install blake3", str(ctx.exception))
            self.assertIs(ctx.exception.__cause__, raised_exc)
        finally:
            sys.meta_path.remove(finder)
            if saved is not None:
                sys.modules["blake3"] = saved

    def test_blake3_digest_hard_errors_without_wheel(self):
        """A missing ``blake3`` wheel (``ModuleNotFoundError``) raises a hard,
        actionable error rather than silently shelling out to an on-demand cargo
        compile (hud-6vrwq)."""
        self._assert_blake3_import_failure_hard_errors(
            ModuleNotFoundError("blocked for test: blake3", name="blake3")
        )

    def test_blake3_digest_hard_errors_on_broken_wheel(self):
        """A present-but-broken ``blake3`` wheel raises a bare ``ImportError``
        (binary/ABI mismatch, missing shared lib, Windows DLL load failure) —
        which is NOT a ``ModuleNotFoundError``. The digest helper must still
        surface the actionable RuntimeError, so it catches the broad
        ``ImportError`` (hud-6vrwq review follow-up)."""
        self._assert_blake3_import_failure_hard_errors(
            ImportError("blocked for test: broken blake3 extension", name="blake3")
        )

    def test_presence_card_node_builders_match_spec(self):
        resource_id = b"\x11" * 32
        root = build_presence_card_root_node()
        sheen = build_presence_card_sheen_node()
        accent = build_presence_card_accent_node((66 / 255.0, 133 / 255.0, 244 / 255.0, 1.0))
        plate = build_presence_card_avatar_plate_node((66 / 255.0, 133 / 255.0, 244 / 255.0, 1.0))
        avatar = build_presence_card_avatar_node(resource_id)
        eyebrow = build_presence_card_eyebrow_node()
        name = build_presence_card_name_node("agent-alpha")
        text = build_presence_card_text_node("agent-alpha")
        chip_bg = build_presence_card_chip_bg_node()
        chip_text = build_presence_card_chip_text_node("now")
        dismiss_bg = build_presence_card_dismiss_bg_node()
        dismiss_text = build_presence_card_dismiss_text_node()
        dismiss_hit_region = build_presence_card_dismiss_hit_region_node()

        self.assertTrue(root.HasField("solid_color"))
        self.assertAlmostEqual(root.solid_color.color.r, 0.10, places=5)
        self.assertAlmostEqual(root.solid_color.color.a, 0.72, places=5)
        self.assertEqual(root.solid_color.bounds.width, 320.0)
        self.assertEqual(root.solid_color.bounds.height, 112.0)
        self.assertEqual(root.solid_color.radius, 12.0)

        self.assertTrue(sheen.HasField("solid_color"))
        self.assertEqual(sheen.solid_color.bounds.height, 2.0)
        self.assertTrue(accent.HasField("solid_color"))
        self.assertEqual(accent.solid_color.bounds.width, 4.0)
        self.assertTrue(plate.HasField("solid_color"))
        self.assertEqual(plate.solid_color.bounds.width, 56.0)
        self.assertEqual(plate.solid_color.bounds.height, 56.0)

        self.assertTrue(avatar.HasField("static_image"))
        self.assertEqual(avatar.static_image.resource_id, resource_id)
        self.assertEqual(avatar.static_image.width, 32)
        self.assertEqual(avatar.static_image.height, 32)
        self.assertEqual(
            avatar.static_image.fit_mode,
            types_pb2.IMAGE_FIT_MODE_COVER,
        )
        self.assertEqual(avatar.static_image.bounds.x, 34.0)
        self.assertEqual(avatar.static_image.bounds.width, 36.0)

        self.assertTrue(eyebrow.HasField("text_markdown"))
        self.assertEqual(eyebrow.text_markdown.content, "RESIDENT AGENT")
        self.assertEqual(eyebrow.text_markdown.font_size_px, 11.0)

        self.assertTrue(name.HasField("text_markdown"))
        self.assertEqual(name.text_markdown.content, "**agent-alpha**")
        self.assertEqual(name.text_markdown.font_size_px, 20.0)

        self.assertTrue(text.HasField("text_markdown"))
        self.assertEqual(text.text_markdown.content, "Connected • last active now")
        self.assertEqual(text.text_markdown.font_size_px, 13.0)

        self.assertTrue(chip_bg.HasField("solid_color"))
        self.assertEqual(chip_bg.solid_color.bounds.width, 44.0)

        self.assertTrue(chip_text.HasField("text_markdown"))
        self.assertEqual(chip_text.text_markdown.content, "NOW")
        self.assertEqual(chip_text.text_markdown.font_size_px, 10.0)

        self.assertTrue(dismiss_bg.HasField("solid_color"))
        self.assertEqual(dismiss_bg.solid_color.bounds.width, 24.0)
        self.assertEqual(dismiss_bg.solid_color.bounds.height, 24.0)
        self.assertEqual(dismiss_bg.solid_color.radius, 8.0)

        self.assertTrue(dismiss_text.HasField("text_markdown"))
        self.assertEqual(dismiss_text.text_markdown.content, "X")

        self.assertTrue(dismiss_hit_region.HasField("hit_region"))
        self.assertEqual(dismiss_hit_region.hit_region.interaction_id, "dismiss-card")
        self.assertTrue(dismiss_hit_region.hit_region.accepts_pointer)

    def test_resource_id_bytes_rejects_invalid_proto_length(self):
        rid = types_pb2.ResourceIdProto(bytes=b"\x01" * 31)
        with self.assertRaises(ValueError):
            _resource_id_bytes(rid)

    @unittest.skipIf(Image is None, "Pillow is required for PNG avatar tests")
    async def test_upload_avatar_png_sends_resource_upload_start_and_returns_resource_id(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        client._send = AsyncMock(return_value=17)
        expected_resource_id = b"\x99" * 32
        client._await_resource_upload_result = AsyncMock(
            return_value=session_pb2.ResourceStored(
                request_sequence=17,
                resource_id=types_pb2.ResourceIdProto(bytes=expected_resource_id),
            )
        )

        avatar_png = make_avatar_png((66, 133, 244))
        resource_id = await client.upload_avatar_png(avatar_png)

        self.assertEqual(resource_id, expected_resource_id)
        client._await_resource_upload_result.assert_awaited_once_with(
            request_sequence=17,
            timeout=10.0,
        )
        sent_start = client._send.await_args.kwargs["resource_upload_start"]
        self.assertEqual(sent_start.resource_type, session_pb2.IMAGE_PNG)
        self.assertEqual(sent_start.total_size_bytes, len(avatar_png))
        self.assertEqual(sent_start.inline_data, avatar_png)
        self.assertEqual(sent_start.metadata.width, 32)
        self.assertEqual(sent_start.metadata.height, 32)
        self.assertEqual(len(sent_start.expected_hash), 32)

    async def test_await_resource_upload_result_raises_resource_error(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        client._response_queue = asyncio.Queue()
        await client._response_queue.put(
            session_pb2.ServerMessage(
                resource_error_response=session_pb2.ResourceErrorResponse(
                    request_sequence=23,
                    error_code=session_pb2.RESOURCE_HASH_MISMATCH,
                    message="hash mismatch",
                    context="expected hash deadbeef",
                    hint="recompute hash",
                )
            )
        )

        with self.assertRaisesRegex(RuntimeError, "RESOURCE_HASH_MISMATCH"):
            await client._await_resource_upload_result(
                request_sequence=23,
                timeout=0.1,
            )

    async def test_wait_for_does_not_drop_unmatched_messages(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        client._response_queue = asyncio.Queue()
        result = session_pb2.ServerMessage(
            request_result=session_pb2.RequestResult(
                batch_id=b"\x01" * 16,
                ok=True,
            )
        )
        reclaimed = session_pb2.ServerMessage(
            reclaimed=session_pb2.Reclaimed(
                surface="tile:00000000-0000-0000-0000-000000000002",
                why=session_pb2.RECLAIM_REASON_EXPIRED,
            )
        )
        await client._response_queue.put(result)
        await client._response_queue.put(reclaimed)

        reclaimed_resp = await client._wait_for("reclaimed", timeout=0.1)
        self.assertEqual(reclaimed_resp.reclaimed.why, session_pb2.RECLAIM_REASON_EXPIRED)
        result_resp = await client._wait_for("request_result", timeout=0.1)
        self.assertEqual(result_resp.request_result.batch_id, b"\x01" * 16)

    async def test_wait_for_matcher_does_not_replay_wrong_deferred_payload(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        client._response_queue = asyncio.Queue()
        for marker in (b"\x41", b"\x42"):
            await client._response_queue.put(
                session_pb2.ServerMessage(
                    request_result=session_pb2.RequestResult(batch_id=marker * 16)
                )
            )

        resp = await client._wait_for(
            "request_result",
            timeout=0.1,
            matcher=lambda msg: msg.request_result.batch_id == b"\x42" * 16,
        )

        self.assertEqual(resp.request_result.batch_id, b"\x42" * 16)
        deferred_resp = await client._wait_for("request_result", timeout=0.1)
        self.assertEqual(deferred_resp.request_result.batch_id, b"\x41" * 16)

    async def test_await_resource_upload_result_does_not_drop_other_responses(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        client._response_queue = asyncio.Queue()
        await client._response_queue.put(
            session_pb2.ServerMessage(
                request_result=session_pb2.RequestResult(
                    batch_id=b"\x11" * 16,
                    ok=True,
                )
            )
        )
        await client._response_queue.put(
            session_pb2.ServerMessage(
                resource_stored=session_pb2.ResourceStored(
                    request_sequence=5,
                    resource_id=types_pb2.ResourceIdProto(bytes=b"\x22" * 32),
                )
            )
        )

        stored = await client._await_resource_upload_result(
            request_sequence=5,
            timeout=0.1,
        )
        self.assertEqual(stored.request_sequence, 5)
        mutation_resp = await client._wait_for("request_result", timeout=0.1)
        self.assertEqual(mutation_resp.request_result.batch_id, b"\x11" * 16)

    async def test_upload_png_resource_rejects_payload_over_inline_limit(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        oversized = b"\x00" * ((64 * 1024) + 1)
        with self.assertRaisesRegex(ValueError, "chunked upload is not implemented"):
            await client.upload_png_resource(oversized)

    async def test_create_presence_card_tile_sequences_helper_calls(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        lease_id = b"\x22" * 16
        client.claim_tile = AsyncMock(
            return_value=ClaimedTile(lease_id=lease_id, tile_id=b"tile-id", node_ids=[])
        )
        client.update_tile_input_mode = AsyncMock()
        client.set_tile_root = AsyncMock()
        client.add_node = AsyncMock(side_effect=[f"node-{idx}".encode() for idx in range(12)])

        avatar_resource_id = b"\x44" * 32

        claimed = await client.create_presence_card_tile(
            agent_name="agent-alpha",
            avatar_resource_id=avatar_resource_id,
        )

        self.assertEqual(claimed.tile_id, b"tile-id")
        client.claim_tile.assert_awaited_once_with(
            anchor=session_pb2.TILE_ANCHOR_BOTTOM_LEFT,
            size=session_pb2.TILE_SIZE_MEDIUM,
            ttl_ms=60000,
        )
        client.update_tile_input_mode.assert_awaited_once_with(
            lease_id,
            b"tile-id",
            types_pb2.TILE_INPUT_MODE_CAPTURE,
        )
        client.set_tile_root.assert_awaited_once()
        client.add_node.assert_awaited()
        root_node = client.set_tile_root.await_args.args[2]
        self.assertTrue(root_node.HasField("solid_color"))
        self.assertEqual(root_node.solid_color.bounds.width, 320.0)
        self.assertEqual(root_node.solid_color.bounds.height, 112.0)
        self.assertEqual(root_node.solid_color.radius, 12.0)

        self.assertEqual(client.add_node.await_count, 12)
        for awaited in client.add_node.await_args_list:
            self.assertEqual(awaited.kwargs["parent_id"], root_node.id)

        self.assertTrue(client.add_node.await_args_list[0].args[2].HasField("solid_color"))
        self.assertTrue(client.add_node.await_args_list[3].args[2].HasField("static_image"))
        self.assertEqual(
            client.add_node.await_args_list[4].args[2].text_markdown.content,
            "RESIDENT AGENT",
        )
        self.assertEqual(
            client.add_node.await_args_list[5].args[2].text_markdown.content,
            "**agent-alpha**",
        )
        self.assertEqual(
            client.add_node.await_args_list[6].args[2].text_markdown.content,
            "Connected • last active now",
        )
        self.assertEqual(
            client.add_node.await_args_list[8].args[2].text_markdown.content,
            "NOW",
        )
        self.assertTrue(client.add_node.await_args_list[9].args[2].HasField("solid_color"))
        self.assertEqual(
            client.add_node.await_args_list[10].args[2].text_markdown.content,
            "X",
        )
        self.assertEqual(
            client.add_node.await_args_list[11].args[2].hit_region.interaction_id,
            "dismiss-card",
        )

    async def test_disconnect_primitives_split_graceful_and_hard_paths(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        client._send = AsyncMock()
        client._shutdown_transport = AsyncMock()

        await client.session_close()
        client._send.assert_awaited_once()
        close_kwargs = client._send.await_args.kwargs
        self.assertIn("session_close", close_kwargs)

        client._send.reset_mock()
        client._shutdown_transport.reset_mock()
        client._session_close_sent = False

        await client.disconnect(graceful=True)
        client._send.assert_awaited_once()
        client._shutdown_transport.assert_awaited_once()

        client._send.reset_mock()
        client._shutdown_transport.reset_mock()

        await client.disconnect(graceful=False)
        client._shutdown_transport.assert_awaited_once()
        client._send.assert_not_called()

    async def test_claim_tile_reports_code_and_hint(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        client._send = AsyncMock(return_value=2)
        client._wait_for = AsyncMock(
            return_value=session_pb2.ServerMessage(
                request_result=session_pb2.RequestResult(
                    seq=2,
                    ok=False,
                    code="NOT_ALLOWED",
                    hint='needs "tiles" in [agents.test] allow',
                )
            )
        )

        with self.assertRaisesRegex(RuntimeError, r"NOT_ALLOWED.*needs \"tiles\""):
            await client.claim_tile(ttl_ms=120_000)

    async def test_claim_tile_returns_lease_tile_and_node_ids(self):
        client = HudClient("example.invalid:50051", psk="test-key")
        client._send = AsyncMock(return_value=2)
        client._wait_for = AsyncMock(
            return_value=session_pb2.ServerMessage(
                request_result=session_pb2.RequestResult(
                    seq=2,
                    ok=True,
                    ids=[b"\x01" * 16, b"\x02" * 16],
                    lease_id=b"\x03" * 16,
                    ttl_ms=60_000,
                )
            )
        )

        claimed = await client.claim_tile(root={"solid_color": {"r": 1, "g": 0, "b": 0, "a": 1}})

        self.assertEqual(claimed.tile_id, b"\x01" * 16)
        self.assertEqual(claimed.node_ids, [b"\x02" * 16])
        self.assertEqual(claimed.lease_id, b"\x03" * 16)
        self.assertEqual(client.last_granted_lease_ttl_ms, 60_000)
        sent = client._send.await_args.kwargs["claim_tile"]
        self.assertTrue(sent.HasField("root"))

    def test_tile_surface_is_hyphenated_uuid(self):
        self.assertEqual(
            tile_surface(bytes(range(16))),
            "tile:00010203-0405-0607-0809-0a0b0c0d0e0f",
        )

    # ── Bounded mutation-ack retry (hud-n5bqp) ────────────────────────────────
    def _mutation_retry_client(self):
        """Build a client whose transport is stubbed so submit_mutation_batch
        can be exercised without a real gRPC stream. Returns (client,
        sent_batch_ids, set_wait) where sent_batch_ids records every batch id
        actually sent and set_wait installs a scripted _wait_for."""
        client = HudClient("example.invalid:50051", psk="test-key")
        sent_batch_ids: list[bytes] = []

        async def capture_send(**payload_kwargs):
            batch = payload_kwargs.get("mutation_batch")
            if batch is not None:
                sent_batch_ids.append(bytes(batch.batch_id))
            return 0

        client._send = capture_send

        def set_wait(fn):
            client._wait_for = fn

        return client, sent_batch_ids, set_wait

    @staticmethod
    def _accepted_result(batch_id: bytes):
        return session_pb2.ServerMessage(
            request_result=session_pb2.RequestResult(
                batch_id=batch_id,
                ok=True,
            )
        )

    # ── Opt-in mutation-batch pacing (hud-2j9as) ────────────────────────────
    async def test_default_batch_pacing_is_unpaced(self):
        """Clients that do not opt in preserve their existing burst behavior."""
        client = HudClient("example.invalid:50051", psk="test-key")

        with patch(
            "hud_grpc_client.asyncio.sleep", new_callable=AsyncMock
        ) as sleep:
            await client._pace_batch_send()

        sleep.assert_not_awaited()

    async def test_configured_batch_pacing_waits_for_remaining_interval(self):
        """An opted-in client sleeps only for the unelapsed interval remainder."""
        client = HudClient("example.invalid:50051", psk="test-key")
        client.configure_batch_pacing(0.035)
        client._last_batch_send_mono = 10.0

        with patch(
            "hud_grpc_client.time.monotonic", side_effect=[10.010, 10.035]
        ), patch(
            "hud_grpc_client.asyncio.sleep", new_callable=AsyncMock
        ) as sleep:
            await client._pace_batch_send()

        sleep.assert_awaited_once()
        self.assertAlmostEqual(sleep.await_args.args[0], 0.025, places=6)
        self.assertEqual(client._last_batch_send_mono, 10.035)

    async def test_configured_batch_pacing_serializes_concurrent_waiters(self):
        """Concurrent callers cannot clear the same pacing interval together."""
        client, sent_batch_ids, set_wait = self._mutation_retry_client()
        client.configure_batch_pacing(0.035)
        client._last_batch_send_mono = 10.0
        native_sleep = asyncio.sleep
        first_sleep_entered = asyncio.Event()
        release_first_sleep = asyncio.Event()
        sleep_calls: list[float] = []

        async def controlled_sleep(delay: float) -> None:
            sleep_calls.append(delay)
            if len(sleep_calls) == 1:
                first_sleep_entered.set()
                await release_first_sleep.wait()

        async def accepted_wait(payload_name, timeout, matcher=None):
            return self._accepted_result(sent_batch_ids[-1])

        set_wait(accepted_wait)
        first = None
        second = None
        with patch(
            "hud_grpc_client.time", SimpleNamespace(monotonic=lambda: 10.0)
        ), patch(
            "hud_grpc_client.asyncio.sleep", side_effect=controlled_sleep,
        ):
            try:
                first = asyncio.create_task(
                    client.apply_mutations(b"\x01" * 16, [])
                )
                await asyncio.wait_for(first_sleep_entered.wait(), timeout=0.5)
                second = asyncio.create_task(
                    client.apply_mutations(b"\x01" * 16, [])
                )
                await native_sleep(0)
                self.assertEqual(len(sleep_calls), 1)
                self.assertEqual(sent_batch_ids, [])
            finally:
                release_first_sleep.set()
                await asyncio.gather(
                    *(task for task in (first, second) if task is not None),
                )

    async def test_configured_batch_pacing_gates_apply_and_retry_submit_paths(self):
        """Both mutation-batch choke points use the same opt-in pacing gate."""
        client, sent_batch_ids, set_wait = self._mutation_retry_client()
        client.configure_batch_pacing(0.035)
        pace = AsyncMock()
        client._pace_batch_send = pace

        async def accepted_wait(payload_name, timeout, matcher=None):
            return self._accepted_result(sent_batch_ids[-1])

        set_wait(accepted_wait)

        await client.apply_mutations(b"\x01" * 16, [])
        await client._submit_mutation_batch_once(b"\x01" * 16, [], timeout=0.05)

        self.assertEqual(pace.await_count, 2)

    async def test_submit_mutation_batch_retries_single_transient_timeout(self):
        """A single transient mutation-ack timeout is retried (resubmitted with
        a fresh batch id) and the call still succeeds — the soak continues."""
        client, sent_batch_ids, set_wait = self._mutation_retry_client()
        calls = {"n": 0}

        async def flaky_wait(payload_name, timeout, matcher=None):
            calls["n"] += 1
            if calls["n"] == 1:
                raise TimeoutError("Timed out waiting for request_result")
            return self._accepted_result(sent_batch_ids[-1])

        set_wait(flaky_wait)

        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            mr = await client.submit_mutation_batch(
                b"\x01" * 16, [], timeout=0.05, retries=3, retry_backoff_s=0.0,
            )

        self.assertTrue(mr.ok)
        # Original send + exactly one resubmit, each with a distinct batch id.
        self.assertEqual(len(sent_batch_ids), 2)
        self.assertNotEqual(sent_batch_ids[0], sent_batch_ids[1])
        # The retry is logged visibly (a recurring wall must stay detectable).
        self.assertIn("mutation-ack timeout", buf.getvalue())
        self.assertIn("retry 1/3", buf.getvalue())

    async def test_submit_mutation_batch_aborts_after_exhausting_retries(self):
        """A sustained ack wall (every attempt times out) still aborts once the
        bounded retry budget is exhausted — the blip tolerance is not infinite."""
        client, sent_batch_ids, set_wait = self._mutation_retry_client()

        async def always_timeout(payload_name, timeout, matcher=None):
            raise TimeoutError("Timed out waiting for request_result")

        set_wait(always_timeout)

        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            with self.assertRaises(TimeoutError):
                await client.submit_mutation_batch(
                    b"\x01" * 16, [], timeout=0.01, retries=2, retry_backoff_s=0.0,
                )

        # First attempt + 2 retries = 3 sends, then the TimeoutError propagates.
        self.assertEqual(len(sent_batch_ids), 3)
        self.assertEqual(buf.getvalue().count("mutation-ack timeout"), 2)

    async def test_submit_mutation_batch_default_is_fail_fast(self):
        """With no retry policy configured (the default), a single ack timeout
        aborts immediately — non-soak callers keep fail-fast semantics."""
        client, sent_batch_ids, set_wait = self._mutation_retry_client()

        async def always_timeout(payload_name, timeout, matcher=None):
            raise TimeoutError("Timed out waiting for request_result")

        set_wait(always_timeout)

        with self.assertRaises(TimeoutError):
            await client.submit_mutation_batch(b"\x01" * 16, [], timeout=0.01)

        self.assertEqual(len(sent_batch_ids), 1)

    async def test_configure_mutation_retry_applies_as_call_default(self):
        """configure_mutation_retry sets the per-client budget used when
        submit_mutation_batch is called without explicit retry args — the lever
        the soak driver pulls before publishing through its helpers."""
        client, sent_batch_ids, set_wait = self._mutation_retry_client()
        client.configure_mutation_retry(2, backoff_s=0.0)
        calls = {"n": 0}

        async def flaky_wait(payload_name, timeout, matcher=None):
            calls["n"] += 1
            if calls["n"] == 1:
                raise TimeoutError("Timed out waiting for request_result")
            return self._accepted_result(sent_batch_ids[-1])

        set_wait(flaky_wait)

        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            mr = await client.submit_mutation_batch(b"\x01" * 16, [], timeout=0.05)

        self.assertTrue(mr.ok)
        self.assertEqual(len(sent_batch_ids), 2)
        # Resetting the policy to 0 restores fail-fast.
        client.configure_mutation_retry(0)
        calls["n"] = 0
        sent_batch_ids.clear()
        with self.assertRaises(TimeoutError):
            await client.submit_mutation_batch(b"\x01" * 16, [], timeout=0.01)
        self.assertEqual(len(sent_batch_ids), 1)

    async def test_submit_mutation_batch_does_not_retry_rejection(self):
        """A rejected batch (RuntimeError) is a real failure, not a transient
        blip — it must propagate immediately without resubmission."""
        client, sent_batch_ids, set_wait = self._mutation_retry_client()

        async def reject_wait(payload_name, timeout, matcher=None):
            return session_pb2.ServerMessage(
                request_result=session_pb2.RequestResult(
                    batch_id=sent_batch_ids[-1],
                    ok=False,
                    code="LEASE_EXPIRED",
                    hint="lease expired",
                )
            )

        set_wait(reject_wait)

        with self.assertRaisesRegex(RuntimeError, "LEASE_EXPIRED"):
            await client.submit_mutation_batch(
                b"\x01" * 16, [], timeout=0.05, retries=3, retry_backoff_s=0.0,
            )

        self.assertEqual(len(sent_batch_ids), 1)


if __name__ == "__main__":
    unittest.main()
