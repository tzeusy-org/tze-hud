"""Portable client and pixel-evidence contracts for the native raster witness."""

import asyncio
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import resident_raster_witness as raster
from windows_smoke import encode_png_rgba


class ResidentRasterWitnessTests(unittest.IsolatedAsyncioTestCase):
    async def test_wait_due_rechecks_an_early_wake_without_a_backwards_wall_clock(self) -> None:
        report = {"captures": {"before-due": "unchanged"}}
        with mock.patch.object(raster, "wall_us", side_effect=[0, 990_000, 1_000_000]), \
                mock.patch.object(raster, "time") as clock, \
                mock.patch.object(raster.asyncio, "sleep", new_callable=mock.AsyncMock) as sleep:
            clock.monotonic.side_effect = [10, 10.99, 11]
            clock.get_clock_info.return_value = mock.Mock(resolution=0.015625, implementation="fixture clock")
            await raster.wait_due(1_000_000, report=report)
        self.assertEqual(sleep.await_count, 2)
        self.assertEqual([call.args[0] for call in sleep.await_args_list], [1.0, 0.03125])
        timing, = report["waits"]
        self.assertEqual((timing["outcome"], timing["wakes_before_target"], timing["backward_wall_steps"]),
                         ("reached", 1, 0))
        self.assertEqual(timing["finished_wall_us"], timing["target_wall_us"])
        self.assertEqual(report["captures"], {"before-due": "unchanged"})

    async def test_wait_due_preserves_settle_and_times_out_on_stalled_or_backward_wall_time(self) -> None:
        cases = [
            # name, due, after_s, wall observations, monotonic observations, outcome, sleeps
            ("already due", 1_000_000, 0, [2_000_000], [5], "reached", []),
            ("denied settle", 1_000_000, 0.5, [0, 1_000_000, 1_500_000], [5, 6, 6.5], "reached", [1.5, 0.5]),
            ("past due settle", 1_000_000, 0.5, [2_000_000, 2_500_000], [5, 5.5], "reached", [0.5]),
            ("forward jump retains settle", 1_000_000, 0.5, [0, 4_000_000, 4_000_000], [5, 5.5, 6.5],
             "reached", [1.5, 1.0]),
            ("stalled", 1_000_000, 0, [0, 0], [5, 8], "timeout", [1.0]),
            ("backward", 1_000_000, 0, [0, -500_000], [5, 8], "timeout", [1.0]),
            ("absolute cap", 100_000_000, 0, [0, 1_000_000], [5, 15], "timeout", [10.0]),
            ("late wall target", 1_000_000, 0, [0, 1_000_000], [5, 9], "timeout", [1.0]),
        ]
        for name, due, after_s, wall, monotonic, outcome, sleeps in cases:
            with self.subTest(name=name), mock.patch.object(raster, "wall_us", side_effect=wall), \
                    mock.patch.object(raster, "time") as clock, \
                    mock.patch.object(raster.asyncio, "sleep", new_callable=mock.AsyncMock) as sleep:
                clock.monotonic.side_effect = monotonic
                clock.get_clock_info.return_value = mock.Mock(resolution=0.015625, implementation="fixture clock")
                report = {}
                if outcome == "timeout":
                    with self.assertRaisesRegex(AssertionError, "monotonic wait budget"):
                        await raster.wait_due(due, after_s=after_s, report=report)
                else:
                    await raster.wait_due(due, after_s=after_s, report=report)
                timing, = report["waits"]
                self.assertEqual(timing["outcome"], outcome)
                self.assertEqual([call.args[0] for call in sleep.await_args_list], sleeps)
                self.assertEqual(report["phase"], "wait-due")
                self.assertLessEqual(timing["budget_s"], 10)
                self.assertEqual(timing["backward_wall_steps"], int(name == "backward"))
                self.assertTrue(all(delay >= 0.03125 for delay in sleeps))
                if outcome == "reached":
                    self.assertGreaterEqual(timing["finished_wall_us"], due)
                    self.assertGreaterEqual(timing["finished_mono_s"], timing["not_before_mono_s"])
                    self.assertEqual(timing["target_wall_us"], max(due, wall[0]) + int(after_s * 1_000_000))

    def test_running_build_identity_rejects_other_checkouts_and_processes(self) -> None:
        run = mock.Mock(pid=42, **{"status.return_value": {"pid": 42, "sha": "a" * 40}})
        self.assertEqual(raster.build_identity(run, "a" * 40), {"pid": 42, "sha": "a" * 40})
        with self.assertRaisesRegex(AssertionError, "checkout"):
            raster.build_identity(run, "b" * 40)
        run.status.return_value = {"pid": 43, "sha": "a" * 40}
        with self.assertRaisesRegex(AssertionError, "process"):
            raster.build_identity(run, "a" * 40)

    async def test_capture_decodes_direction_and_rejects_an_early_frame_that_arrives_late(self) -> None:
        src = raster.directional_rgba()
        # Derive a fullscreen rendering from the uploaded texels without using
        # the expected colour list in the generated capture.
        width, height = 160, 120
        frame = bytearray()
        for y in range(height):
            for x in range(width):
                offset = (y * raster.HEIGHT // height * raster.WIDTH + x * raster.WIDTH // width) * 4
                frame.extend(src[offset:offset + 4])
        png = encode_png_rgba(width, height, frame)
        smoke = mock.Mock(**{"get_bytes.return_value": (200, "image/png", png)})
        with tempfile.TemporaryDirectory() as directory:
            run = mock.Mock(out=Path(directory), smoke=smoke)
            report = {"captures": {}}

            def right_order(samples):
                return raster.matches(samples["ambient"], raster.DIRECTIONAL, 0)

            with mock.patch.object(raster, "wall_us", return_value=99):
                await raster.capture(run, report, "early", right_order, before_us=100)
            with mock.patch.object(raster, "wall_us", return_value=100), self.assertRaises(AssertionError):
                await raster.capture(run, report, "late", right_order, before_us=100)
            self.assertEqual((Path(directory) / "late.png").read_bytes(), png)
            self.assertEqual(report["captures"]["late"]["samples"]["ambient"], raster.DIRECTIONAL)
            self.assertFalse(raster.matches(report["captures"]["late"]["samples"]["ambient"],
                                            raster.DIRECTIONAL[::-1], 16))

    async def test_stream_correlates_reuse_and_refusal_without_leaking_the_credential(self) -> None:
        s = raster.session
        data = raster.directional_rgba()
        digest = raster.blake3.blake3(data).digest()
        call = mock.Mock(write=mock.AsyncMock(), read=mock.AsyncMock(side_effect=[
            s.ServerMessage(session_established=s.SessionEstablished(
                session_id=b"test session", namespace="fixture", heartbeat_interval_ms=5000)),
            s.ServerMessage(degradation_notice=s.DegradationNotice()),
            s.ServerMessage(heartbeat=s.Heartbeat()),
            s.ServerMessage(resource_stored=s.ResourceStored(request_sequence=2, was_deduplicated=True,
                decoded_bytes=len(data), resource_id=raster.types.ResourceIdProto(bytes=digest))),
            s.ServerMessage(request_result=s.RequestResult(seq=3, code="INVALID_ARGUMENT", hint="resource")),
            s.ServerMessage(request_result=s.RequestResult(seq=3, ok=True)),
        ]))
        report = {"replies": []}
        stream = raster.Resident(mock.Mock(**{"Session.return_value": call}), "fixture", report)
        self.assertEqual(await stream.connect("fixture-credential"), 2.5)
        await stream.upload(data, digest, deduplicated=True, reuse=True)
        await stream.request("publish", raster.image(raster.PIP, digest), deny=True)
        with self.assertRaisesRegex(AssertionError, "sequence"):
            await stream.request("clear", s.Clear(surface=raster.PIP))
        envelopes = [event.args[0] for event in call.write.await_args_list]
        self.assertEqual([msg.sequence for msg in envelopes], [1, 2, 3, 4])
        upload = envelopes[1].resource_upload_start
        self.assertEqual((upload.total_size_bytes, upload.inline_data), (len(data), b""))
        self.assertNotIn("fixture-credential", repr(report))

        writing, finish, stop = asyncio.Event(), asyncio.Event(), asyncio.Event()

        async def in_flight_heartbeat(message):
            self.assertEqual(message.WhichOneof("payload"), "heartbeat")
            writing.set()
            await finish.wait()

        call.write.side_effect = in_flight_heartbeat
        keepalive = asyncio.create_task(stream.heartbeat(0, stop))
        try:
            await asyncio.wait_for(writing.wait(), 1)
            stop.set()
            self.assertFalse(keepalive.done())
        finally:
            stop.set()
            finish.set()
            await asyncio.wait_for(keepalive, 1)
