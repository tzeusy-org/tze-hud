"""Prove paired resident upload-to-compositor pixels with the canonical app.

Private Xvfb: .venv/bin/python3 scripts/ci/resident_raster_witness.py \
    --exe target/release-dev/tze_hud --out test_results/resident-raster
The Windows workflow supplies its already-built fullscreen executable on hosted Windows.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import statistics
import subprocess
import sys
import time
from pathlib import Path

import blake3
import grpc

from hud_gestures import CONFIG, FOREIGN_AGENT, LAVAPIPE, REPO, WINDOWS, Refused, Run
from windows_smoke import AGENT, capture_frame

sys.path.insert(0, str(REPO / ".claude/skills/user-test-performance/scripts"))
from proto_gen import session_pb2 as session, session_pb2_grpc, types_pb2 as types  # noqa: E402

PIP, AMBIENT = "zone:pip", "zone:ambient-background"
WIDTH, HEIGHT = 8, 6
RED, GREEN, BLUE, YELLOW = (255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0)
DIRECTIONAL = (RED, GREEN, BLUE, YELLOW)
MARKER = ((255, 0, 255),) * 4
POINTS = {
    # Inset quadrants from the built-in pip rect x=.75..97, y=.40..66.
    "pip": ((.805, .465), (.915, .465), (.805, .595), (.915, .595)),
    # Ambient quadrants, all outside pip and the shell controls.
    "ambient": ((.22, .24), (.65, .24), (.22, .74), (.65, .74)),
}
RPC_TIMEOUT = 5
SCHEDULE_US = 4_000_000
DUE_WAIT_SLACK_S = 2.0
DUE_WAIT_MAX_S = 10.0


def wall_us() -> int:
    return time.time_ns() // 1000


def directional_rgba() -> bytes:
    """Quad order is TL, TR, BL, BR. Every source texel is opaque."""
    tl, tr, bl, br = (bytes((*rgb, 255)) for rgb in DIRECTIONAL)
    top = (tl * (WIDTH // 2) + tr * (WIDTH // 2)) * (HEIGHT // 2)
    bottom = (bl * (WIDTH // 2) + br * (WIDTH // 2)) * (HEIGHT // 2)
    return top + bottom


def sample_rgb(frame: tuple[int, int, bytes]) -> dict:
    width, height, rgba = frame
    assert width >= 64 and height >= 64 and len(rgba) == width * height * 4, "invalid capture geometry"
    samples = {}
    for zone, points in POINTS.items():
        patches = []
        for u, v in points:
            x, y = int(width * u), int(height * v)
            offsets = [((y + dy) * width + x + dx) * 4 for dy in (-1, 0, 1) for dx in (-1, 0, 1)]
            patches.append(tuple(statistics.median(rgba[offset + c] for offset in offsets) for c in range(3)))
        samples[zone] = tuple(patches)
    return samples


def matches(actual: tuple, expected: tuple, tolerance: int) -> bool:
    return all(abs(channel - wanted) <= tolerance
               for rgb, expected_rgb in zip(actual, expected, strict=True)
               for channel, wanted in zip(rgb, expected_rgb, strict=True))


class Resident:
    """One real paired stream, with sequence-correlated replies and writes."""

    def __init__(self, stub: session_pb2_grpc.HudSessionStub, namespace: str, report: dict) -> None:
        self.call = stub.Session(timeout=100)
        self.namespace = namespace
        self.replies = report["replies"]
        self.sequence = 0
        self.write_lock = asyncio.Lock()

    async def send(self, **payload) -> int:
        async with self.write_lock:
            self.sequence += 1
            envelope = session.ClientMessage(sequence=self.sequence, timestamp_wall_us=wall_us(), **payload)
            await asyncio.wait_for(self.call.write(envelope), RPC_TIMEOUT)
            return self.sequence

    async def expect(self, name: str, *, sequence: int | None = None, correlation: str = "seq"):
        async with asyncio.timeout(RPC_TIMEOUT):
            while True:
                message = await self.call.read()
                assert message is not grpc.aio.EOF, "resident server closed before its reply"
                kind = message.WhichOneof("payload")
                if kind in {"heartbeat", "degradation_notice"}:
                    continue
                assert kind == name, f"expected {name}, received {kind}"
                reply = getattr(message, kind)
                if sequence is not None:
                    assert getattr(reply, correlation) == sequence, f"{kind} reply sequence did not match request"
                return reply

    async def connect(self, psk: str) -> float:
        await self.send(session_init=session.SessionInit(
            agent_id=self.namespace,
            auth_credential=session.AuthCredential(pre_shared_key=session.PreSharedKeyCredential(key=psk)),
        ))
        reply = await self.expect("session_established")
        assert reply.namespace == self.namespace and reply.session_id, "wrong resident identity"
        assert reply.heartbeat_interval_ms > 0, "missing resident heartbeat interval"
        return reply.heartbeat_interval_ms / 2000

    async def heartbeat(self, interval_s: float, stop: asyncio.Event) -> None:
        while not stop.is_set():
            try:
                await asyncio.wait_for(stop.wait(), interval_s)
            except TimeoutError:
                await self.send(heartbeat=session.Heartbeat(timestamp_mono_us=time.monotonic_ns() // 1000))

    async def upload(self, data: bytes, digest: bytes, *, deduplicated: bool, reuse: bool = False) -> None:
        seq = await self.send(resource_upload_start=session.ResourceUploadStart(
            expected_hash=digest, resource_type=session.IMAGE_RGBA8, total_size_bytes=len(data),
            metadata=session.ResourceMetadata(width=WIDTH, height=HEIGHT), inline_data=b"" if reuse else data,
        ))
        reply = await self.expect("resource_stored", sequence=seq, correlation="request_sequence")
        self.replies.append(dict(agent=self.namespace, operation="reuse" if reuse else "upload", seq=seq,
                                 digest=reply.resource_id.bytes.hex(), decoded_bytes=reply.decoded_bytes,
                                 deduplicated=reply.was_deduplicated))
        assert reply.resource_id.bytes == digest and reply.decoded_bytes == len(data), "wrong stored raster"
        assert reply.was_deduplicated == deduplicated, "wrong raster deduplication result"

    async def request(self, verb: str, request, *, deny: bool = False) -> None:
        seq = await self.send(**{verb: request})
        reply = await self.expect("request_result", sequence=seq)
        self.replies.append(dict(agent=self.namespace, operation=verb, surface=request.surface, seq=seq,
                                 present_at_us=getattr(request, "present_at_us", 0), ok=reply.ok, code=reply.code))
        if deny:
            assert not reply.ok and reply.code == "INVALID_ARGUMENT" and "resource" in reply.hint.lower(), \
                "expected a static-image resource refusal"
        else:
            assert reply.ok, f"{verb} was refused ({reply.code})"


def image(surface: str, digest: bytes, *, due: int = 0) -> session.Publish:
    content = types.ZoneContent(static_image_ref=types.StaticImageRef(resource_id=digest))
    return session.Publish(surface=surface, content=content, present_at_us=due)


async def capture(run: Run, report: dict, name: str, check, *, settle_s: float = 0, before_us: int = 0) -> dict:
    report["phase"] = name
    deadline = time.monotonic() + settle_s
    attempt = {"captures": {}}
    while True:
        try:
            frame, _, _ = await asyncio.to_thread(capture_frame, run.smoke, evidence=attempt, name=name)
        except Exception:
            report["captures"][name] = attempt["captures"].get(name, {})
            raise
        finished_us = wall_us()
        samples = sample_rgb(frame)
        passed = check(samples)
        if passed or time.monotonic() >= deadline:
            break
        await asyncio.sleep(0.2)
    timing = attempt["captures"][name]
    (run.out / f"{name}.png").write_bytes(timing.pop("png"))
    report["captures"][name] = dict(timing, samples=samples, finished_wall_us=finished_us)
    assert not before_us or finished_us < before_us, f"{name} did not finish before due"
    assert passed, f"unexpected directional samples at {name}"
    print(f"ok  {name}", flush=True)
    return samples


async def wait_due(due: int, *, after_s: float = 0, report: dict | None = None) -> None:
    """Recheck the wall target after timer wakes; never retry a capture."""
    clock = time.get_clock_info("monotonic")
    started_wall, started_mono = wall_us(), time.monotonic()
    # Retain the full requested settle even when the deadline is already past.
    target = max(due, started_wall) + int(after_s * 1_000_000)
    duration = (target - started_wall) / 1_000_000
    not_before_mono = started_mono + duration
    budget = min(DUE_WAIT_MAX_S, duration + DUE_WAIT_SLACK_S)
    # asyncio timers may wake early by a clock tick. Avoid a near-target spin.
    minimum_sleep = max(0.01, 2 * clock.resolution)
    timing = dict(due_us=due, after_s=after_s, target_wall_us=target,
                  started_wall_us=started_wall, started_mono_s=started_mono, budget_s=budget,
                  not_before_mono_s=not_before_mono, minimum_sleep_s=minimum_sleep,
                  sleeps=0, wakes_before_target=0, wakes_before_minimum=0,
                  backward_wall_steps=0, largest_backward_us=0, outcome="waiting",
                  python=sys.version, loop=type(asyncio.get_running_loop()).__name__,
                  monotonic_clock=clock.implementation, monotonic_resolution_s=clock.resolution,
                  wall_clock_resolution_s=time.get_clock_info("time").resolution)
    if report is not None:
        report["phase"] = "wait-due"
        report.setdefault("waits", []).append(timing)
    now_wall, now_mono = started_wall, started_mono
    while True:
        elapsed = now_mono - started_mono
        timing.update(finished_wall_us=now_wall, finished_mono_s=now_mono, elapsed_mono_s=elapsed)
        if elapsed >= budget:
            timing["outcome"] = "timeout"
            raise AssertionError("wall target not observed within the monotonic wait budget")
        if now_wall >= target and now_mono >= not_before_mono:
            timing["outcome"] = "reached"
            return
        # The floor can exceed the remaining budget by one short sleep; the
        # next observation still checks timeout before accepting the wall time.
        remaining = max((target - now_wall) / 1_000_000, not_before_mono - now_mono)
        delay = max(minimum_sleep, min(remaining, budget - elapsed))
        timing.update(last_sleep_s=delay, sleeps=timing["sleeps"] + 1)
        await asyncio.sleep(delay)
        previous_wall = now_wall
        now_wall, now_mono = wall_us(), time.monotonic()
        if now_wall < previous_wall:
            timing["backward_wall_steps"] += 1
            timing["largest_backward_us"] = max(timing["largest_backward_us"], previous_wall - now_wall)
        if now_wall < target:
            timing["wakes_before_target"] += 1
        if now_mono < not_before_mono:
            timing["wakes_before_minimum"] += 1


async def exercise(run: Run, report: dict, owner: Resident, peer: Resident) -> None:
    data = directional_rgba()
    digest = blake3.blake3(data).digest()
    report["phase"] = "peer-marker"
    marker = types.ZoneContent(solid_color=types.Rgba(r=1, b=1, a=1))
    await peer.request("publish", session.Publish(surface=AMBIENT, content=marker))
    marker_pixels = await capture(run, report, "peer-marker", lambda s: matches(s["ambient"], MARKER, 66)
                                  and matches(s["pip"], MARKER, 66), settle_s=6)

    report["phase"] = "owner-upload"
    await owner.upload(data, digest, deduplicated=False)

    def unchanged(samples):
        return all(matches(samples[zone], marker_pixels[zone], 6) for zone in POINTS)

    await capture(run, report, "upload-before-publish", unchanged)

    report["phase"] = "immediate-denials"
    await owner.request("publish", image(PIP, blake3.blake3(b"absent raster fixture").digest()), deny=True)
    await peer.request("publish", image(AMBIENT, digest), deny=True)
    await capture(run, report, "after-immediate-denials", unchanged)

    report["phase"] = "scheduled-denial"
    denied_due = wall_us() + SCHEDULE_US
    await peer.request("publish", image(AMBIENT, digest, due=denied_due), deny=True)
    await wait_due(denied_due, after_s=0.5, report=report)
    await capture(run, report, "after-denied-due", unchanged)

    report["phase"] = "peer-first-grant"
    await peer.upload(data, digest, deduplicated=True)
    await peer.request("publish", image(AMBIENT, digest))
    peer_pixels = await capture(run, report, "peer-image", lambda s: matches(s["ambient"], DIRECTIONAL, 66),
                                settle_s=6)

    def peer_unchanged(samples):
        return all(matches(samples[zone], peer_pixels[zone], 6) for zone in POINTS)

    report["phase"] = "owner-schedule"
    due = wall_us() + SCHEDULE_US
    await owner.request("publish", image(PIP, digest, due=due))
    await capture(run, report, "before-due", peer_unchanged, before_us=due)
    await wait_due(due, report=report)
    await capture(run, report, "after-due", lambda s: matches(s["pip"], DIRECTIONAL, 16)
                  and matches(s["ambient"], peer_pixels["ambient"], 6), settle_s=8)

    report["phase"] = "owner-clear"
    await owner.request("clear", session.Clear(surface=PIP))
    # Compare the same patches, including the ambient image's bilinear transitions.
    await capture(run, report, "after-clear", peer_unchanged, settle_s=6)

    report["phase"] = "owner-reuse"
    await owner.upload(data, digest, deduplicated=True, reuse=True)
    await owner.request("publish", image(PIP, digest))
    await capture(run, report, "republished", lambda s: matches(s["pip"], DIRECTIONAL, 16)
                  and matches(s["ambient"], peer_pixels["ambient"], 6), settle_s=6)


async def witness(run: Run, report: dict) -> None:
    async with grpc.aio.insecure_channel(f"127.0.0.1:{run.grpc_port}") as channel:
        await asyncio.wait_for(channel.channel_ready(), RPC_TIMEOUT)
        stub = session_pb2_grpc.HudSessionStub(channel)
        owner, peer = Resident(stub, AGENT, report), Resident(stub, FOREIGN_AGENT, report)
        owner_interval, peer_interval = await asyncio.gather(owner.connect(run.psk), peer.connect(run.foreign_psk))
        stop = asyncio.Event()
        beats = [asyncio.create_task(stream.heartbeat(interval, stop))
                 for stream, interval in ((owner, owner_interval), (peer, peer_interval))]
        try:
            await exercise(run, report, owner, peer)
        finally:
            # Let a heartbeat write finish before closing its stream. Canceling
            # a live write could cancel the RPC and mask a successful witness.
            stop.set()
            heartbeat_results = await asyncio.gather(*beats, return_exceptions=True)
        assert all(result is None for result in heartbeat_results), "resident heartbeat failed"
        for stream in (owner, peer):
            await stream.send(session_close=session.SessionClose(reason="raster witness complete"))


def build_identity(run: Run, checkout: str) -> dict:
    status = run.status()
    identity = dict(sha=status.get("sha"), pid=status.get("pid"))
    assert identity["pid"] == run.pid, "operator status belongs to a different process"
    assert identity["sha"] == checkout, "running build does not match the witness checkout"
    return identity


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    run = Run(exe=args.exe.resolve(), out=out, injector="sendinput" if WINDOWS else "xtest")
    report = dict(profile=run.exe.parent.name, outcome="FAIL", phase="startup", captures={}, replies=[])
    try:
        assert WINDOWS or LAVAPIPE.is_file(), "private Xvfb needs the software Vulkan ICD"
        checkout, tree = subprocess.run(["git", "rev-parse", "HEAD", "HEAD^{tree}"], cwd=REPO,
                                        capture_output=True, text=True, check=True, timeout=5).stdout.splitlines()
        report.update(checkout=checkout, tree=tree)
        with run.exe.open("rb") as binary:
            report["exe_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
        with CONFIG.open("rb") as config:
            report["config_sha256"] = hashlib.file_digest(config, "sha256").hexdigest()
        run.start(resident_raster=True)
        report.update(pid=run.pid, display=run.display or "primary", phase="build-identity")
        report["build_start"] = build_identity(run, checkout)
        report["phase"] = "authenticate"
        asyncio.run(witness(run, report))
        report["build_end"] = build_identity(run, checkout)
        assert run.hud.poll() is None, "owned HUD did not survive the witness"
        report.update(outcome="PASS", phase="complete")
    except Exception as error:  # Never log gRPC errors or protobufs containing a credential.
        report["error"] = type(error).__name__
        if isinstance(error, (AssertionError, Refused)):
            report["detail"] = str(error)
        elif isinstance(error, grpc.RpcError):
            report["grpc_status"] = error.code().name
        print(f"FAIL {report['phase']} ({report['error']})", file=sys.stderr)
    finally:
        try:
            survivors = run.stop()
            if survivors:
                report.update(outcome="FAIL", teardown=survivors)
        except Exception as error:
            report.update(outcome="FAIL", teardown=type(error).__name__)
        (out / "witness.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"{report['outcome']} resident raster witness", flush=True)
    return 0 if report["outcome"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
