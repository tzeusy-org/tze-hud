# Progress/status restyle evidence

Seven unaltered own-frame PNGs from the paired owner's already-running build
`24818c39c44dba4ffbe596aa1edc92d849f893d9`, before this package's restyle.
The exact [fixture](../../../.claude/skills/user-test/scripts/progress-status-restyle-test.json)
contains progress 0/50/100 (info severity) and all four declared status values,
online/away/busy/offline. It sends no owner-only theme or fill parameters.

Authenticated discovery and admin status confirmed both target widgets were
not held by this pairing and absent from the drawn-widget observations before
publishing. Capture was one second after each accepted update at 3840×2160.
Each publish had a finite 15-second TTL. Progress was allowed to expire before
status captures; cleanup used TTL only. No shared clear, input read/ACK,
deployment, restart, pairing, permissions, theme, font or scale change occurred.
The owner's selected theme and DPI/UI scale are not exposed by status and were
left unchanged. All seven originals were inspected and hashes verified;
[before.json](before.json) binds the fixture, build, accepted publications and PNGs.

| Fixture index | Case | Original frame |
| --- | --- | --- |
| 0 | progress 0 | [PNG](before-owner-01.png) |
| 1 | progress 50 | [PNG](before-owner-02.png) |
| 2 | progress 100 | [PNG](before-owner-03.png) |
| 3 | online | [PNG](before-owner-04.png) |
| 4 | away | [PNG](before-owner-05.png) |
| 5 | busy | [PNG](before-owner-06.png) |
| 6 | offline | [PNG](before-owner-07.png) |

## Source-bound AFTER matrix

The [gallery](gallery.html) links all seven BEFORE originals and all 33 AFTER
originals. Its AFTER previews crop the original frames in the viewer; PNG bytes
are unmodified. [after.json](after.json) binds every frame to source
`385b76a787684fa2e7b79342e1bbdd5e38ca7429`, the exact fixture message,
resolved tokens, production configuration and PNG hash.

AFTER uses the production scene startup and real windowed GPU capture path on
Mesa llvmpipe at 1920×1080, monitor scale 1, UI scale 1, one second after each
isolated fixture message. The matrix covers progress 0/50/100 and all four status
values under Tonal Glass, Classic and Blueprint (21 frames), plus all four status
values for each owner-only minimal/system/friendly style under Tonal Glass
(12 frames). Owner styles are selected through production `initial_params` in
fixture-only configuration copies; no agent theme parameter is published.
The manifest records the exact configuration hashes and owner-style patches.
All 40 original PNG hashes and the shared fixture hash were verified, and the
33 AFTER previews were visually inspected.

Progress now uses a 320×50 logical card, secondary label, always-visible shaped
monospaced percent and a 4dp track. Status uses a 40dp capsule with its selected
marker, status name and bounded inline reason in the existing tooltip canvas.
Owner label/reason hover behavior, four status values and agent parameter
schemas remain unchanged.

At the captured source, `just ci` passed (2,958 workspace tests, zero skips;
148 integration tests; static/Python, Linux and Windows GNU Clippy, token
footprint and both production boot suites). `just gesture widget-hover` passed
on private Xvfb with real XTEST input. Existing GPU coverage checks progress
values, percent visibility/change, all 12 owner-style/status combinations,
reason clipping, capsule shape, severity/fill and per-instance raster behavior.
Tests: +0 ~6 -0; no additional test functions.

The owner's BEFORE build/theme/DPI and resolution differ from this software-GPU
AFTER environment. These frames prove the captured source's rendering, without
claiming cross-environment pixel parity. Live Windows owner AFTER and visual
acceptance remain open in **hud-h51u7.8.6**.
