# Compact status bar evidence

The [gallery](gallery.html) shows viewer crops of four unaltered full-frame PNGs.
The same [fixture](../../../.claude/skills/user-test/scripts/status-bar-restyle-test.json)
publishes three synthetic string entries: build, host and network status.
[before.json](before.json) and [after.json](after.json) bind the exact fixture,
build/source, capture environment, dimensions and every original PNG hash.

BEFORE is the paired owner's own-frame admin screenshot on the already-running
build `24818c39c44dba4ffbe596aa1edc92d849f893d9`, captured at
2026-11-14T22:32:15Z, 1.2 seconds after an accepted publication with a 15-second TTL.
Authenticated discovery confirmed the status-bar surface first. Existing theme,
font and DPI/UI scale were left unchanged and are unknown. Cleanup uses TTL only;
no settings, deployment, restart, pairing, permissions, input read/ACK or shared
clear occurred. The vertical card and all three synthetic entries were inspected.

AFTER is actual production scene startup and windowed GPU build/capture on
Mesa llvmpipe at source `f0dc5d316c6895cd3ec8b59316599265bcc33107`, with clean
tracked source throughout. Each theme uses the exact same fixture, one second
after publication, at 1920×1080 with monitor scale 1 and UI scale 1. The unchanged
capture binary and compiled example source are hashed in the manifest. All three
originals were inspected: sorted key/value pairs share one compact horizontal
pill with dividers; no empty vertical slab remains.

| Theme | Original AFTER |
| --- | --- |
| Tonal Glass | [PNG](after--tonal-glass.png) |
| Classic | [PNG](after--classic.png) |
| Blueprint | [PNG](after--blueprint.png) |

The pill uses semantic surface/text/outline roles, 36dp height, 16dp outer inset,
14dp side padding, 6dp key/value gap and 12dp either side of each divider.
The runtime retains sorted MergeByKey/latest values, one-line ellipsis,
explicit owner SVG icons, and shared drawing/drag bounds. Empty entries draw
nothing, including no drag handle. A 240dp per-entry limit and the available
region bound long fields and hide a suffix that cannot fit. Measurement and
painted text honor the existing profile byte budget at UTF-8 boundaries.

`just ci` passed: 2,958 workspace tests with zero skips, 148 integration tests,
static/Python checks, Linux and Windows GNU Clippy, token footprint, and both
production-boot suites. A separate `cargo check --workspace` passed. Existing
GPU parents cover scaled geometry (150%/187.5%), token gap overrides, sorted
merges, empty rendering, shared hit bounds, actual ellipsis pixels, font-role
cache invalidation, UTF-8/newline byte bounds, custom 36px-high regions, and
owner icon resizing. The compositor exports the drawn bounds to runtime drag
admission. The existing runtime drag parent failed against the old handler
(the full-screen region clamped movement to 0,0), then passed the correction:
visible-pill movement, both stale and refreshed frame bounds, and screen-edge
clamping retain the original maximum region. Independent review then reproduced
later growth escaping the display after a valid left drag: a full-width pill
started at x=-984 and eight text boxes began offscreen. The corrected layout
caps measurement to the assigned display dimensions and clamps the final pill,
while preserving its original maximum region and short-content placement.
The same existing GPU parent now exercises growth/shrinkage after translation,
both screen edges, a secondary display with a negative origin, and a display
smaller than the region. Text/SDF/drag geometry agree; unchanged actual-font
layouts reuse measurement. No new drag schema is agent-facing.

`just gesture drag-alignment` passed on the corrected source in 2.6 seconds,
using real XTEST input on a private owned Xvfb display. An initial run on
813acdbb failed at composer focus before any drag; an older landed binary and
the corrected sources passed the same harness. The cause is unproven and retained in
**hud-7e55f**; the later pass does not claim to fix that focus failure.

The refreshed three-theme PNGs are byte-identical to the preceding source's
unchanged fixture captures; their new manifest binds the corrected source.

Earlier Windows run 43650526249 on head 6c0ac9dc passed runtime 622/parser 2/
font 7 tests and boot smoke. Its aggregate success masked informational
fullscreen gesture failures and a foreign-foreground refusal with 0/0 complete
results; only the two overlay scenarios passed. **hud-xq14t** retains that
limitation. This is not a native fullscreen gesture pass or an owner signoff.

Tests: +0 ~20 -0; no added test functions.

The owner BEFORE environment and software-GPU AFTER differ in resolution,
selected theme and DPI. These frames prove captured source rendering; they do
not establish pixel parity or Windows owner visual acceptance. Live owner AFTER
and visual acceptance remain separately open in **hud-h51u7.4.6**.
