//! External behavior suites, retained at their original source paths.

#[path = "common/mod.rs"]
mod common;
#[path = "../../crates/tze_hud_runtime/src/test_support.rs"]
mod gpu_init;

#[path = "multi_agent.rs"]
mod multi_agent;

// HitRegionNode input capture, local feedback, and focus cycling.
// Tests: PointerDown/Up events on Refresh/Dismiss buttons, hover enter/leave,
// auto_capture + release_on_up semantics, focus cycling Tab/Shift+Tab,
// focus ring bounds, FocusGainedEvent/FocusLostEvent dispatch, cross-tile Tab,
// and keyboard-only activation via NAVIGATE_NEXT + ACTIVATE.
#[path = "dashboard_tile_input.rs"]
mod dashboard_tile_input;

// Subtitle streaming breakpoint reveal — MCP and gRPC publish path verification.
// Tests: gRPC ZonePublish with stream_text + breakpoints forwarded to compositor,
// empty breakpoints = immediate reveal, replacement cancels streaming, gRPC/MCP parity,
// list_zones subtitle zone metadata (contention_policy, accepted_media_types).
#[path = "subtitle_streaming.rs"]
mod subtitle_streaming;

#[path = "text_stream_portal_surface.rs"]
mod text_stream_portal_surface;

#[path = "text_stream_portal_governance.rs"]
mod text_stream_portal_governance;

// Phase-1 markdown rendering subset — parse-on-commit tests.
// Covers: each subset construct (headings, bold, italic, bold-italic, inline
// code, fenced/indented code blocks, lists, links), each excluded construct
// degrading to literal text, link non-navigability, node-budget compliance
// (65535-byte payload), and zero-per-frame parse cost (task 2.4 + 2.5).
#[path = "text_stream_portal_markdown.rs"]
mod text_stream_portal_markdown;

// Phase-1 composer draft editing verification (task 4.7).
// Covers: local echo independence from adapter latency, word-wise delete,
// coalesced notifications, oversized-paste truncation and non-forwarding,
// submit-content fidelity, safe-mode suspension, keystroke non-passthrough
// across both adapter families (runtime draft + cooperative projection adapter).
#[path = "text_stream_portal_composer.rs"]
mod text_stream_portal_composer;

// Sustained streaming cadence verification.
// Covers: work-conserving coalescing with cross-portal fairness, cadence harness
// (≥200 scalars/s sustained, 4KiB/250ms bursts), frame/stage budget verification,
// dual-portal fairness under equal sustained rates, retained-window coherence under
// burst. Live soak (5.5), live cadence (5.6), and RTT evidence (5.7) are deferred
// to the Windows reference host.
#[path = "text_stream_portal_cadence.rs"]
mod text_stream_portal_cadence;

// Portal component styling verification.
// Covers: portal part inventory token mapping, exemplar adapter consuming resolved tokens
// (no literal visual values), collapsed/expanded transitions redaction-safe, profile-swap
// reskin test, token-propagation on republish. Live profile-swap phase is deferred
// to the Windows reference host (discovered follow-up).
#[path = "text_stream_portal_styling.rs"]
mod text_stream_portal_styling;

// Long-press drag-to-reposition interaction.
// Tests: long-press recognizer activation/cancellation, multi-touch independence,
// snap quantisation, boundary clamping at all 4 edges, full drag lifecycle
// (press → hold → drag → release → persisted geometry).
#[path = "drag_reposition.rs"]
mod drag_reposition;

// E2E integration tests for persistent movable elements.
// Tests: cross-session persistence (TOML round-trip), element discovery by
// namespace filter, display resolution change (relative-percentage invariance).
// Also covers reset fallback paths and ElementRepositionedEvent delivery.
#[path = "movable_elements_e2e.rs"]
mod movable_elements_e2e;

// POC acceptance (docs/scope.md) for the Portal and Zones bullets,
// end to end over MCP on loopback HTTP and the real windowed event-loop
// dispatch, headless, with production.toml and an injected TestClock. The
// MCP-level regression guard for the T7 portal rewrite.
#[path = "poc_acceptance.rs"]
mod poc_acceptance;
