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
"""
Python gRPC client for tze_hud session protocol.

Provides a high-level HudClient class for user-test scripts that need
to exercise tile creation, lease management, and node tree mutations
over the bidirectional gRPC session stream.

Usage:
    from hud_grpc_client import HudClient

    async with HudClient("windows-host.example:50051",
                          psk="tze-hud-key",
                          agent_id="test-agent") as client:
        lease_id = await client.request_lease(ttl_ms=60000)
        avatar_png = make_avatar_png((66, 133, 244))
        avatar_resource_id = await client.upload_avatar_png(avatar_png)
        tile_id = await client.create_presence_card_tile(
            lease_id,
            tab_id=None,
            agent_name="agent-alpha",
            avatar_resource_id=avatar_resource_id,
        )
        await client.session_close(expect_resume=False)
"""

from __future__ import annotations

import asyncio
import io
import json
import os
import sys
import time
import uuid
from collections.abc import Callable
from typing import Any, Optional

import grpc

# Proto stubs are in proto_gen/ relative to this file.
_SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, _SCRIPT_DIR)
sys.path.insert(0, os.path.join(_SCRIPT_DIR, "proto_gen"))

from proto_gen import session_pb2, session_pb2_grpc, types_pb2  # noqa: E402  # import must follow sys.path setup above


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _now_wall_us() -> int:
    """Current UTC wall-clock in microseconds since epoch."""
    return int(time.time() * 1_000_000)


def _uuid_bytes() -> bytes:
    """Generate a 16-byte UUID for batch/request IDs."""
    return uuid.uuid4().bytes


def _png_image_size(png_bytes: bytes) -> tuple[int, int]:
    """Return the dimensions of a PNG payload."""
    from PIL import Image

    with Image.open(io.BytesIO(png_bytes)) as img:
        img.load()
        return img.width, img.height


def make_avatar_png(rgb: tuple[int, int, int]) -> bytes:
    """Build a solid-color 32x32 PNG avatar fixture."""
    from PIL import Image

    if len(rgb) != 3:
        raise ValueError("avatar rgb must be a 3-tuple")
    if any(channel < 0 or channel > 255 for channel in rgb):
        raise ValueError("avatar rgb values must be 0..255")

    image = Image.new("RGB", (32, 32), rgb)
    buf = io.BytesIO()
    image.save(buf, format="PNG")
    return buf.getvalue()


def _blake3_digest_bytes(data: bytes) -> bytes:
    """Compute a BLAKE3 digest of ``data``.

    Requires the ``blake3`` Python package, which is declared in this script's
    PEP-723 dependency header (and installed by the user-test CI lane and by
    ``uv run``). We deliberately do NOT fall back to compiling a throwaway Rust
    helper on the fly: that hid a slow (2s+ warm cache) crates.io network
    dependency behind a "pure Python" surface and broke offline. Fail fast with
    an actionable message instead.

    Catches the broad ``ImportError`` rather than only ``ModuleNotFoundError``:
    ``blake3`` is a compiled Rust extension, so a present-but-broken install
    (binary/ABI mismatch, missing shared library, Windows DLL load failure)
    raises a bare ``ImportError`` — which must produce the same actionable
    message, not propagate raw.
    """
    try:
        import blake3  # type: ignore
    except ImportError as exc:
        raise RuntimeError(
            "BLAKE3 digest requires a working 'blake3' Python package, but it is "
            "missing or failed to load (it is a compiled extension, so a broken "
            "wheel raises ImportError). Install it with: pip install blake3  "
            "(add --force-reinstall if a broken wheel is already present, or run "
            "this script via 'uv run', which installs the PEP-723 dependencies "
            "automatically). Required for avatar/image digest verification."
        ) from exc

    return blake3.blake3(data).digest()


def avatar_resource_id_from_png(png_bytes: bytes) -> bytes:
    """Validate a 32x32 PNG avatar and return its content-addressed ResourceId."""
    if _png_image_size(png_bytes) != (32, 32):
        raise ValueError("avatar PNG must be exactly 32x32 pixels")
    return _blake3_digest_bytes(png_bytes)

def _resource_id_bytes(resource_id: Any) -> bytes:
    """Normalize a raw resource id or ResourceIdProto into 32 bytes."""
    if isinstance(resource_id, types_pb2.ResourceIdProto):
        raw = resource_id.bytes
        if len(raw) != 32:
            raise ValueError("resource id proto bytes must be 32 bytes")
        return raw
    if isinstance(resource_id, (bytes, bytearray)):
        raw = bytes(resource_id)
        if len(raw) != 32:
            raise ValueError("resource id must be 32 bytes")
        return raw
    raise TypeError(f"unsupported resource id type: {type(resource_id)!r}")


def _resource_error_code_name(error_code: int) -> str:
    """Render a stable enum name for resource-upload failures."""
    try:
        return session_pb2.ResourceErrorCode.Name(error_code)
    except ValueError:
        return f"RESOURCE_ERROR_{error_code}"


def build_presence_card_root_node(
    width: float = 320.0,
    height: float = 112.0,
) -> types_pb2.NodeProto:
    """Build the presence card background root node."""
    return _make_node(
        {
            "solid_color": {
                "r": 0.10,
                "g": 0.14,
                "b": 0.19,
                "a": 0.72,
                "radius": 12.0,
            },
            "bounds": [0, 0, width, height],
        }
    )


def build_presence_card_sheen_node(width: float = 320.0) -> types_pb2.NodeProto:
    """Build the top sheen used by the glass presence card."""
    return _make_node(
        {
            "solid_color": {
                "r": 0.92,
                "g": 0.96,
                "b": 1.0,
                "a": 0.16,
            },
            "bounds": [0, 0, width, 2],
        }
    )


def build_presence_card_accent_node(
    accent_rgba: tuple[float, float, float, float],
) -> types_pb2.NodeProto:
    """Build the left accent rail used by the glass presence card."""
    return _make_node(
        {
            "solid_color": {
                "r": accent_rgba[0],
                "g": accent_rgba[1],
                "b": accent_rgba[2],
                "a": 0.78,
            },
            "bounds": [0, 18, 4, 76],
        }
    )


def build_presence_card_avatar_plate_node(
    accent_rgba: tuple[float, float, float, float],
) -> types_pb2.NodeProto:
    """Build the translucent plate behind the avatar."""
    return _make_node(
        {
            "solid_color": {
                "r": accent_rgba[0],
                "g": accent_rgba[1],
                "b": accent_rgba[2],
                "a": 0.22,
            },
            "bounds": [24, 28, 56, 56],
        }
    )


def build_presence_card_avatar_node(resource_id: Any) -> types_pb2.NodeProto:
    """Build the avatar node used by Presence Card."""
    return _make_node(
        {
            "static_image": {
                "resource_id": _resource_id_bytes(resource_id),
                "width": 32,
                "height": 32,
                "decoded_bytes": 32 * 32 * 4,
                "fit_mode": types_pb2.IMAGE_FIT_MODE_COVER,
            },
            "bounds": [34, 38, 36, 36],
        }
    )


def build_presence_card_eyebrow_node() -> types_pb2.NodeProto:
    """Build the uppercase metadata label."""
    return _make_node(
        {
            "text_markdown": {
                "content": "RESIDENT AGENT",
                "font_size_px": 11.0,
                "color": [0.72, 0.80, 0.90, 0.82],
            },
            "bounds": [96, 18, 152, 12],
        }
    )


def build_presence_card_name_node(agent_name: str) -> types_pb2.NodeProto:
    """Build the bold display-name line."""
    return _make_node(
        {
            "text_markdown": {
                "content": f"**{agent_name}**",
                "font_size_px": 20.0,
                "color": [0.97, 0.99, 1.0, 1.0],
            },
            "bounds": [96, 34, 152, 26],
        }
    )


def build_presence_card_text_node(
    agent_name: str,
    last_active_label: str = "now",
) -> types_pb2.NodeProto:
    """Build the status line used by Presence Card."""
    del agent_name
    return _make_node(
        {
            "text_markdown": {
                "content": f"Connected • last active {last_active_label}",
                "font_size_px": 13.0,
                "color": [0.82, 0.88, 0.94, 0.92],
            },
            "bounds": [96, 68, 148, 18],
        }
    )


def build_presence_card_chip_bg_node() -> types_pb2.NodeProto:
    """Build the compact time-chip background."""
    return _make_node(
        {
            "solid_color": {
                "r": 0.86,
                "g": 0.92,
                "b": 1.0,
                "a": 0.12,
            },
            "bounds": [224, 20, 44, 22],
        }
    )


def _presence_card_chip_label(last_active_label: str) -> str:
    if last_active_label == "now":
        return "NOW"
    if last_active_label.endswith("s ago"):
        return f"{last_active_label[:-5]}S"
    if last_active_label.endswith("m ago"):
        return f"{last_active_label[:-5]}M"
    return last_active_label.upper()


def build_presence_card_chip_text_node(last_active_label: str = "now") -> types_pb2.NodeProto:
    """Build the compact time-chip label."""
    return _make_node(
        {
            "text_markdown": {
                "content": _presence_card_chip_label(last_active_label),
                "font_size_px": 10.0,
                "color": [0.96, 0.98, 1.0, 0.96],
            },
            "bounds": [224, 21, 44, 20],
        }
    )


def build_presence_card_dismiss_bg_node() -> types_pb2.NodeProto:
    """Build the compact dismiss button background."""
    return _make_node(
        {
            "solid_color": {
                "r": 0.94,
                "g": 0.97,
                "b": 1.0,
                "a": 0.14,
                "radius": 8.0,
            },
            "bounds": [280, 18, 24, 24],
        }
    )


def build_presence_card_dismiss_text_node() -> types_pb2.NodeProto:
    """Build the dismiss button label."""
    return _make_node(
        {
            "text_markdown": {
                "content": "X",
                "font_size_px": 12.0,
                "color": [0.97, 0.99, 1.0, 0.98],
            },
            "bounds": [280, 18, 24, 24],
        }
    )


def build_presence_card_dismiss_hit_region_node() -> types_pb2.NodeProto:
    """Build the dismiss button hit target."""
    return _make_node(
        {
            "hit_region": {
                "interaction_id": "dismiss-card",
                "accepts_focus": True,
                "accepts_pointer": True,
            },
            "bounds": [280, 18, 24, 24],
        }
    )


def build_presence_card_add_node_mutations(
    tile_id: bytes,
    resource_id: Any,
    agent_name: str,
    last_active_label: str = "now",
    accent_rgba: tuple[float, float, float, float] = (66 / 255.0, 133 / 255.0, 244 / 255.0, 1.0),
    card_width: float = 320.0,
    card_height: float = 112.0,
) -> tuple[types_pb2.NodeProto, list[types_pb2.NodeProto], list[types_pb2.MutationProto]]:
    """Build the glass Presence Card tree and its AddNode mutations."""
    root = build_presence_card_root_node(width=card_width, height=card_height)
    child_nodes = [
        build_presence_card_sheen_node(width=card_width),
        build_presence_card_accent_node(accent_rgba),
        build_presence_card_avatar_plate_node(accent_rgba),
        build_presence_card_avatar_node(resource_id),
        build_presence_card_eyebrow_node(),
        build_presence_card_name_node(agent_name),
        build_presence_card_text_node(agent_name, last_active_label),
        build_presence_card_chip_bg_node(),
        build_presence_card_chip_text_node(last_active_label),
        build_presence_card_dismiss_bg_node(),
        build_presence_card_dismiss_text_node(),
        build_presence_card_dismiss_hit_region_node(),
    ]
    mutations = [
        types_pb2.MutationProto(
            set_tile_root=types_pb2.SetTileRootMutation(
                tile_id=tile_id,
                node=root,
            )
        )
    ]
    mutations.extend(
        types_pb2.MutationProto(
            add_node=types_pb2.AddNodeMutation(
                tile_id=tile_id,
                parent_id=root.id,
                node=node,
            )
        )
        for node in child_nodes
    )
    return root, child_nodes, mutations


def build_presence_card_tree_mutations(
    tile_id: bytes,
    resource_id: Any,
    agent_name: str,
    last_active_label: str = "now",
    accent_rgba: tuple[float, float, float, float] = (66 / 255.0, 133 / 255.0, 244 / 255.0, 1.0),
    card_width: float = 320.0,
    card_height: float = 112.0,
) -> tuple[types_pb2.NodeProto, list[types_pb2.NodeProto], list[types_pb2.MutationProto]]:
    """Alias for build_presence_card_add_node_mutations()."""
    return build_presence_card_add_node_mutations(
        tile_id=tile_id,
        resource_id=resource_id,
        agent_name=agent_name,
        last_active_label=last_active_label,
        accent_rgba=accent_rgba,
        card_width=card_width,
        card_height=card_height,
    )


def _make_node(data: dict) -> types_pb2.NodeProto:
    """Build a NodeProto from a dict spec.

    Supported types:
      {"solid_color": {"r": f, "g": f, "b": f, "a": f}, "bounds": [x,y,w,h]}
      {"text_markdown": {"content": str, "font_size_px": f, "color": [r,g,b,a]}, "bounds": [x,y,w,h]}
      {"hit_region": {"interaction_id": str, "accepts_focus": bool, "accepts_pointer": bool}, "bounds": [x,y,w,h]}

    Optional fields:
      {"id": bytes}  # explicit NodeProto.id (otherwise a random UUID is assigned)
    """
    node = types_pb2.NodeProto(id=data.get("id", _uuid_bytes()))

    bounds = data.get("bounds", [0, 0, 100, 100])
    rect = types_pb2.Rect(x=bounds[0], y=bounds[1], width=bounds[2], height=bounds[3])

    if "solid_color" in data:
        c = data["solid_color"]
        node.solid_color.CopyFrom(types_pb2.SolidColorNodeProto(
            color=types_pb2.Rgba(r=c["r"], g=c["g"], b=c["b"], a=c.get("a", 1.0)),
            bounds=rect,
            radius=c.get("radius", -1.0),
        ))
    elif "text_markdown" in data:
        t = data["text_markdown"]
        color = t.get("color", [1.0, 1.0, 1.0, 1.0])
        tm = types_pb2.TextMarkdownNodeProto(
            content=t["content"],
            bounds=rect,
            font_size_px=t.get("font_size_px", 14.0),
            color=types_pb2.Rgba(r=color[0], g=color[1], b=color[2], a=color[3]),
        )
        if "overflow" in t:
            tm.overflow = t["overflow"]
        bg = t.get("background")
        if bg:
            tm.background.CopyFrom(types_pb2.Rgba(r=bg[0], g=bg[1], b=bg[2], a=bg[3]))
        for run in t.get("color_runs", []):
            run_color = run.get("color", color)
            tm.color_runs.append(types_pb2.TextColorRunProto(
                start_byte=run["start_byte"],
                end_byte=run["end_byte"],
                color=types_pb2.Rgba(
                    r=run_color[0],
                    g=run_color[1],
                    b=run_color[2],
                    a=run_color[3],
                ),
            ))
        node.text_markdown.CopyFrom(tm)
    elif "hit_region" in data:
        h = data["hit_region"]
        node.hit_region.CopyFrom(types_pb2.HitRegionNodeProto(
            bounds=rect,
            interaction_id=h.get("interaction_id", ""),
            accepts_focus=h.get("accepts_focus", False),
            accepts_pointer=h.get("accepts_pointer", False),
            auto_capture=h.get("auto_capture", False),
            release_on_up=h.get("release_on_up", False),
            accepts_composer_input=h.get("accepts_composer_input", False),
        ))
    elif "static_image" in data:
        s = data["static_image"]
        node.static_image.CopyFrom(types_pb2.StaticImageNodeProto(
            resource_id=s["resource_id"],
            width=s["width"],
            height=s["height"],
            decoded_bytes=s.get("decoded_bytes", 0),
            fit_mode=s.get("fit_mode", types_pb2.IMAGE_FIT_MODE_UNSPECIFIED),
            bounds=rect,
        ))
    else:
        raise ValueError(f"Unknown node type in: {data}")

    return node


# ---------------------------------------------------------------------------
# HudClient
# ---------------------------------------------------------------------------

class HudClient:
    """Async gRPC client for the tze_hud session protocol."""

    def __init__(
        self,
        target: str,
        psk: str,
        agent_id: str = "user-test-agent",
        capabilities: Optional[list[str]] = None,
        initial_subscriptions: Optional[list[str]] = None,
    ):
        self.target = target
        self.psk = psk
        self.agent_id = agent_id
        self.capabilities = capabilities or [
            "create_tiles",
            "modify_own_tiles",
            "access_input_events",
            "upload_resource",
        ]
        self.initial_subscriptions = initial_subscriptions or ["SCENE_TOPOLOGY"]
        self._channel: Optional[grpc.aio.Channel] = None
        self._stream = None
        self._seq = 0
        self._server_seq = 0
        self.session_id: Optional[bytes] = None
        self.namespace: Optional[str] = None
        self.heartbeat_interval_ms: Optional[int] = None
        # TTL (ms) granted by the most recent request_lease/renew_lease. Long-lived
        # callers (soak, sustained streaming) read this to schedule renewals before
        # the lease expires — otherwise the runtime rejects mutations with
        # MUTATION_REJECTED / "lease expired" mid-run (hud-hk8kl).
        self.last_granted_lease_ttl_ms: int = 0
        self.granted_capabilities: list[str] = []
        # Runtime-resolved portal design tokens from the handshake (hud-16um0);
        # populated by connect(), empty when the runtime does not expose them.
        self.resolved_portal_tokens: dict[str, str] = {}
        self.scene_snapshot_json: Optional[str] = None
        self.scene_display_area: Optional[tuple[float, float]] = None
        # Optional minimum spacing between mutation-batch sends. The default
        # keeps existing callers unpaced; portal evidence enables this to stay
        # inside the production profile's per-agent update-rate envelope.
        self._min_batch_interval_s: float = 0.0
        self._last_batch_send_mono: float = 0.0
        self._batch_pacing_lock = asyncio.Lock()
        self._response_queue: asyncio.Queue = asyncio.Queue()
        self._deferred_responses: list[Any] = []
        self._response_wait_lock = asyncio.Lock()
        self._event_queue: asyncio.Queue = asyncio.Queue()
        self._reader_task: Optional[asyncio.Task] = None
        self._send_queue: Optional[asyncio.Queue] = None
        self._transport_closed = False
        self._session_close_sent = False
        # ── Batch-correlated present acknowledgment (hud-vjlqh / hud-91uu6) ────
        # FramePresented events (ServerMessage field 52) delivered to sessions
        # subscribed to TELEMETRY_FRAMES pair one or more MutationBatch.batch_ids
        # with the wall-clock time their composited frame was presented on
        # screen. We capture them off the read loop (rather than the generic
        # response queue) so a present-latency consumer can correlate a batch it
        # sent to the true present time, distinct from the transport-RTT proxy.
        self._frame_presented_events: list[Any] = []
        # batch_id (bytes) → send wall-clock (UTC µs, _now_wall_us domain — the
        # same domain as FramePresented.present_wall_us). Bounded to the most
        # recent sends so a long run does not grow this unbounded.
        self._batch_send_wall_us: "dict[bytes, int]" = {}
        self._batch_send_order: list[bytes] = []
        self.last_mutation_batch_id: Optional[bytes] = None
        # ── Bounded mutation-ack retry policy (hud-n5bqp) ─────────────────────
        # A single transient mutation-ack blip (a momentary tailnet stall or
        # runtime pause) should not abort a full-duration soak. Long-lived
        # callers enable a small retry budget via configure_mutation_retry();
        # the default (0 retries) preserves fail-fast behavior for every other
        # caller. submit_mutation_batch reads these when its own retry args are
        # left unset.
        self._mutation_retry_budget: int = 0
        self._mutation_retry_backoff_s: float = 0.5

    async def __aenter__(self):
        await self.connect()
        return self

    async def __aexit__(self, *exc):
        await self.close()

    def _next_seq(self) -> int:
        self._seq += 1
        return self._seq

    async def connect(self):
        """Open channel, start session stream, perform handshake."""
        self._transport_closed = False
        self._session_close_sent = False
        self._response_queue = asyncio.Queue()
        self._deferred_responses = []
        self._channel = grpc.aio.insecure_channel(self.target)
        stub = session_pb2_grpc.HudSessionStub(self._channel)

        # Build the outbound request iterator — we'll feed messages via a queue.
        self._send_queue = asyncio.Queue()
        self._stream = stub.Session(self._request_iterator())

        # Send SessionInit
        init_msg = session_pb2.ClientMessage(
            sequence=self._next_seq(),
            timestamp_wall_us=_now_wall_us(),
            session_init=session_pb2.SessionInit(
                agent_id=self.agent_id,
                agent_display_name=self.agent_id,
                auth_credential=session_pb2.AuthCredential(
                    pre_shared_key=session_pb2.PreSharedKeyCredential(key=self.psk),
                ),
                requested_capabilities=self.capabilities,
                initial_subscriptions=self.initial_subscriptions,
                agent_timestamp_wall_us=_now_wall_us(),
                min_protocol_version=1000,
                max_protocol_version=1000,
            ),
        )
        await self._send_queue.put(init_msg)

        # Start background reader
        self._reader_task = asyncio.create_task(self._read_loop())

        # Wait for SessionEstablished
        resp = await self._wait_for("session_established", timeout=5.0)
        est = resp.session_established
        self.session_id = est.session_id
        self.namespace = est.namespace
        self.heartbeat_interval_ms = est.heartbeat_interval_ms
        self.granted_capabilities = list(est.granted_capabilities)
        # Runtime-resolved portal design tokens (hud-16um0). When the runtime
        # exposes them, this is the ACTIVE profile's fully-resolved portal token
        # map ({key: value_string}); empty when the runtime predates the field,
        # in which case a client falls back to its local default mirror.
        if est.HasField("portal_part_tokens"):
            self.resolved_portal_tokens = dict(est.portal_part_tokens.tokens)
        else:
            self.resolved_portal_tokens = {}
        print(f"  [grpc] Session established: namespace={self.namespace}, "
              f"caps={self.granted_capabilities}", flush=True)

        snapshot_resp = await self._wait_for("scene_snapshot", timeout=5.0)
        self.scene_snapshot_json = snapshot_resp.scene_snapshot.snapshot_json
        self.scene_display_area = self._extract_scene_display_area(self.scene_snapshot_json)
        if self.scene_display_area is not None:
            w, h = self.scene_display_area
            print(f"  [grpc] Scene display area: {w:g}x{h:g}", flush=True)

    @staticmethod
    def _extract_scene_display_area(snapshot_json: str) -> Optional[tuple[float, float]]:
        try:
            snapshot = json.loads(snapshot_json)
        except json.JSONDecodeError:
            return None
        display_area = snapshot.get("display_area") or snapshot.get("displayArea")
        if not isinstance(display_area, dict):
            return None
        width = display_area.get("width")
        height = display_area.get("height")
        if not isinstance(width, (int, float)) or not isinstance(height, (int, float)):
            return None
        if width <= 0 or height <= 0:
            return None
        return float(width), float(height)

    async def _request_iterator(self):
        """Async generator that yields ClientMessages from the send queue."""
        while True:
            msg = await self._send_queue.get()
            if msg is None:
                return
            yield msg

    async def _read_loop(self):
        """Background task that reads ServerMessages and dispatches them."""
        try:
            async for msg in self._stream:
                self._server_seq = msg.sequence
                which = msg.WhichOneof("payload")
                if which == "event_batch":
                    await self._event_queue.put(msg.event_batch)
                elif which == "frame_presented":
                    # Present-ack (hud-vjlqh): capture off to the side so it does
                    # not interfere with the request/response matching in
                    # _wait_for. State-stream/droppable — a plain list is fine.
                    self._frame_presented_events.append(msg.frame_presented)
                else:
                    await self._response_queue.put(msg)
        except grpc.aio.AioRpcError as e:
            if e.code() != grpc.StatusCode.CANCELLED:
                print(f"  [grpc] Stream error: {e}", flush=True)
        except Exception as e:
            print(f"  [grpc] Reader error: {e}", flush=True)

    async def _wait_for(
        self,
        payload_name: str,
        timeout: float = 10.0,
        matcher: Optional[Callable[[Any], bool]] = None,
    ) -> Any:
        """Wait for a ServerMessage with the given payload type."""
        deadline = time.monotonic() + timeout
        async with self._response_wait_lock:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"Timed out waiting for {payload_name}")
                msg = self._pop_deferred_response(
                    lambda candidate: self._matches_payload_wait(
                        candidate,
                        payload_name,
                        matcher,
                    )
                )
                if msg is None:
                    try:
                        msg = await asyncio.wait_for(
                            self._response_queue.get(), timeout=remaining
                        )
                    except asyncio.TimeoutError:
                        raise TimeoutError(f"Timed out waiting for {payload_name}")

                which = msg.WhichOneof("payload")
                if which == payload_name and (matcher is None or matcher(msg)):
                    return msg
                if which == "session_error":
                    raise RuntimeError(
                        f"Session error: {msg.session_error.code} — "
                        f"{msg.session_error.message} "
                        f"(hint: {msg.session_error.hint})"
                    )
                self._deferred_responses.append(msg)

    async def wait_for(self, payload_name: str, timeout: float = 10.0) -> Any:
        """Public wrapper for waiting on a specific server payload."""
        return await self._wait_for(payload_name, timeout)

    async def _send(self, **payload_kwargs) -> int:
        """Send a ClientMessage with the given payload field and return sequence."""
        sequence = self._next_seq()
        msg = session_pb2.ClientMessage(
            sequence=sequence,
            timestamp_wall_us=_now_wall_us(),
            **payload_kwargs,
        )
        if self._send_queue is None:
            raise RuntimeError("client transport has not been initialized")
        # Record the send wall-clock of every mutation batch so a present-latency
        # consumer can correlate the batch to its FramePresented (hud-vjlqh). The
        # timestamp is UTC µs (same domain as FramePresented.present_wall_us).
        batch = payload_kwargs.get("mutation_batch")
        if batch is not None and batch.batch_id:
            self._record_batch_send(bytes(batch.batch_id), msg.timestamp_wall_us)
        await self._send_queue.put(msg)
        return sequence

    # ── Present-ack correlation (hud-vjlqh / hud-91uu6) ───────────────────────
    _MAX_TRACKED_BATCH_SENDS = 512

    def _record_batch_send(self, batch_id: bytes, send_wall_us: int) -> None:
        """Remember a mutation batch's send wall-clock, bounded to the most
        recent sends so a long run does not grow the map unbounded."""
        if batch_id not in self._batch_send_wall_us:
            self._batch_send_order.append(batch_id)
            if len(self._batch_send_order) > self._MAX_TRACKED_BATCH_SENDS:
                evicted = self._batch_send_order.pop(0)
                self._batch_send_wall_us.pop(evicted, None)
        self._batch_send_wall_us[batch_id] = send_wall_us
        self.last_mutation_batch_id = batch_id

    def batch_send_wall_us(self, batch_id: bytes) -> Optional[int]:
        """Send wall-clock (UTC µs) recorded for a mutation batch, if tracked."""
        return self._batch_send_wall_us.get(bytes(batch_id))

    def present_wall_us_for_batch(self, batch_id: bytes) -> Optional[int]:
        """Return the present wall-clock (UTC µs) of the FramePresented whose
        batch_ids contains ``batch_id``, or None if not yet observed. When more
        than one frame carried the batch (should not happen — a batch presents
        once) the earliest present is returned."""
        target = bytes(batch_id)
        for event in self._frame_presented_events:
            if any(bytes(b) == target for b in event.batch_ids):
                return event.present_wall_us
        return None

    async def wait_for_frame_presented(
        self, batch_id: bytes, timeout: float = 2.0, poll_s: float = 0.01,
    ) -> Optional[int]:
        """Await the present wall-clock (UTC µs) for ``batch_id``.

        Returns None if no matching FramePresented arrives within ``timeout``.
        A None result is expected and non-fatal: the runtime only emits
        FramePresented from the headless present path today (windowed emission is
        deferred, hud-4va6q), and delivery further requires the session to hold
        the read_telemetry capability and a TELEMETRY_FRAMES subscription."""
        deadline = time.monotonic() + timeout
        while True:
            present = self.present_wall_us_for_batch(batch_id)
            if present is not None:
                return present
            if time.monotonic() >= deadline:
                return None
            await asyncio.sleep(poll_s)

    async def _shutdown_transport(self):
        """Cancel background tasks and close the gRPC channel."""
        if self._transport_closed:
            return
        self._transport_closed = True
        if self._send_queue is not None:
            await self._send_queue.put(None)
        if self._reader_task:
            self._reader_task.cancel()
            try:
                await self._reader_task
            except (asyncio.CancelledError, Exception):
                pass
        if self._channel:
            await self._channel.close()

    async def session_close(self, reason: str = "test complete", expect_resume: bool = False):
        """Request a graceful session close, but leave the transport open."""
        if self._session_close_sent or self._transport_closed:
            return
        await self._send(
            session_close=session_pb2.SessionClose(
                reason=reason,
                expect_resume=expect_resume,
            )
        )
        self._session_close_sent = True

    async def drop_connection(self):
        """Unconditionally close the underlying gRPC transport without SessionClose."""
        await self._shutdown_transport()

    async def disconnect(
        self,
        graceful: bool = True,
        reason: str = "test complete",
        expect_resume: bool = False,
    ):
        """Disconnect the session by graceful close or by dropping transport."""
        if graceful:
            await self.session_close(reason=reason, expect_resume=expect_resume)
            await self._shutdown_transport()
        else:
            await self.drop_connection()

    async def release_lease(self, lease_id: bytes):
        """Release a lease, removing all its tiles immediately."""
        await self._send(
            lease_release=session_pb2.LeaseRelease(lease_id=lease_id)
        )
        await self._wait_for("lease_response", timeout=5.0)
        print("  [grpc] Lease released", flush=True)

    async def close(self, reason: str = "test complete", expect_resume: bool = False):
        """Gracefully close the session."""
        try:
            if not self._session_close_sent and not self._transport_closed:
                await self.session_close(reason=reason, expect_resume=expect_resume)
        except Exception:
            pass
        await self._shutdown_transport()

    # ─── Lease management ─────────────────────────────────────────────────

    async def request_lease(
        self, ttl_ms: int = 60000, priority: int = 2
    ) -> bytes:
        """Request a lease and return the granted lease_id."""
        await self._send(
            lease_request=session_pb2.LeaseRequest(
                ttl_ms=ttl_ms,
                capabilities=self.capabilities,
                lease_priority=priority,
            )
        )
        resp = await self._wait_for("lease_response", timeout=5.0)
        lr = resp.lease_response
        if not lr.granted:
            deny_reason = getattr(lr, "deny_reason", "") or "unspecified denial"
            deny_code = getattr(lr, "deny_code", "")
            if deny_code:
                raise RuntimeError(f"Lease denied [{deny_code}]: {deny_reason}")
            raise RuntimeError(f"Lease denied: {deny_reason}")
        self.last_granted_lease_ttl_ms = lr.granted_ttl_ms
        print(f"  [grpc] Lease granted: ttl={lr.granted_ttl_ms}ms, "
              f"priority={lr.granted_priority}", flush=True)
        return lr.lease_id

    async def renew_lease(self, lease_id: bytes, new_ttl_ms: int = 0) -> int:
        """Renew an existing lease's TTL and return the newly granted TTL (ms).

        ``new_ttl_ms=0`` asks the runtime to re-grant the lease's original TTL
        (see ``LeaseRenew.new_ttl_ms`` in session.proto). Sustained callers renew
        before ~75% of the granted TTL elapses so the lease never expires mid-run.
        """
        await self._send(
            lease_renew=session_pb2.LeaseRenew(
                lease_id=lease_id,
                new_ttl_ms=new_ttl_ms,
            )
        )
        resp = await self._wait_for("lease_response", timeout=5.0)
        lr = resp.lease_response
        if not lr.granted:
            deny_reason = getattr(lr, "deny_reason", "") or "unspecified denial"
            deny_code = getattr(lr, "deny_code", "")
            if deny_code:
                raise RuntimeError(f"Lease renew denied [{deny_code}]: {deny_reason}")
            raise RuntimeError(f"Lease renew denied: {deny_reason}")
        self.last_granted_lease_ttl_ms = lr.granted_ttl_ms
        print(f"  [grpc] Lease renewed: ttl={lr.granted_ttl_ms}ms", flush=True)
        return lr.granted_ttl_ms

    async def _await_resource_upload_result(
        self,
        request_sequence: int,
        timeout: float = 10.0,
    ) -> session_pb2.ResourceStored:
        """Wait for ResourceStored/ResourceErrorResponse correlated to a request sequence."""
        deadline = time.monotonic() + timeout
        async with self._response_wait_lock:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"Timed out waiting for resource upload result "
                        f"(request_sequence={request_sequence})"
                    )
                msg = self._pop_deferred_response(
                    lambda candidate: self._matches_resource_upload_wait(
                        candidate, request_sequence
                    )
                )
                if msg is None:
                    try:
                        msg = await asyncio.wait_for(
                            self._response_queue.get(), timeout=remaining
                        )
                    except asyncio.TimeoutError:
                        raise TimeoutError(
                            f"Timed out waiting for resource upload result "
                            f"(request_sequence={request_sequence})"
                        )

                if not self._matches_resource_upload_wait(msg, request_sequence):
                    self._deferred_responses.append(msg)
                    continue

                which = msg.WhichOneof("payload")
                if which == "resource_stored":
                    return msg.resource_stored
                if which == "resource_error_response":
                    err = msg.resource_error_response
                    code_name = _resource_error_code_name(err.error_code)
                    raise RuntimeError(
                        f"Resource upload failed [{code_name}]: {err.message} "
                        f"(context: {err.context}, hint: {err.hint})"
                    )
                if which == "session_error":
                    raise RuntimeError(
                        f"Session error while waiting for upload result: "
                        f"{msg.session_error.code} — {msg.session_error.message} "
                        f"(hint: {msg.session_error.hint})"
                    )
                if which == "runtime_error":
                    raise RuntimeError(
                        f"Runtime error while waiting for upload result: "
                        f"{msg.runtime_error.error_code} — {msg.runtime_error.message}"
                    )

    def _pop_deferred_response(
        self,
        matcher: Callable[[Any], bool],
    ) -> Any | None:
        """Return the first deferred response matching matcher, if any."""
        for index, msg in enumerate(self._deferred_responses):
            if matcher(msg):
                return self._deferred_responses.pop(index)
        return None

    @staticmethod
    def _matches_payload_wait(
        msg: Any,
        payload_name: str,
        matcher: Optional[Callable[[Any], bool]],
    ) -> bool:
        which = msg.WhichOneof("payload")
        if which == "session_error":
            return True
        if which != payload_name:
            return False
        return matcher is None or matcher(msg)

    @staticmethod
    def _matches_resource_upload_wait(msg: Any, request_sequence: int) -> bool:
        which = msg.WhichOneof("payload")
        if which == "resource_stored":
            return msg.resource_stored.request_sequence == request_sequence
        if which == "resource_error_response":
            return msg.resource_error_response.request_sequence == request_sequence
        return which in {"session_error", "runtime_error"}

    async def upload_png_resource(
        self,
        png_bytes: bytes,
        *,
        timeout: float = 10.0,
    ) -> bytes:
        """Upload a PNG via resident ResourceUploadStart and return ResourceId bytes."""
        if len(png_bytes) > 64 * 1024:
            raise ValueError(
                "PNG exceeds 64 KiB inline upload limit; chunked upload is not implemented"
            )
        width, height = _png_image_size(png_bytes)
        request_sequence = await self._send(
            resource_upload_start=session_pb2.ResourceUploadStart(
                expected_hash=_blake3_digest_bytes(png_bytes),
                resource_type=session_pb2.IMAGE_PNG,
                total_size_bytes=len(png_bytes),
                metadata=session_pb2.ResourceMetadata(width=width, height=height),
                inline_data=png_bytes,
            )
        )
        stored = await self._await_resource_upload_result(
            request_sequence=request_sequence,
            timeout=timeout,
        )
        resource_id = _resource_id_bytes(stored.resource_id)
        print(
            f"  [grpc] Resource uploaded: {resource_id.hex()[:16]}... "
            f"bytes={len(png_bytes)} dedup={stored.was_deduplicated}",
            flush=True,
        )
        return resource_id

    async def upload_avatar_png(
        self,
        png_bytes: bytes,
        *,
        timeout: float = 10.0,
    ) -> bytes:
        """Upload a 32x32 PNG avatar via resident upload flow and return ResourceId."""
        if _png_image_size(png_bytes) != (32, 32):
            raise ValueError("avatar PNG must be exactly 32x32 pixels")
        return await self.upload_png_resource(png_bytes, timeout=timeout)

    def configure_batch_pacing(self, min_interval_s: float) -> None:
        """Set an opt-in minimum spacing between mutation-batch sends."""
        self._min_batch_interval_s = max(0.0, float(min_interval_s))

    async def _pace_batch_send(self) -> None:
        """Wait only when opt-in batch pacing requires it."""
        if self._min_batch_interval_s <= 0.0:
            return
        async with self._batch_pacing_lock:
            if self._min_batch_interval_s <= 0.0:
                return
            wait = (
                self._last_batch_send_mono
                + self._min_batch_interval_s
                - time.monotonic()
            )
            if wait > 0.0:
                await asyncio.sleep(wait)
            self._last_batch_send_mono = time.monotonic()

    async def apply_mutations(
        self,
        lease_id: bytes,
        mutations: list[types_pb2.MutationProto],
    ) -> session_pb2.MutationResult:
        """Submit a raw mutation batch and return the acknowledged result."""
        batch_id = _uuid_bytes()
        await self._pace_batch_send()
        await self._send(
            mutation_batch=session_pb2.MutationBatch(
                batch_id=batch_id,
                lease_id=lease_id,
                mutations=mutations,
            )
        )
        resp = await self._wait_for("mutation_result", timeout=5.0)
        mr = resp.mutation_result
        if not mr.accepted:
            raise RuntimeError(
                f"Mutation batch rejected: {mr.error_code} — {mr.error_message}"
            )
        return mr

    # ─── Tile operations ──────────────────────────────────────────────────

    def configure_mutation_retry(self, retries: int, backoff_s: float = 0.5) -> None:
        """Set the per-client bounded-retry policy for submit_mutation_batch.

        Long-lived callers (the portal soak) enable a small retry budget so a
        single transient mutation-ack timeout — a momentary tailnet stall or
        runtime pause — does not abort a full-duration run. The default (0
        retries) preserves fail-fast behavior for every other caller.
        ``retries`` is the number of resubmits attempted *after* the first
        send; ``backoff_s`` is the base delay, applied with exponential
        growth between attempts.
        """
        self._mutation_retry_budget = max(0, int(retries))
        self._mutation_retry_backoff_s = max(0.0, float(backoff_s))

    async def submit_mutation_batch(
        self,
        lease_id: bytes,
        mutations: list[types_pb2.MutationProto],
        timeout: float = 5.0,
        retries: Optional[int] = None,
        retry_backoff_s: Optional[float] = None,
    ) -> session_pb2.MutationResult:
        """Submit a mutation batch and return the accepted result.

        A mutation-ack (``mutation_result``) that fails to arrive within
        ``timeout`` raises ``TimeoutError``. For sustained runs (the portal
        soak) a single transient ack blip should not abort the whole run, so
        an optional bounded retry resubmits the batch — with a fresh
        ``batch_id`` — up to ``retries`` times, sleeping ``retry_backoff_s``
        (exponential) between attempts. Every retry is logged visibly so a
        *recurring* ack wall is still detectable rather than silently
        swallowed; once the budget is exhausted the ``TimeoutError``
        propagates and the caller still aborts. Rejections (``RuntimeError``)
        are never retried — only ack timeouts. ``retries`` and
        ``retry_backoff_s`` fall back to the per-client policy set by
        ``configure_mutation_retry`` (default: no retries).
        """
        budget = self._mutation_retry_budget if retries is None else max(0, retries)
        backoff_s = (
            self._mutation_retry_backoff_s
            if retry_backoff_s is None
            else max(0.0, retry_backoff_s)
        )
        attempt = 0
        while True:
            try:
                return await self._submit_mutation_batch_once(
                    lease_id, mutations, timeout
                )
            except TimeoutError:
                if attempt >= budget:
                    raise
                attempt += 1
                delay = backoff_s * (2 ** (attempt - 1)) if backoff_s > 0 else 0.0
                # Visible so a recurring ack wall is still detectable — the
                # soak must not silently swallow a systemic stall (hud-n5bqp).
                print(
                    f"  [grpc] mutation-ack timeout after {timeout:.1f}s "
                    f"(retry {attempt}/{budget}); resubmitting batch after "
                    f"{delay:.2f}s backoff",
                    flush=True,
                )
                if delay > 0:
                    await asyncio.sleep(delay)

    async def _submit_mutation_batch_once(
        self,
        lease_id: bytes,
        mutations: list[types_pb2.MutationProto],
        timeout: float,
    ) -> session_pb2.MutationResult:
        """Submit one mutation batch and wait for its ack (single attempt).

        Raises ``TimeoutError`` if no matching ``mutation_result`` arrives
        within ``timeout``; the bounded-retry wrapper in
        ``submit_mutation_batch`` decides whether to resubmit.
        """
        batch_id = _uuid_bytes()
        await self._pace_batch_send()
        await self._send(
            mutation_batch=session_pb2.MutationBatch(
                batch_id=batch_id,
                lease_id=lease_id,
                mutations=mutations,
            )
        )
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    "Timed out waiting for mutation_result for submitted batch"
                )
            resp = await self._wait_for("mutation_result", timeout=remaining)
            mr = resp.mutation_result
            if mr.batch_id != batch_id:
                continue
            if not mr.accepted:
                raise RuntimeError(
                    f"Mutation batch rejected: {mr.error_code} — {mr.error_message}"
                )
            return mr

    async def create_tile(
        self,
        lease_id: bytes,
        tab_id: Optional[bytes] = None,
        x: float = 50,
        y: float = 50,
        w: float = 400,
        h: float = 300,
        z_order: int = 100,
    ) -> bytes:
        """Create a tile and return its SceneId."""
        mr = await self.submit_mutation_batch(
            lease_id,
            [
                types_pb2.MutationProto(
                    create_tile=types_pb2.CreateTileMutation(
                        tab_id=tab_id or b"",
                        bounds=types_pb2.Rect(x=x, y=y, width=w, height=h),
                        z_order=z_order,
                    )
                )
            ],
        )
        tile_id = mr.created_ids[0]
        print(f"  [grpc] Tile created: {tile_id.hex()[:16]}...", flush=True)
        return tile_id

    async def update_tile_opacity(
        self,
        lease_id: bytes,
        tile_id: bytes,
        opacity: float,
    ) -> None:
        """Update a tile's opacity."""
        await self.submit_mutation_batch(
            lease_id,
            [
                types_pb2.MutationProto(
                    update_tile_opacity=types_pb2.UpdateTileOpacityMutation(
                        tile_id=tile_id,
                        opacity=opacity,
                    )
                )
            ],
        )
        print(f"  [grpc] Tile opacity set to {opacity}", flush=True)

    async def update_tile_input_mode(
        self,
        lease_id: bytes,
        tile_id: bytes,
        input_mode: types_pb2.TileInputModeProto,
    ) -> None:
        """Update a tile's input mode."""
        await self.submit_mutation_batch(
            lease_id,
            [
                types_pb2.MutationProto(
                    update_tile_input_mode=types_pb2.UpdateTileInputModeMutation(
                        tile_id=tile_id,
                        input_mode=input_mode,
                    )
                )
            ],
        )
        print("  [grpc] Tile input mode set", flush=True)

    async def set_tile_root(
        self,
        lease_id: bytes,
        tile_id: bytes,
        node_spec: Any,
    ):
        """Set a tile's root node from a dict spec or NodeProto."""
        node = node_spec if isinstance(node_spec, types_pb2.NodeProto) else _make_node(node_spec)
        await self.submit_mutation_batch(
            lease_id,
            [
                types_pb2.MutationProto(
                    set_tile_root=types_pb2.SetTileRootMutation(
                        tile_id=tile_id,
                        node=node,
                    )
                )
            ],
        )
        print("  [grpc] Tile root set", flush=True)

    async def add_node(
        self,
        lease_id: bytes,
        tile_id: bytes,
        node_spec: Any,
        parent_id: Optional[bytes] = None,
    ) -> bytes:
        """Add a child node to a tile and return the created node id."""
        node = node_spec if isinstance(node_spec, types_pb2.NodeProto) else _make_node(node_spec)
        mr = await self.submit_mutation_batch(
            lease_id,
            [
                types_pb2.MutationProto(
                    add_node=types_pb2.AddNodeMutation(
                        tile_id=tile_id,
                        parent_id=parent_id or b"",
                        node=node,
                    )
                )
            ],
        )
        node_id = mr.created_ids[0]
        print(f"  [grpc] Node added: {node_id.hex()[:16]}...", flush=True)
        return node_id

    async def update_node_content(
        self,
        lease_id: bytes,
        tile_id: bytes,
        node_id: bytes,
        node_spec: Any,
    ) -> None:
        """Replace a node's content in place."""
        node = node_spec if isinstance(node_spec, types_pb2.NodeProto) else _make_node(node_spec)
        mutation = types_pb2.UpdateNodeContentMutation(tile_id=tile_id, node_id=node_id)
        if node.HasField("solid_color"):
            mutation.solid_color.CopyFrom(node.solid_color)
        elif node.HasField("text_markdown"):
            mutation.text_markdown.CopyFrom(node.text_markdown)
        elif node.HasField("hit_region"):
            mutation.hit_region.CopyFrom(node.hit_region)
        elif node.HasField("static_image"):
            mutation.static_image.CopyFrom(node.static_image)
        await self.submit_mutation_batch(
            lease_id,
            [types_pb2.MutationProto(update_node_content=mutation)],
        )

    async def create_presence_card_tile(
        self,
        lease_id: bytes,
        tab_id: Optional[bytes],
        agent_name: str,
        avatar_resource_id: Any,
        *,
        accent_rgba: tuple[float, float, float, float] = (66 / 255.0, 133 / 255.0, 244 / 255.0, 1.0),
        x: float = 24.0,
        y: float = 0.0,
        w: float = 320.0,
        h: float = 112.0,
        z_order: int = 100,
    ) -> bytes:
        """Create a full Presence Card tile and return the tile id."""
        tile_id = await self.create_tile(
            lease_id,
            tab_id=tab_id,
            x=x,
            y=y,
            w=w,
            h=h,
            z_order=z_order,
        )
        await self.update_tile_opacity(lease_id, tile_id, 1.0)
        await self.update_tile_input_mode(
            lease_id,
            tile_id,
            types_pb2.TILE_INPUT_MODE_CAPTURE,
        )
        root, children, _ = build_presence_card_add_node_mutations(
            tile_id=tile_id,
            resource_id=avatar_resource_id,
            agent_name=agent_name,
            accent_rgba=accent_rgba,
            card_width=w,
            card_height=h,
        )
        await self.set_tile_root(lease_id, tile_id, root)
        for node in children:
            await self.add_node(lease_id, tile_id, node, parent_id=root.id)
        return tile_id

    async def send_heartbeat(self):
        """Send a keepalive heartbeat."""
        await self._send(heartbeat=session_pb2.Heartbeat())

    async def wait_for_click(
        self,
        interaction_id: str,
        timeout: Optional[float] = None,
    ):
        """Wait until an INPUT_EVENTS batch carries a matching ClickEvent."""
        while True:
            if timeout is None:
                batch = await self._event_queue.get()
            else:
                batch = await asyncio.wait_for(self._event_queue.get(), timeout=timeout)
            for envelope in batch.events:
                if envelope.WhichOneof("event") != "click":
                    continue
                if envelope.click.interaction_id == interaction_id:
                    return envelope.click


# ---------------------------------------------------------------------------
# Quick self-test
# ---------------------------------------------------------------------------

async def _self_test():
    """Connect, create a Presence Card tile, hold for 5s, close."""
    import argparse
    parser = argparse.ArgumentParser(description="gRPC client self-test")
    parser.add_argument("--target", default="windows-host.example:50051")
    parser.add_argument("--psk", default=os.getenv("MCP_TEST_PSK", "tze-hud-key"))
    args = parser.parse_args()

    print(f"Connecting to {args.target}...", flush=True)
    async with HudClient(args.target, psk=args.psk, agent_id="grpc-self-test") as client:
        lease_id = await client.request_lease(ttl_ms=30000)
        avatar_png = make_avatar_png((255, 0, 0))
        avatar_resource_id = await client.upload_avatar_png(avatar_png)
        tile_id = await client.create_presence_card_tile(
            lease_id,
            tab_id=None,
            agent_name="grpc-self-test",
            avatar_resource_id=avatar_resource_id,
            x=500,
            y=300,
            w=320,
            h=112,
            z_order=100,
        )
        print(
            f"  Presence card tile {tile_id.hex()[:16]}... visible for 10 seconds...",
            flush=True,
        )
        await asyncio.sleep(10)
        await client.release_lease(lease_id)
        print("  Closing session.", flush=True)


if __name__ == "__main__":
    asyncio.run(_self_test())
