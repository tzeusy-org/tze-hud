# Claude Code five-hour usage gauge (owner opt-in)

`hud_usage.py` updates one shared HUD gauge directly from the status line, with
zero model calls or model tokens. It does not install settings or modify the
owner's status line, selected pairing, keys, or model permissions. This is an
independent widget publisher; it never drains input or writes attention state.

The [official status-line reference](https://code.claude.com/docs/en/statusline)
was checked 2026-11-02: `rate_limits.five_hour` supplies `used_percentage`
(0–100) and `resets_at` (Unix seconds). Subscription windows appear only after
the first API response for eligible Pro/Max/Ultra accounts. Individual windows
may be absent and disappear at reset. Claude natively updates the status line
at a known reset and cancels an in-flight status-line run when replacing it.
These are documentation facts, not observed owner CLI behavior. No genuine
owner status-line recording or Windows visual acceptance is available yet.

## Setup

On the HUD, add this instance inside the desired existing `[[tabs]]` block
(built-in `gauge` is embedded; no widget bundle deployment):

```toml
[[tabs.widgets]]
widget_type = "gauge"
instance_id = "usage"

[tabs.widgets.geometry]
x_pct = 0.05
y_pct = 0.42
width_pct = 0.028125
height_pct = 0.1555556
```

Use a trusted Linux/WSL status-line host with Bash, `setsid` (util-linux), and
Python 3.10+. Native Windows/macOS status-line hosts are unverified; this
integration requires the Linux `setsid` utility and POSIX locks/timer. The HUD
can run on Windows. Pair through the existing owner flow. The helper uses
`hud_env.adapter_endpoint()` and `adapter_key()`: explicit `HUD_HOST` selects
that endpoint and its one paired key; an unset host requires exactly one safe
paired host. Multiple hosts need `HUD_HOST`. No key enumeration or automatic
pairing occurs. The selected identity needs only `allow = ["widget:usage"]`
(or the selected instance). Review grants in the HUD's Agents panel. If this
identity is also used by a model, preserve its existing grants; do not replace
its key to install this optional helper. No alternate key selector is added.

In the existing Bash status-line script, **after its existing** `input=$(cat)`
(or equivalent retained stdin variable), add this exact single line, replacing
only the interpreter/repository paths and using the existing variable's name:

```bash
(setsid /absolute/path/to/python3 /absolute/path/to/tze-hud/.claude/skills/hud-projection/scripts/hud_usage.py <<<"$input" >/dev/null 2>&1 & ) 2>/dev/null
```

Keep all existing stdout code unchanged and do not add another stdin reader.
The detached helper receives the retained JSON on stdin, not in process arguments
or the environment, inherits no output descriptors, and survives cancellation
of the status-line process group. Paths containing spaces must be quoted.
Export `HUD_HOST` in the existing trusted environment if needed; optionally
export `HUD_USAGE_WIDGET=usage` to select a different configured instance
(1–64 ASCII letters/digits, `_`, `-`). Never put a key in this line.
Do not add `refreshInterval`, a timer, or a background polling loop.

## Display and update behavior

Only a finite numeric `used_percentage` from 0 through 100 and an unexpired,
representable numeric `resets_at` are eligible. The helper ignores context-window,
seven-day, spend-limit, session, and unknown fields. Invalid/absent metric data,
including malformed JSON, means unavailable and attempts one own-widget clear;
repeated unavailable input is a no-op after acknowledgment or an uncertain attempt. Oversized input
(over 64 KiB), unsafe metadata, missing pairing, or invalid selection silently
skips delivery. An already-clear `NOT_HELD` response confirms clear.

For 41.2%, the level is 0.412, label `5h usage`, and hover readout
`41% · resets HH:MM`. Reset time uses the **status-line host's local time zone**,
not the Windows display host's. 41.9% in the same window makes no request;
42.0% makes one. Severity is `info` below 80%, `warning` at 80% and above,
using the gauge's existing caution edge and local hover behavior. A different
`resets_at` identifies a new window even at the same whole percent. There is
no helper reset timer: Claude's native reset event or the next genuine
invocation supplies the clear/update. Acknowledged publications are durable
unless capped by the instance's owner-set `auto_clear_ms`.

## Concurrency, bounds, and uncertainty

All sessions selecting the same endpoint, widget, and paired identity share
one gauge. State lives outside the repository at
`~/.cache/tze-hud/claude-usage/<domain-hash>/state.json`; directories are 0700,
files 0600. The domain is a SHA-256 of endpoint/widget/identity; it stores no
raw endpoint or credential. Metadata is capped at 2 KiB and contains only the
version, bounded desired generation and scalar percentage/window/level, latest
attempted generation/key, and acknowledged generation/key. A retained legacy ACK
may have an unknown generation; it cannot suppress a distinct new transition.
No raw status JSON, session content, credentials, or payload logs are stored.
There is one directory per selected domain, not
one per session/window/percentage; ordinary updates do not accumulate history.
Atomic writes reuse one validated `state.pending` staging file under the state
lock, so abrupt crashes leave at most one bounded staging file, not growing
temporary-file history.

A nonblocking short state lock admits/updates desired metadata. A separate
nonblocking delivery descriptor serializes sends; network never holds the
state lock. Concurrent invocations can record new desired values during a held
response, then exit without waiting. After a parsed success, the sender keeps
delivery ownership and waits for the short state lock within the original
process deadline, then re-reads state and records only the acknowledged claim,
preserving newer desire. Temporary metadata contention therefore does not
discard a known successful ACK. Exhausting that same deadline during local
persistence still leaves an uncertain claim.
It can send at most one newest-desire catch-up within the same budget; further
changes wait for the next genuine invocation. No daemon, heartbeat, polling,
backoff, or retry loop runs. Delayed session snapshots can be older than each
other; this does not establish global Claude snapshot order.

One absolute 1.0 s default-action process timer covers input, admission,
network/DNS/response, and cleanup after helper imports. HTTP uses only the
remaining budget and responses are capped at 4 KiB. Admission and delivery locks
never wait; only post-ACK state persistence can wait within that same timer. Interpreter
startup, OS scheduling, filesystem stalls/uninterruptible syscalls, or stopped
processes do not establish an end-to-end hard real-time guarantee. Detached
execution keeps these delays off existing stdout; actual owner-host latency
still needs measurement. Failure produces no stdout/stderr; timeout termination
may have a signal exit status. That status is not a delivery acknowledgment.

**Lost ACK/crash remains uncertain for that generation.** Before a possibly
transmitted request, the helper persists its generation/key claim. Only a
matching successful response (or clear `NOT_HELD`) acknowledges that claim.
Transport failure, rejection, oversized response, interrupted ACK persistence,
or crash never confirms it. Replaying that same attempted generation, waiting,
restarting the helper, or releasing the delivery descriptor does not retry it.
A failure known to precede any possible transmission, such as missing pairing
before a claim, may retry on a genuine invocation; transport refusal is not
presumed safe.

After delivery ownership is free, a genuine invocation may send one **later
distinct** percentage/window/clear generation despite older uncertainty. A
newer desire admitted while the old sender was live remains pending; the next
real invocation can claim it without another value change. Returning to an old
value after an intervening transition, such as 41→42→41 or value→clear→value,
is a new generation; a historical equal ACK cannot suppress it. Unclaimed
intermediate desires can coalesce. State stays constant in size rather than
retaining a once-per-value history. Valid older in-flight metadata remains
conservatively attempted/uncertain for its matching step; migration never
clears it into retry eligibility. Unsafe or exhausted state skips silently.

**This is an advisory gauge with an explicitly unbounded stale-display risk.**
The widget API has no client-generation fence, compare-and-set, or request-status
query. Local release is not server completion: an arbitrarily late old publish
can regress percentage/readout/severity, overwrite a newer window, or resurrect
the gauge after a successful clear. An old clear can remove newer content.
Stale or missing display may persist indefinitely, including after unchanged
input. Unchanged input makes no repair request. A further distinct genuine
transition may update it if delivered, but no finite next-input or eventual
convergence, cross-request ordering, exactly-once server execution, rollback,
or stale-write exclusion is promised. TTL/auto-clear, reset time, elapsed client
time, client death, discovery and state deletion do not supply a server fence.
The owner approved this option C tradeoff; it adds no server API, polling,
daemon, background retry, or timer-driven refresh.

## Disable and verification

Remove only the added line and let owned helpers exit. Clear only
`widget:usage` through the ordinary selected paired client when desired;
outstanding old server work may still republish it or remove newer content.
Optional removal of this domain's nonsecret `state.json` requires separately
establishing that old server work is drained; preserve stable lock files.
Deleting state is not a fence and can revive an uncertain attempt. Normal C
operation does not require manual draining or state deletion. Preserve other
hooks, settings, keys, and widget publications. A human clear does not cause
an unchanged acknowledged generation to republish; a distinct genuine
percentage/window/clear transition supplies the next observation.

The existing `test_portal_client.py` fixture uses docs-derived **synthetic** JSON
and real helper/Bash subprocesses against a loopback fake HUD. It checks request
counts, finite metrics, raw-level/floor formatting, 80% caution, new windows,
unavailable clear dedup, private bounded state, concurrent held ACK catch-up,
lock freedom, lost ACK, post-client-death server commit, same-generation crash suppression, allowed late old publish/clear commits,
matching-generation ACKs, later-distinct eligibility, old-state compatibility, and
parent cancellation with unchanged stdout. Existing widget-client defaults are
also retained and tested. These fixtures are not recordings or Windows proof.

Owner acceptance remains **unexecuted while the host hibernates**: retain exact
CLI version, existing status-line SHA, helper/client SHA, HUD exe/config/theme
SHA/provenance and safe recorded field names; then verify 41→42 fill/readout and
hover, 80% caution, missing/reset clear, repeated unavailable no-op, and HUD-down
status-line latency. Record actual max/p95 latency and screenshots. Live Claude
cancellation and native reset scheduling remain separate from the synthetic Bash
cancellation test. Source merge does not close those checks.
