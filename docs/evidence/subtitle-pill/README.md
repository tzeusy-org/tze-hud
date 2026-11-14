# Subtitle pill evidence

The approved content-hugging pill is the `Subtitle + progress` element in
`Hud.dc.html` from artifact [7U7xiKxWGAptBz7GaqQ2DN](https://claude.ai/artifact/7U7xiKxWGAptBz7GaqQ2DN).
The retained owner export has SHA256 `8418bc1525e8cd1abe2b410505a56362317f09177d9d75f78f324f5484f80a70`. Its caption is 26 / 34 / 600,
with a 760 dp maximum width, 10 / 20 dp padding, `shape.l` (16 dp) corners and
`color.surface` at `opacity.scrim` (0.72). The same measured bounds drive the
pill, wrapped text, ellipsis priming and drag handle throughout word reveal.
Classic retains its 28 / 39.2 / 600 outlined, zone-wide caption without a pill
fill; Blueprint uses its own caption, corner and surface tokens.

BEFORE consists of three unaltered frames from the paired owner HUD's existing
build `24818c39c44dba4ffbe596aa1edc92d849f893d9`, at 3840×2160. No update,
restart, theme, font or scale setting was changed. Its admin status does not
expose the selected theme or UI scale. Authenticated discovery found the subtitle
surface free. Each sequential publication used a 20-second TTL; captures were
one second after acceptance, and cleanup used expiry. There was no shared clear,
input polling or ACK. These are observed streaming checkpoints, not proof that
the old build renders every intermediate or final line correctly.
[before.json](before.json) records original hashes, dimensions and receipts.

AFTER uses clean source `ae9078105082efde2046ead8cd998a61d4521198`, the actual production startup,
MCP dispatch and windowed GPU capture seam (`render-scene`), Mesa llvmpipe,
1920×1080, monitor/UI scale 1 and checkpoint 1000 ms. Each invocation replays
one exact message from the same [fixture](../../../.claude/skills/user-test/scripts/subtitle-pill-test.json),
so latest-wins cannot erase the earlier cells. All nine original PNGs were
inspected and their hashes verified. [after.json](after.json) binds each frame
to the exact fixture/cell/content hashes, clean source tree, binary and compiled
example hashes, resolved theme tokens, renderer and dimensions. Subsequent
commits add only evidence; the captured implementation is unchanged.

The short line hugs its content. The paragraph and overflow fixtures wrap and
ellipsize within the same bounded pill; Classic keeps its wider outlined text.
The default viewport is head-anchored, so the overflow's repeated tail is hidden.
The optional Appearance → Subtitle backdrop still adds a separate whole-zone
contrast fill below the pill. GPU tests independently cover that layer order in
normal/Simplified rendering, including an explicitly rounded whole-zone fill,
plus caption/scale/priming/unchanged-frame/lifecycle behavior.

Open [gallery.html](gallery.html) for bottom-of-frame views linked to the
unaltered full originals. CSS crops are only a viewing aid. BEFORE and AFTER
have different resolutions and environments and are not pixel-compatible.
The existing gray drag affordance is outside this subtitle restyle; its own
restyle remains in `hud-h51u7.4.5`.

Live AFTER captures and the owner's visual verdict remain in **hud-h51u7.4.6**
after deployment. These captures do not establish owner acceptance or native
Windows gesture success. No owner settings, installed build, pairing or
permissions were changed.

| Exact fixture message | Owner BEFORE | Tonal Glass AFTER | Classic AFTER | Blueprint AFTER |
| --- | --- | --- | --- | --- |
| 0 — Short line | [original](before-owner-01.png) | [original](after--tonal-glass--01.png) | [original](after--classic--01.png) | [original](after--blueprint--01.png) |
| 1 — Paragraph | [original](before-owner-02.png) | [original](after--tonal-glass--02.png) | [original](after--classic--02.png) | [original](after--blueprint--02.png) |
| 2 — Overflow | [original](before-owner-03.png) | [original](after--tonal-glass--03.png) | [original](after--classic--03.png) | [original](after--blueprint--03.png) |
