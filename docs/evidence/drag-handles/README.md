# Hover-only drag-handle evidence

The [gallery](gallery.html) compares viewer crops of seven unaltered full-frame
PNGs. The same [fixture](../../../.claude/skills/user-test/scripts/drag-handle-restyle-test.json)
publishes two synthetic notifications with a 15-second TTL.
[before.json](before.json) and [after.json](after.json) bind the fixture, source,
capture environment, binary and original PNG hashes.

BEFORE is the paired owner's own-frame admin screenshot of the already-running
build `24818c39c44dba4ffbe596aa1edc92d849f893d9`, captured at
2026-11-15T00:53:02Z after authenticated discovery and publication. The original
3840×2160 frame shows the two fixture cards and the old always-visible grey grip.
Existing theme and DPI/UI scale are unknown and were left unchanged. The finite
TTL expired naturally; no settings, deployment, restart, pairing, permission,
input read/ACK or shared clear occurred.

AFTER uses production scene startup and the actual windowed GPU build/capture
on Mesa llvmpipe at clean source
`537537d7b3b602a433ea41e0c6d7600f21a27863`. Each frame uses that same fixture,
1920×1080, monitor/UI scale 1, and a one-second capture checkpoint. The capture
binary and compiled example source are hashed. Offline `--drag-hover-at 1500,100`
resolves through the refreshed frame's hit-test snapshot and local hover mirror;
it injects no OS input and does not prove an operating-system gesture.

| Theme | Idle | Element hover |
| --- | --- | --- |
| Tonal Glass | [PNG](after--tonal-glass--idle.png) | [PNG](after--tonal-glass--hover.png) |
| Classic | [PNG](after--classic--idle.png) | [PNG](after--classic--hover.png) |
| Blueprint | [PNG](after--blueprint--idle.png) | [PNG](after--blueprint--hover.png) |

All six originals were inspected. Idle has no move grip; hover shows one rounded
32×4dp semantic pill in a 12dp row, separated from the card by `space.s` (8dp).
For each theme, idle/hover differ at 127 pixels, all within `(1654,43)–(1686,47)`.
The continuous hover envelope includes the gap without adding pointer capture.
Portal frames retain their full header input band and draw a small pill within
it; widgets retain their adopted no-handle/Layout placement contract. The pip
resize mark remains separate. These notification captures do not depict portal
or pip appearance.

Independent review reproduced a rear-tile pill on overlapping body hover. The
existing runtime parent now fails on that old query and passes front-to-back
hover selection, while exposed rear bodies still resolve to their own pill and
grip capture stays unchanged. Refreshed captures on the corrected source are
byte-identical to the six previously inspected originals; their new manifest
binds the corrected executable and clean source.

Nine focused existing parents passed, including actual PNG readback, all three
themes at 100/150/200% scale, token overrides, active drag feedback, quiet hover
entry/exit, and runtime drag/control translation. The runtime parent proves that
card-body and gap hover do not become drag capture and that empty logical zone
space below the painted cards has no pill. Tests: +0 ~6 -0; no test function added.

The owner BEFORE and software-GPU AFTER differ in resolution, selected theme and
DPI. This is source-rendering evidence, not pixel parity or Windows owner visual
acceptance. Live owner AFTER and visual acceptance remain open in **hud-h51u7.4.6**.
