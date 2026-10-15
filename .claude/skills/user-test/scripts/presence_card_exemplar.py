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
"""Run three Presence Card streams under one paired HUD identity.

Display labels alpha/beta/gamma are cosmetic. Each stream claims a filled tile,
updates acknowledged text nodes, then gamma drops its transport to demonstrate
orphan grace. JSON events report protocol evidence; pixels need owner observation.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import hud_env
from hud_grpc_client import (
    HudClient, PRESENCE_CARD_ROLES, build_presence_card_text_node,
    build_presence_card_chip_text_node, make_avatar_png,
)

DEFAULT_TRANSCRIPT_PATH = "test_results/presence-card-latest.json"
DISMISS_INTERACTION_ID = "dismiss-card"


@dataclass(frozen=True)
class AgentSpec:
    name: str
    rgb: tuple[int, int, int]


# Card labels, not identities. Every stream is authenticated by the same PSK.
AGENTS = [
    AgentSpec("agent-alpha", (66, 133, 244)),
    AgentSpec("agent-beta", (52, 168, 83)),
    AgentSpec("agent-gamma", (251, 188, 4)),
]


@dataclass
class AgentRuntime:
    spec: AgentSpec
    client: HudClient
    lease_id: bytes
    tile_id: bytes
    node_ids: dict[str, bytes]
    heartbeat_task: asyncio.Task | None = None
    dismiss_task: asyncio.Task | None = None
    dismissed_by_user: bool = False
    dropped: bool = False


def format_last_active(elapsed_seconds: int) -> str:
    if elapsed_seconds <= 0:
        return "now"
    if elapsed_seconds < 60:
        return f"{elapsed_seconds}s ago"
    return f"{elapsed_seconds // 60}m ago"


def build_step_plan(update_wait_s: int, orphan_grace_s: int) -> list[dict[str, str]]:
    return [
        {"code": "create", "action": "claim 3 filled cards under the paired identity",
         "expected_visual": "3 stacked bottom-left cards, each with name, body and avatar"},
        {"code": "update", "action": f"wait {update_wait_s}s, then update acknowledged body/chip IDs",
         "expected_visual": f"all cards show last active {format_last_active(update_wait_s)}"},
        {"code": "disconnect", "action": "drop agent-gamma transport without SessionClose or Clear",
         "expected_visual": "alpha and beta remain connected"},
        {"code": "orphan_observe", "action": "pause for owner observation after the hard drop",
         "expected_visual": "disconnection badge on gamma only; text and avatar remain"},
        {"code": "cleanup_wait", "action": f"wait {orphan_grace_s}s for configured orphan grace",
         "expected_visual": "gamma reclaimed; alpha and beta remain at their original positions"},
        {"code": "final_state", "action": "clear this run's survivors, then record protocol completion",
         "expected_visual": "2 surviving cards before cleanup; this run's cards gone afterward"},
    ]


def emit_step_event(transcript: list[dict[str, Any]], status: str, step: dict[str, str], **extra: Any) -> None:
    event = {"ts_wall": int(time.time()), "status": status, **step,
             "visual_observation": "unverified", **extra}
    transcript.append(event)
    print(json.dumps(event, sort_keys=True), flush=True)


def write_transcript(path: str, payload: dict[str, Any]) -> None:
    out = Path(path)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def _scene_id(raw: bytes) -> str:
    return str(uuid.UUID(bytes=raw))


def agent_evidence(agent: AgentRuntime) -> dict[str, Any]:
    return {
        "card_label": agent.spec.name, "namespace": agent.client.namespace,
        "lease_id": _scene_id(agent.lease_id), "tile_id": _scene_id(agent.tile_id),
        "node_ids": {role: _scene_id(node_id) for role, node_id in agent.node_ids.items()},
        "display_area": agent.client.scene_display_area,
    }


async def heartbeat_loop(client: HudClient, interval_ms: int) -> None:
    while True:
        await asyncio.sleep(max(1.0, interval_ms / 2000.0))
        await client.send_heartbeat()


async def start_agent(target: str, psk: str, spec: AgentSpec, agent_id: str = "") -> AgentRuntime:
    client = HudClient(target, psk=psk, agent_id=agent_id,
                       initial_subscriptions=["SCENE_TOPOLOGY", "INPUT_EVENTS"])
    try:
        await client.connect()
        avatar_resource_id = await client.upload_avatar_png(make_avatar_png(spec.rgb))
        claimed = await client.create_presence_card_tile(
            agent_name=spec.name, avatar_resource_id=avatar_resource_id,
            accent_rgba=tuple(channel / 255.0 for channel in spec.rgb) + (1.0,),
            ttl_ms=120_000,
        )
        agent = AgentRuntime(spec, client, claimed.lease_id, claimed.tile_id,
                             dict(zip(PRESENCE_CARD_ROLES, claimed.node_ids, strict=True)))
        agent.heartbeat_task = asyncio.create_task(heartbeat_loop(client, client.heartbeat_interval_ms or 5000))
        agent.dismiss_task = asyncio.create_task(watch_for_dismiss(agent))
        return agent
    except BaseException:
        await client.close(reason="presence-card setup failed")
        raise


async def update_agent_card(agent: AgentRuntime, elapsed_seconds: int) -> dict[str, Any]:
    if agent.dismissed_by_user:
        raise RuntimeError(f"card {agent.spec.name} dismissed during the scenario")
    label = format_last_active(elapsed_seconds)
    result = await agent.client.update_node_content(
        agent.lease_id, agent.tile_id, agent.node_ids["body"],
        build_presence_card_text_node(agent.spec.name, label),
    )
    await agent.client.update_node_content(
        agent.lease_id, agent.tile_id, agent.node_ids["chip_text"],
        build_presence_card_chip_text_node(label),
    )
    return {"card_label": agent.spec.name, "body_id": _scene_id(agent.node_ids["body"]),
            "expected_text": f"Connected • last active {label}",
            "ack": {"ok": result.ok, "code": result.code, "hint": result.hint}}


async def watch_for_dismiss(agent: AgentRuntime) -> None:
    try:
        await agent.client.wait_for_click(DISMISS_INTERACTION_ID)
    except asyncio.CancelledError:
        raise
    except Exception:
        return
    agent.dismissed_by_user = True
    await stop_agent(agent)


async def stop_agent(agent: AgentRuntime, *, hard_drop: bool = False) -> None:
    for task in (agent.heartbeat_task, agent.dismiss_task):
        if task is not None and task is not asyncio.current_task():
            task.cancel()
            try:
                await task
            except (asyncio.CancelledError, Exception):
                pass
    agent.heartbeat_task = agent.dismiss_task = None
    if hard_drop:
        agent.dropped = True
        await agent.client.drop_connection()
    elif not agent.dropped:
        try:
            if not agent.dismissed_by_user:
                await agent.client.release_tile(agent.tile_id)
        finally:
            await agent.client.close(reason="presence-card cleanup")


async def run_scenario(args: argparse.Namespace) -> int:
    transcript: list[dict[str, Any]] = []
    agents: list[AgentRuntime] = []
    psk = ""
    target = args.target or hud_env.grpc_target(args.host)
    phase = {"code": "connect", "action": "load paired key and establish resident streams"}
    exit_code = 1
    try:
        psk = hud_env.load_psk(args.host or args.target, args.psk_env)
        plan = build_step_plan(args.update_wait_s, args.orphan_grace_s)
        phase = plan[0]
        emit_step_event(transcript, "started", phase, target=target)
        for spec in AGENTS:
            agent = await start_agent(target, psk, spec, args.agent)
            agents.append(agent)
            emit_step_event(transcript, "acknowledged", phase, **agent_evidence(agent))

        phase = plan[1]
        emit_step_event(transcript, "started", phase)
        await asyncio.sleep(args.update_wait_s)
        for agent in agents:
            update = await update_agent_card(agent, args.update_wait_s)
            emit_step_event(transcript, "acknowledged", phase, **update)

        phase = plan[2]
        gamma = agents[-1]
        emit_step_event(transcript, "started", phase, **agent_evidence(gamma))
        await stop_agent(gamma, hard_drop=True)
        emit_step_event(transcript, "transport_dropped", phase, disconnect_mode="hard")

        phase = plan[3]
        emit_step_event(transcript, "started", phase)
        await asyncio.sleep(args.observe_badge_s)
        emit_step_event(transcript, "observation_pending", phase)

        phase = plan[4]
        emit_step_event(transcript, "started", phase)
        await asyncio.sleep(args.orphan_grace_s)
        for agent in agents[:-1]:
            update = await update_agent_card(agent, args.update_wait_s + args.orphan_grace_s)
            emit_step_event(transcript, "acknowledged", phase, **update)
        emit_step_event(transcript, "observation_pending", phase)

        phase = plan[5]
        exit_code = 0
    except (Exception, asyncio.CancelledError) as exc:
        # A server/transport exception is untrusted text. Never let a credential
        # quoted in it enter stdout, logs or the persisted receipt.
        detail = str(exc).replace(psk, "<redacted>") if psk else str(exc)
        emit_step_event(transcript, "failed", phase, error=type(exc).__name__, detail=detail,
                        remote_outcome="uncertain; no automatic replay")
        exit_code = 130 if isinstance(exc, asyncio.CancelledError) else 1
    finally:
        for agent in agents:
            try:
                await stop_agent(agent)
            except Exception as exc:
                emit_step_event(transcript, "failed", {"code": "cleanup", "action": "clear only this run's known survivors"},
                                card_label=agent.spec.name, error=type(exc).__name__)
                exit_code = 1
        if exit_code == 0:
            emit_step_event(transcript, "protocol_complete", phase,
                            surviving_labels=[agent.spec.name for agent in agents[:-1]],
                            orphan_reclaim_observation="unverified",
                            rendering_followups=["hud-pcscu", "hud-o5elz"])
        if args.transcript_out:
            write_transcript(args.transcript_out, {"target": target, "exit_code": exit_code, "steps": transcript})
    return exit_code


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--host", help="Paired HUD host (default: HUD_HOST)")
    parser.add_argument("--target", help="Explicit gRPC host:port; --host selects the paired key if the target differs")
    parser.add_argument("--agent", default="", help="Optional paired identity assertion; default derives identity from PSK")
    parser.add_argument("--psk-env", help="Existing key environment-variable name; default uses paired file")
    parser.add_argument("--update-wait-s", type=int, default=30)
    parser.add_argument("--orphan-grace-s", type=int, default=30, help="Match the deployed HUD's configured resume grace")
    parser.add_argument("--observe-badge-s", type=float, default=1.0)
    parser.add_argument("--transcript-out", default=DEFAULT_TRANSCRIPT_PATH)
    args = parser.parse_args(argv)
    if min(args.update_wait_s, args.orphan_grace_s, args.observe_badge_s) < 0:
        parser.error("waits must be nonnegative")
    return args


def main() -> int:
    try:
        return asyncio.run(run_scenario(parse_args()))
    except KeyboardInterrupt:
        print(json.dumps({"error": "interrupted"}), file=sys.stderr)
        return 130
    except Exception as exc:
        print(json.dumps({"error": type(exc).__name__, "phase": "configuration"}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
