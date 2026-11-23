# Test publish payloads

Use the [user-test workflow](../SKILL.md) to discover the available surfaces.
Pass a JSON array to the [shared publisher](../../th-hud-publish/scripts/publish.py)
with `--messages-file`. A row names a `zone`, a `widget` instance, or a full
`surface` such as `zone:subtitle`. Pick only rows whose targets are allocated to
the test and supported by discovery. Substitute the requested test content.

```json
[
  {"zone":"alert-banner","content":"Dev update complete","ttl_ms":30000},
  {"zone":"subtitle","content":"Checking the dev HUD","ttl_ms":60000},
  {"zone":"status-bar","content":{"type":"status_bar","entries":{"test":"passing"}},"key":"dev-check","ttl_ms":60000},
  {"zone":"notification-area","content":{"body":"Build complete","urgency":1},"ttl_ms":30000},
  {"zone":"ambient-background","content":{"type":"solid_color","r":0.1,"g":0.15,"b":0.4,"a":0.05},"ttl_ms":30000},
  {"zone":"pip","content":{"type":"solid_color","r":0.2,"g":0.8,"b":0.2,"a":0.05},"ttl_ms":30000},
  {"widget":"main-progress","params":{"progress":0.5,"label":"Dev check"},"ttl_ms":60000}
]
```

Zone content can be plain text or a typed object. Widget values must match the
discovered types and ranges. Use a finite, top-level `ttl_ms` for test content.
See [HUD Publish](../../th-hud-publish/SKILL.md) for content shapes, keyed
notifications, and input ownership if a test needs buttons.

For runs without a screenshot, `--cleanup-on-exit` checks that batch widget
targets are listed and unheld before publishing, then attempts to clear those
it may have changed even if a request fails or the run is interrupted. Use it
only on a dedicated pairing with no existing, scheduled, or concurrent work. After
a screenshot, let finite TTLs expire or clear only your own dedicated widget
with `--surface widget:<name> --clear`. Inspect the clear receipt.
