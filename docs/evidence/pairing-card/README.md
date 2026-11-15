# Pairing card and toast evidence

Source: `273bf18d0e3a8c4a1fd2942f8f477d82051e18d8`, `hud-h51u7.8.3`.
[Gallery](gallery.html) · [source-bound capture manifest](after.json).

Nine original PNGs show the pairing card, blocked-firewall warning and toast under
Tonal Glass, Classic and Blueprint. The offline `render-scene --system-card` mode
uses production scene startup and the actual windowed GPU build/capture seam,
Mesa llvmpipe, 1920×1080, monitor/UI scale 1, checkpoint 1000 ms. Each fixture is
synthetic; no runtime listener, live code, installed setting, agent publication or
owner host was used. The manifest binds every fixture, compiled example, renderer
binary and original PNG hash to the clean source commit. These files are copies
of the original captures, without pixel edits; the gallery uses CSS viewports.

**The required current Windows owner BEFORE is missing.** Synthetic captures are
additional renderer evidence, not an owner BEFORE. AC3 remains incomplete pending
that capture or an explicit owner amendment. The package must remain draft and
open. Live owner AFTER/visual acceptance remains separately open in
`hud-h51u7.8.6`; these images do not establish Windows rendering or owner approval.

The renderer uses semantic highest/container surfaces, outline, caution and type
roles. Tonal Glass has a 440 dp card, 28 dp padding/corners, 20 dp group gaps and a
16 dp code well. Six numeric cells use the actual monospace font, 0.06 em tracking
and a three-digit group gap; the underlying six-digit code stays unchanged.
MCP-port pairing URL and optional host metadata remain owner-only. An actual
blocked firewall verdict supplies the warning; text does not infer severity.
Actual font measurement and glyph buffers are retained across unchanged frames,
with invalidation on content, tokens, fonts and dimensions. A constrained display
fits the complete message, including the warning, with the geometry and type
scaled together. Toasts use the same rounded semantic chrome at bottom centre.

The mockup's New code / Done buttons are omitted under the issue's explicit
instruction to avoid adding a new input model. Existing Connect controls own
regeneration and closing. No agent surface or wire schema changes.

The existing three layout parents cover geometry, role overrides, all themes,
150% monitor scale, 187.5% combined scale and long warning/URL containment in a
320×240 viewport. Existing GPU parents cover the rounded corner/hairline, safe-mode
layering, notice exclusion, no-op updates and zero warm-frame shaping. Eight
focused compositor/config parents passed after the theme declarations and grouped
code were complete. Full workspace validation is recorded in the PR; no owner
acceptance is implied by a green check.

Tests: +0 ~8 -0 (one extended parent is the offline authoring binary's existing
input/PNG test; another test changes only its owner-model fixture literal).

Reproduce a frame from the source commit:

```bash
just render-scene --system-card examples/render_artifacts/fixtures/pairing-card.json \
  --theme tonal-glass --capture-at-ms 1000 --output test_results/pairing-example.png
```

Use `pairing-card-warning.json` or `system-toast.json` and the other theme names
for the remaining cells. Output paths must be fresh. Never supply a live code or
private endpoint/host to a fixture intended for publication.
