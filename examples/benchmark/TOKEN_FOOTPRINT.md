# Token-footprint calibration

This Layer-3 calibration measures the exact JSON-RPC request and response
bodies of the canonical MCP flows against a real headless runtime and its MCP
HTTP server:

| Flow | Calls |
|---|---|
| `tools_list` | `tools/list` |
| `discover` | `hud_surfaces` (default zones plus one gauge widget) |
| `zone_publish` | `hud_publish` of a notification |
| `widget_publish` | `hud_publish` of one widget param |
| `portal` | `hud_publish` (attaches and publishes), `hud_input` poll, `hud_input` ack, `hud_clear` |
| `error` | `hud_publish` to an unknown zone |

`token_footprint_flow.py` drives the flows with standard `tools/call`
requests. The fixture fixes ids, content, order, and the portal input. HTTP
headers and credentials are excluded. No model or external network call
occurs.

Each operation records two measures:

- **Wire** (`request`, `response`, `total`): the full JSON-RPC bodies.
- **Model-visible** (`model_visible`): what enters the model's context — the
  tool name and arguments, plus the result text (for `tools/list`, the tool
  array). The JSON-RPC envelope (about 50 tokens per call) never reaches the
  model.

`tiktoken-rs` 0.12.0 counts each text with the bundled `o200k_base`
vocabulary. Operation and flow totals are integer sums. The tokenizer,
vocabulary, flow version, fixture, and flow fingerprints make incompatible
baselines fail closed.

CI runs the calibration twice and requires byte-identical JSON. It then checks
two things against `scripts/ci/token_footprint_baseline.json`:

- **Budgets:** each flow's model-visible tokens must stay at or under the
  ceiling in `budgets` (the targets in `docs/api.md`).
- **Regressions:** every metric is compared with the baseline:

```text
measured * 100 > baseline * 105  => fail
baseline < measured <= 105%      => warning
measured < baseline              => improvement
```

Run locally:

```bash
mkdir -p test_results/token-footprint
HEADLESS_FORCE_SOFTWARE=1 cargo run -p benchmark --features headless \
  --bin token_footprint_calibration -- \
  --output test_results/token-footprint/measurement.json
python3 scripts/ci/check_token_footprint.py \
  --measurement test_results/token-footprint/measurement.json \
  --baseline scripts/ci/token_footprint_baseline.json \
  --output test_results/token-footprint/gate-report.json
```

The checked-in values are the comparison authority. Their approval status is
`owner_approved`, or `pending_owner_review` while the owner reviews a new
baseline in its PR; a pending baseline is compared in full but reports at best
`warning`. The gate fails closed with `baseline_incompatible` on any other
approval status, a missing decision reference or budgets, or a change to the
compatibility identity
(tokenizer, fixture fingerprint, flow version, flow fingerprint, or operation
set). An intentional change re-records the baseline with owner approval.
