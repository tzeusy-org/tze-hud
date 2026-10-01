//! Image texture cache, local composer state, and font/image loading methods
//! for the compositor.
//!
//! Moved from `renderer/mod.rs` banner 6 (`// ─── Image texture cache ──`)
//! and the font/image loading method cluster (formerly ~L1338–1735 pre-split),
//! by Step R-2 of the renderer module split (hud-fgryk).  No logic was changed;
//! only visibility modifiers were added where Rust's module-privacy rules
//! require them (listed in the PR body).

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

use tze_hud_scene::resolve_token_placeholders;
use tze_hud_scene::types::*;

use crate::text::TextRasterizer;

fn unreferenced_resource_ids<'a>(
    cached_ids: impl Iterator<Item = &'a ResourceId>,
    referenced_ids: &HashSet<ResourceId>,
) -> Vec<ResourceId> {
    cached_ids
        .filter(|id| !referenced_ids.contains(id))
        .copied()
        .collect()
}

// ─── Image texture cache ────────────────────────────────────────────────────

/// A cached GPU texture for a static image resource.
///
/// Created by [`Compositor::ensure_image_texture`] on first reference and
/// reused across frames until eviction.
pub struct ImageTextureEntry {
    /// The GPU texture holding RGBA pixel data, kept alive for `bind_group`.
    /// Prefixed with `_` to make intent explicit: this field exists solely to
    /// retain ownership and keep the wgpu texture alive as long as the bind
    /// group references it.
    pub _texture: wgpu::Texture,
    /// Pre-built bind group (texture view + sampler) ready for draw calls.
    pub bind_group: wgpu::BindGroup,
    /// Image width in pixels (needed for fit-mode UV calculations).
    pub width: u32,
    /// Image height in pixels (needed for fit-mode UV calculations).
    pub height: u32,
}

fn downsample_rgba_nearest(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Vec<u8> {
    let mut target = vec![0; target_width as usize * target_height as usize * 4];
    for y in 0..target_height {
        let source_y = y.saturating_mul(source_height) / target_height;
        for x in 0..target_width {
            let source_x = x.saturating_mul(source_width) / target_width;
            let source_offset = (source_y as usize * source_width as usize + source_x as usize) * 4;
            let target_offset = (y as usize * target_width as usize + x as usize) * 4;
            target[target_offset..target_offset + 4]
                .copy_from_slice(&source[source_offset..source_offset + 4]);
        }
    }
    target
}

/// GPU state and render pipeline.
/// Runtime-side local composer echo state.
///
/// Passed from the input-event thread to the compositor thread via an
/// [`Arc<StdMutex<Option<LocalComposerState>>>`] so the compositor can render
/// the current draft text + caret locally on every frame — WITHOUT waiting for
/// an adapter round-trip (§4.1 "Local feedback first").
///
/// Created by the windowed runtime on every keystroke that mutates the draft,
/// cleared when the composer is deactivated (blur, submit, cancel).
#[derive(Debug, Clone)]
pub struct LocalComposerState {
    /// Draft text at the cursor byte position (pre-caret).
    pub text: String,
    /// Caret position as a byte offset into `text`.
    pub cursor_byte: usize,
    /// Selection anchor as a byte offset into `text`.
    ///
    /// Equal to `cursor_byte` when no selection is active.  When
    /// `selection_anchor != cursor_byte`, the selected region spans
    /// `[min(cursor_byte, selection_anchor), max(cursor_byte, selection_anchor)]`
    /// in the original text, and the compositor renders a selection-highlight
    /// background behind those characters.
    pub selection_anchor: usize,
    /// True when the draft buffer has reached its byte capacity.
    pub at_capacity: bool,
    /// The `SceneId` of the `HitRegionNode` that owns this composer region.
    /// Used to locate the tile bounds for overlay placement.
    pub node_id: tze_hud_scene::types::SceneId,
    /// Optional dimmed hint rendered when the draft is EMPTY (hud-evk0j).
    ///
    /// When `text` is empty and this is `Some(non-empty)`, the composer echo
    /// renders this string colored from `portal.composer.placeholder_color`
    /// (see `ComposerOverlayTokens::placeholder_color`) instead of the empty
    /// draft. It is purely a render-time hint: it is never part of the draft
    /// buffer, is never submitted, carries no caret, and disappears the instant
    /// the user types (the moment `text` is non-empty the normal draft path runs).
    /// `None` (or empty) renders no placeholder, so composers that opt out are
    /// unchanged.
    pub placeholder: Option<String>,
}

/// Shared update slot for delivering [`LocalComposerState`] from the
/// input-event thread to the compositor thread without locking the full scene.
///
/// Semantics:
/// - `None` — no new update since the last frame; compositor keeps its current
///   `local_composer` unchanged.
/// - `Some(None)` — explicit deactivation (blur / submit / cancel); compositor
///   clears `local_composer` so the overlay disappears.
/// - `Some(Some(state))` — new draft state; compositor replaces `local_composer`.
///
/// Written by the input-event thread (main thread) on every composer mutation
/// and on deactivation.  Drained (taken) by the compositor thread once per
/// frame at the top of the render loop.
pub type LocalComposerStateHandle = Arc<StdMutex<Option<Option<LocalComposerState>>>>;

/// Reverse channel for soft-wrap vertical caret movement (hud-21o6x): the
/// compositor thread publishes the active composer's wrapped-line layout here
/// each frame; the input thread reads it before dispatching ArrowUp/ArrowDown.
///
/// `Some(layout)` when a multi-line composer is active and was measured this
/// frame; `None` when there is no composer or it is the single-line profile.
/// Mirrors [`LocalComposerStateHandle`] in the opposite direction.
pub type ComposerVisualLayoutHandle = Arc<StdMutex<Option<tze_hud_input::ComposerVisualLayout>>>;

/// Caret blink half-period: the caret stays solid for this long, then hidden
/// for this long, repeating.  ~530 ms is the conventional editor blink rate.
pub(crate) const CARET_BLINK_HALF_PERIOD: std::time::Duration =
    std::time::Duration::from_millis(530);

/// Pure blink-phase logic: given the time elapsed since the caret was last reset
/// to "solid" (a keystroke / caret move), return whether the caret should be
/// drawn on the current frame.
///
/// Phase 0 (`[0, half)`) is solid → `true`; phase 1 (`[half, 2*half)`) is
/// hidden → `false`; and so on.  Resetting `elapsed` to zero on edit therefore
/// yields a solid caret immediately after typing (standard editor behavior).
///
/// This is frame-level state computed by the runtime from its own clock — the
/// model never participates in the blink loop.
pub(crate) fn caret_visible_at(elapsed: std::time::Duration) -> bool {
    let half = CARET_BLINK_HALF_PERIOD.as_nanos();
    if half == 0 {
        return true;
    }
    // Even half-period index → solid; odd → hidden.
    (elapsed.as_nanos() / half) % 2 == 0
}

/// Apply one drain cycle: take the pending update from `handle` and write it
/// into `current`.
///
/// - `handle` holds `None`          → `current` is left unchanged.
/// - `handle` holds `Some(None)`    → `current` is cleared (deactivation).
/// - `handle` holds `Some(Some(s))` → `current` is replaced with `s`.
///
/// Returns `true` when a new draft state (`Some(Some(_))`) was applied — i.e.
/// the caller should reset the caret-blink phase so the caret is solid right
/// after the keystroke.  Returns `false` for the keep-current and deactivation
/// cases (no need to disturb the blink timer).
///
/// This is the pure logic extracted from [`Compositor::drain_local_composer_state`]
/// so that tests can drive it without a GPU-backed [`Compositor`].
pub(crate) fn apply_composer_slot(
    handle: &LocalComposerStateHandle,
    current: &mut Option<LocalComposerState>,
) -> bool {
    if let Ok(mut guard) = handle.lock() {
        if let Some(update) = guard.take() {
            let is_new_draft = update.is_some();
            *current = update;
            return is_new_draft;
        }
    }
    false
}

/// Compute the composer's horizontal scroll offset — the number of physical
/// pixels to shift the draft text LEFT within its visible window so the active
/// caret stays visible once the draft is wider than the composer box (hud-zlfi4).
///
/// This is the pure, GPU-free core of horizontal caret-follow: given the caret's
/// x position and the full draft width — both measured against the composer font
/// by the renderer once per frame — it returns how far to scroll so the caret
/// sits inside the visible text window with a keep-visible margin. Keeping the
/// clamp semantics here (rather than inline in the render path) lets them be
/// unit-tested without a text rasterizer.
///
/// All coordinates are physical pixels measured from the start of the draft text
/// (x = 0 is the first glyph's left edge):
/// - `caret_x`       — the caret's x within the draft (`0` at Home, `content_width` at End).
/// - `content_width` — the full draft width laid out on one unwrapped line.
/// - `window_width`  — visible text width inside the composer box (box width minus
///   the left + right text margins).
/// - `margin`        — keep-visible gap held between the caret and each window edge.
///
/// Semantics (standard single-line chat-input follow, e.g. Telegram/iMessage):
/// - Draft fits (`content_width <= window_width`) → offset `0` (left-aligned).
/// - Otherwise the caret is pinned to the right keep-visible band as the draft
///   grows past the box, and moving the caret back toward Home reveals the
///   earlier text while keeping the caret visible throughout.
/// - The offset is clamped to `[0, content_width + margin - window_width]`, so the
///   tail (End) is fully revealed with the caret inside the margin and deleting
///   text scrolls back left with no blank "dead space" past the draft end.
///
/// The offset is recomputed each frame from the current caret, so no persistent
/// per-composer scroll state is required.
pub(crate) fn composer_scroll_offset(
    caret_x: f32,
    content_width: f32,
    window_width: f32,
    margin: f32,
) -> f32 {
    // Degenerate window: nothing sensible to scroll.
    if window_width <= 0.0 {
        return 0.0;
    }
    // Draft fits entirely — left-align and never scroll. This avoids a stray
    // offset when the caret sits at the end of a draft only slightly narrower
    // than the window.
    if content_width <= window_width {
        return 0.0;
    }
    // Keep the margin sane for very narrow boxes so the target band never inverts.
    let margin = margin.clamp(0.0, window_width * 0.5);
    // Allow the caret + right margin to sit just past the last glyph at End, but
    // no further. Clamping to this bound is what prevents dead space when the
    // draft shrinks (delete) below the current scroll.
    let max_scroll = (content_width + margin - window_width).max(0.0);
    // Right keep-visible band: place the caret `window_width - margin` from the
    // window's left edge once it would otherwise fall off the right. Starting the
    // target at 0 (prefer showing the draft start) and pushing right only as far
    // as needed yields Home → 0 and End → max_scroll for free.
    let target = caret_x - (window_width - margin);
    target.clamp(0.0, max_scroll)
}

/// Per-frame composer layout state, computed by
/// [`super::Compositor::prime_composer_scroll_offset`] and consumed by
/// `render_composer_overlay` / `collect_composer_text_item` (hud-nx7yq.1).
///
/// One of two profiles is active per frame:
/// - **Single-line** (`wrap == false`, max-lines token = 1): the draft is laid
///   out on one unwrapped line and slides horizontally so the caret stays visible
///   (hud-zlfi4). `h_scroll_px` / `content_width` drive it; the vertical fields
///   stay at their one-line defaults.
/// - **Multi-line** (`wrap == true`, max-lines token > 1): the draft wraps within
///   the composer width, the box grows upward to `visible_lines`, and past the
///   token-bounded maximum it scrolls vertically by `vscroll_px` to keep the caret
///   line visible. `h_scroll_px` stays `0` (no horizontal slide).
///
/// All state is local presentation state recomputed each frame from runtime-owned
/// draft text + geometry — no adapter round trip.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ComposerLayout {
    /// `true` = multi-line wrap profile; `false` = single-line horizontal caret-follow.
    pub(crate) wrap: bool,
    /// Horizontal scroll offset (px) — single-line profile only; `0` when wrapping.
    pub(crate) h_scroll_px: f32,
    /// Natural single-line content width (px) — single-line profile only.
    pub(crate) content_width: f32,
    /// Caret x (px), measured from the start of the draft — single-line profile
    /// only; `0` when wrapping (the multi-line profile locates the caret via the
    /// published [`tze_hud_input::ComposerVisualLayout`] instead, hud-hxhnt).
    ///
    /// Raw (pre-scroll) value from `TextRasterizer::measure_composer_caret`,
    /// stashed here by `Compositor::prime_composer_scroll_offset` so the
    /// chrome-layer caret quad (`Compositor::append_composer_caret_vertices`)
    /// does not need its own text-rasterizer pass to relocate it.
    pub(crate) caret_x: f32,
    /// Number of text lines the box currently shows (`1..=max_lines`); drives the
    /// upward-grown box height.
    pub(crate) visible_lines: f32,
    /// Total wrapped line count of the draft (≥ `visible_lines`); the full content
    /// height the draft lays out into before vertical clipping.
    pub(crate) total_lines: f32,
    /// Vertical scroll offset (px) — multi-line profile, non-zero only once the
    /// draft exceeds `max_lines` lines.
    pub(crate) vscroll_px: f32,
}

impl Default for ComposerLayout {
    fn default() -> Self {
        // Single-line, one line tall, no scroll — the inert state when no composer
        // is active or the draft is empty.
        Self {
            wrap: false,
            h_scroll_px: 0.0,
            content_width: 0.0,
            caret_x: 0.0,
            visible_lines: 1.0,
            total_lines: 1.0,
            vscroll_px: 0.0,
        }
    }
}

/// Number of text lines the composer box shows for a wrapped draft (hud-nx7yq.1).
///
/// The box grows from one line up to the token-bounded `max_lines`; beyond that it
/// stops growing and scrolls internally (see [`composer_vertical_line_offset`]).
/// Always at least one line so an empty draft still shows a one-line box.
pub(crate) fn composer_visible_line_count(total_lines: usize, max_lines: usize) -> usize {
    total_lines.clamp(1, max_lines.max(1))
}

/// How many composer text lines the region interior can actually display
/// (hud-nottc).
///
/// The rendered box height is `visible_lines * line_height + 2*margin` clamped to
/// the region height (see `Compositor::composer_input_box`), so a composer region
/// only fits `floor((region_height - 2*margin) / line_height)` full lines — at
/// least one. The multi-line growth (`composer_visible_line_count`) and internal
/// scroll (`composer_vertical_line_offset`) must be bounded by THIS, not just the
/// `max_lines` token: a short composer pane (e.g. a one/two-line input strip)
/// would otherwise "grow" past its bounds and the vertical-scroll math — computed
/// against `max_lines` — would leave the caret line clipped outside the visible
/// box. Bounding both by the region fit keeps the caret inside the box.
pub(crate) fn composer_region_fit_lines(
    region_height: f32,
    line_height: f32,
    margin: f32,
) -> usize {
    if line_height <= 0.0 {
        return 1;
    }
    let interior = region_height - margin * 2.0;
    let fit = (interior / line_height).floor();
    if fit.is_finite() && fit >= 1.0 {
        fit as usize
    } else {
        1
    }
}

/// First visible wrapped line (count scrolled off the top) so the caret line stays
/// visible inside the bounded `max_lines` window (hud-nx7yq.1).
///
/// The vertical analogue of [`composer_scroll_offset`]'s right-pin: when the draft
/// fits within `max_lines` there is no scroll; otherwise the caret line is pinned
/// to the BOTTOM visible line as the draft grows, and moving the caret up reveals
/// earlier lines. Recomputed each frame from the caret line — no persistent state.
///
/// - `caret_line`  — zero-based wrapped-line index the caret sits on.
/// - `total_lines` — total wrapped line count.
/// - `max_lines`   — token-bounded maximum visible line count.
///
/// Returns a first-visible-line index in `[0, total_lines - max_lines]`.
pub(crate) fn composer_vertical_line_offset(
    caret_line: usize,
    total_lines: usize,
    max_lines: usize,
) -> usize {
    let max_lines = max_lines.max(1);
    // Draft fits in the window → no vertical scroll.
    if total_lines <= max_lines {
        return 0;
    }
    let max_first = total_lines - max_lines;
    // Bottom-pin: keep the caret on the last visible line as the draft grows;
    // `saturating_sub` keeps early caret lines (0..max_lines-1) at first-visible 0.
    let desired = caret_line.saturating_sub(max_lines - 1);
    desired.min(max_first)
}

// ─── Font / image loading methods ────────────────────────────────────────────

impl super::Compositor {
    /// Initialize (or re-initialize) the text rasterizer for a given surface format.
    ///
    /// Must be called once after creation before text rendering is available.
    /// For headless compositors, `format` should be `Rgba8UnormSrgb`.
    /// For windowed compositors, use the negotiated swapchain format.
    ///
    /// Calling this multiple times replaces the existing rasterizer (e.g. on
    /// surface resize or format change).
    pub fn init_text_renderer(&mut self, format: wgpu::TextureFormat) {
        let mut rasterizer = TextRasterizer::new(&self.device, &self.queue, format);
        // Re-apply any operator-configured truncation-input bound: a fresh
        // TextRasterizer starts at the TruncationCache default, so re-init on
        // resize / format change would otherwise silently revert an applied
        // per-surface bound.
        if let Some(bytes) = self.configured_max_truncation_input_bytes {
            rasterizer
                .truncation_cache
                .set_max_truncation_input_bytes(bytes);
        }
        self.text_rasterizer = Some(rasterizer);
        tracing::debug!(format = ?format, "text renderer initialized");
    }

    /// Apply the operator-configured per-surface truncation-input bound.
    ///
    /// Sourced from the resolved `DisplayProfile::max_truncation_input_bytes`
    /// (see `tze_hud_config`), this bounds the bytes a single uncached
    /// truncation may shape before the viewport-adjacent-window fallback engages
    /// (spec.md §324/§331).  The value is retained on the compositor so it is
    /// re-applied across any later [`Compositor::init_text_renderer`] call, and
    /// forwarded immediately to the live truncation cache when the rasterizer is
    /// already initialized.
    ///
    /// When this is never called, the truncation cache keeps its built-in
    /// default (`overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES`, 4096), preserving
    /// historical behaviour for configs that omit the override.
    pub fn set_max_truncation_input_bytes(&mut self, bytes: usize) {
        self.configured_max_truncation_input_bytes = Some(bytes);
        if let Some(rasterizer) = &mut self.text_rasterizer {
            rasterizer
                .truncation_cache
                .set_max_truncation_input_bytes(bytes);
        }
    }

    /// Load raw font bytes (TTF or OTF) from an agent upload into glyphon's
    /// `FontSystem`.
    ///
    /// After this call, the font is available for text layout in subsequent
    /// frames.  The `resource_id` is the 32-byte BLAKE3 digest of `data`
    /// (matches `tze_hud_resource::ResourceId::as_bytes()`); duplicate calls
    /// with the same `resource_id` are no-ops.
    ///
    /// Returns `true` when the font was loaded/already present. Returns `false`
    /// when the text renderer is not initialized or resident admission denies
    /// the new copy, allowing the runtime handoff to report the real outcome.
    ///
    /// # Truncation cache invalidation
    ///
    /// Loading a new font changes shaping metrics (advance widths, kerning),
    /// which invalidates any cached truncation results computed with the old
    /// font.  This method therefore resets `truncation_cache_scene_version` to
    /// `u64::MAX` — the forced-re-prime sentinel — so the next call to
    /// `prime_truncation_cache` recomputes all entries regardless of whether
    /// the scene version has changed.  The cadence gate is bypassed for
    /// forced primes (see `prime_truncation_cache`), ensuring the new font is
    /// reflected immediately on the next commit.  [hud-v2z6u]
    pub fn load_font_bytes(&mut self, resource_id: [u8; 32], data: &[u8]) -> bool {
        if let Some(rasterizer) = &mut self.text_rasterizer {
            let was_new = !rasterizer.has_font(&resource_id);
            if was_new
                && let Some(ledger) = &self.resident_ledger
                && ledger
                    .reserve(
                        tze_hud_resource::ResidentClass::Font,
                        format!(
                            "font:glyphon:{}",
                            crate::text::format_resource_id(&resource_id)
                        ),
                        data.len() as u64,
                    )
                    .is_err()
            {
                tracing::warn!(
                    resource_id = %crate::text::format_resource_id(&resource_id),
                    "font residency admission denied"
                );
                return false;
            }
            rasterizer.load_font_bytes(resource_id, data);
            if was_new {
                // A new font changes shaping widths — cached truncation points
                // are now stale.  Reset to the forced-prime sentinel so the
                // next prime_truncation_cache call re-resolves all entries.
                self.truncation_cache_scene_version = u64::MAX;
                self.truncation_cache_scene_instance = None;
                tracing::debug!(
                    resource_id = %crate::text::format_resource_id(&resource_id),
                    "load_font_bytes: new font loaded — truncation cache invalidated [hud-v2z6u]"
                );
            }
            true
        } else {
            tracing::debug!("load_font_bytes called before text renderer is initialized — skipped");
            false
        }
    }

    /// Returns `true` if the font with the given `resource_id` has been loaded
    /// into the `FontSystem`.
    ///
    /// Returns `false` if the text renderer is not yet initialized.
    pub fn has_font(&self, resource_id: &[u8; 32]) -> bool {
        self.text_rasterizer
            .as_ref()
            .map(|r| r.has_font(resource_id))
            .unwrap_or(false)
    }

    // ─── Image texture cache ─────────────────────────────────────────────────

    /// Register decoded RGBA image bytes for a resource.
    ///
    /// The runtime should call this after each successful image upload so the
    /// compositor can create GPU textures on demand. `rgba_data` must be
    /// `width × height × 4` bytes of RGBA8 pixel data. The explicit `width` and
    /// `height` are stored alongside the bytes so that
    /// [`Compositor::ensure_scene_image_textures`] can create GPU textures for
    /// zone images (which do not carry dimensions in `ZoneContent::StaticImage`)
    /// without resorting to the square-root heuristic that fails for non-square
    /// images (e.g. 640×360).
    ///
    /// Duplicate calls with the same `resource_id` are ignored (content-addressed
    /// identity guarantees the bytes are identical).
    ///
    /// Returns without registering if `width` or `height` is zero, or if the
    /// product `width × height × 4` overflows `usize`.
    pub fn register_image_bytes(
        &mut self,
        resource_id: ResourceId,
        rgba_data: Arc<[u8]>,
        width: u32,
        height: u32,
    ) {
        // Guard: reject zero-dimension images; content-addressed uploads should
        // never arrive with degenerate dimensions, so treat this as a caller bug.
        if width == 0 || height == 0 {
            return;
        }
        // Guard: overflow — reject if the byte count would overflow usize.
        if width
            .checked_mul(height)
            .and_then(|px| px.checked_mul(4))
            .is_none()
        {
            return;
        }

        // Insert both maps atomically via the Entry API.  Using separate
        // `or_insert` calls would allow one map to contain the key without the
        // other if a previous partially-failed insertion left them out of sync.
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.image_bytes.entry(resource_id)
        {
            if let Some(ledger) = &self.resident_ledger
                && ledger
                    .reserve(
                        tze_hud_resource::ResidentClass::Resource,
                        format!("image:{resource_id}:cpu"),
                        rgba_data.len() as u64,
                    )
                    .is_err()
            {
                return;
            }
            entry.insert(rgba_data);
            self.image_dims.insert(resource_id, (width, height));
        }
    }

    /// Ensure a GPU texture exists for the given `ResourceId`.
    ///
    /// Returns `true` if a texture is ready (either already cached or just
    /// created). Returns `false` if the image bytes are not registered.
    ///
    /// The flow: check `image_texture_cache` → miss → look up `image_bytes` →
    /// create `wgpu::Texture` → write data → create `BindGroup` → cache.
    pub(super) fn ensure_image_texture(
        &mut self,
        resource_id: ResourceId,
        img_width: u32,
        img_height: u32,
    ) -> bool {
        // Already cached?
        if self.image_texture_cache.contains_key(&resource_id) {
            return true;
        }

        // Retrieve raw RGBA bytes.
        let rgba_data = match self.image_bytes.get(&resource_id) {
            Some(data) => Arc::clone(data),
            None => {
                tracing::debug!(
                    resource_id = %resource_id,
                    "image bytes not registered — falling back to placeholder"
                );
                return false;
            }
        };

        // Validate byte count.
        let expected_bytes = (img_width as usize) * (img_height as usize) * 4;
        if rgba_data.len() != expected_bytes {
            tracing::warn!(
                resource_id = %resource_id,
                expected = expected_bytes,
                actual = rgba_data.len(),
                "image bytes length mismatch — falling back to placeholder"
            );
            return false;
        }
        let reduce_quality = self.degradation_policy.level
            >= tze_hud_scene::DegradationLevel::Simplified
            && (img_width > self.degradation_policy.texture_quality_threshold_px
                || img_height > self.degradation_policy.texture_quality_threshold_px);
        let (texture_width, texture_height) = if reduce_quality {
            (
                ((img_width as f32 * self.degradation_policy.texture_scale_factor).round() as u32)
                    .max(1),
                ((img_height as f32 * self.degradation_policy.texture_scale_factor).round() as u32)
                    .max(1),
            )
        } else {
            (img_width, img_height)
        };
        let scaled_rgba = reduce_quality.then(|| {
            downsample_rgba_nearest(
                &rgba_data,
                img_width,
                img_height,
                texture_width,
                texture_height,
            )
        });
        let upload_rgba = scaled_rgba.as_deref().unwrap_or(&rgba_data);
        if let Some(ledger) = &self.resident_ledger
            && ledger
                .reserve(
                    tze_hud_resource::ResidentClass::Resource,
                    format!("image:{resource_id}:gpu"),
                    upload_rgba.len() as u64,
                )
                .is_err()
        {
            return false;
        }

        // Create GPU texture.
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(&format!("img_tex_{resource_id}")),
            size: wgpu::Extent3d {
                width: texture_width,
                height: texture_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Upload pixel data.
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            upload_rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(texture_width * 4),
                rows_per_image: Some(texture_height),
            },
            wgpu::Extent3d {
                width: texture_width,
                height: texture_height,
                depth_or_array_layers: 1,
            },
        );

        // Create bind group.
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(&format!("img_bg_{resource_id}")),
            layout: &self.texture_rect_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.image_sampler),
                },
            ],
        });

        self.image_texture_cache.insert(
            resource_id,
            ImageTextureEntry {
                _texture: texture,
                bind_group,
                width: texture_width,
                height: texture_height,
            },
        );

        tracing::debug!(
            resource_id = %resource_id,
            width = img_width,
            height = img_height,
            "image texture uploaded to GPU"
        );

        true
    }

    /// Ensure a GPU texture exists for a status-bar icon SVG file path.
    ///
    /// Uses `ResourceId::of(path.as_bytes())` as the cache key so the texture
    /// is stored in `image_texture_cache` and drawn by the standard image pass.
    ///
    /// On cache hit, returns `true` immediately (no I/O).  On miss:
    /// 1. Reads the SVG bytes from `path` (filesystem).
    ///    NOTE: This is a blocking filesystem read on the compositor thread.
    ///    It only occurs once per unique SVG path (cache miss); subsequent
    ///    calls for the same path return immediately from cache or the
    ///    negative-cache set (`failed_icon_paths`).
    /// 2. Rasterizes at [`ICON_SIZE_PX`] × [`ICON_SIZE_PX`] via resvg/tiny-skia.
    /// 3. Uploads the RGBA pixmap to a `wgpu::Texture`.
    /// 4. Inserts into `image_texture_cache` under the path-derived `ResourceId`.
    ///
    /// Returns `false` if the file cannot be read or parsed.  In that case the
    /// `ResourceId` is added to `failed_icon_paths` so no further I/O is
    /// attempted for this path (graceful degradation — text-only fallback).
    pub(super) fn ensure_icon_texture(&mut self, path: &str) -> bool {
        let resource_id = ResourceId::of(path.as_bytes());
        if self.image_texture_cache.contains_key(&resource_id) {
            return true;
        }
        // Negative cache: skip repeated I/O for known-bad paths.
        if self.failed_icon_paths.contains(&resource_id) {
            return false;
        }

        let size = super::token_colors::ICON_SIZE_PX as u32;

        // Read SVG bytes from disk (blocking; occurs at most once per path).
        let svg_bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path, error = %e, "icon: failed to read SVG file");
                self.failed_icon_paths.insert(resource_id);
                return false;
            }
        };
        let svg_str = match std::str::from_utf8(&svg_bytes) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path, error = %e, "icon: SVG file is not valid UTF-8");
                self.failed_icon_paths.insert(resource_id);
                return false;
            }
        };

        // Apply design token substitution before parsing.
        // `resolve_token_placeholders` has an internal fast path for SVGs that
        // contain neither `{{` nor escape sequences — only a `contains` scan,
        // no allocation.  For SVGs with placeholders that cannot be resolved
        // (token missing), log a warning and degrade gracefully (skip icon).
        let resolved_svg;
        let svg_str = match resolve_token_placeholders(svg_str, &self.token_map) {
            Ok(s) => {
                resolved_svg = s;
                resolved_svg.as_str()
            }
            Err(missing_key) => {
                tracing::warn!(
                    path,
                    token_key = %missing_key,
                    "icon: unresolved design token in SVG; skipping icon"
                );
                self.failed_icon_paths.insert(resource_id);
                return false;
            }
        };

        // Rasterize via resvg (same pipeline as WidgetRenderer).
        let opts = resvg::usvg::Options::default();
        let tree = match resvg::usvg::Tree::from_str(svg_str, &opts) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path, error = %e, "icon: failed to parse SVG");
                self.failed_icon_paths.insert(resource_id);
                return false;
            }
        };
        let mut pixmap = match tiny_skia::Pixmap::new(size, size) {
            Some(p) => p,
            None => {
                tracing::warn!(path, size, "icon: failed to allocate pixmap");
                self.failed_icon_paths.insert(resource_id);
                return false;
            }
        };
        // Scale uniformly with centering (same transform logic as widget rasterization).
        let svg_size = tree.size();
        let sx = size as f32 / svg_size.width();
        let sy = size as f32 / svg_size.height();
        let uniform_scale = sx.min(sy);
        let rendered_w = svg_size.width() * uniform_scale;
        let rendered_h = svg_size.height() * uniform_scale;
        let offset_x = (size as f32 - rendered_w) * 0.5;
        let offset_y = (size as f32 - rendered_h) * 0.5;
        let transform = tiny_skia::Transform::from_translate(offset_x, offset_y)
            .post_scale(uniform_scale, uniform_scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        // Upload to GPU.
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(&format!("icon_tex_{path}")),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixmap.data(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size * 4),
                rows_per_image: Some(size),
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
        );
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(&format!("icon_bg_{path}")),
            layout: &self.texture_rect_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.image_sampler),
                },
            ],
        });
        self.image_texture_cache.insert(
            resource_id,
            ImageTextureEntry {
                _texture: texture,
                bind_group,
                width: size,
                height: size,
            },
        );
        tracing::debug!(path, size, "icon texture rasterized and uploaded to GPU");
        true
    }

    /// Evict cached GPU textures for resources no longer referenced in the scene.
    ///
    /// Call once per frame after rendering. `referenced_ids` is the set of
    /// `ResourceId`s that appeared in zone publications or tile nodes during
    /// this frame. Any cache entry not in this set is dropped.
    pub fn evict_unused_image_textures(&mut self, referenced_ids: &HashSet<ResourceId>) {
        let stale_texture_ids =
            unreferenced_resource_ids(self.image_texture_cache.keys(), referenced_ids);
        let stale_byte_ids = unreferenced_resource_ids(self.image_bytes.keys(), referenced_ids);
        if let Some(ledger) = &self.resident_ledger {
            for id in &stale_texture_ids {
                ledger.release_evicted(
                    tze_hud_resource::ResidentClass::Resource,
                    &tze_hud_resource::AllocationId(format!("image:{id}:gpu")),
                );
            }
            for id in &stale_byte_ids {
                ledger.release_evicted(
                    tze_hud_resource::ResidentClass::Resource,
                    &tze_hud_resource::AllocationId(format!("image:{id}:cpu")),
                );
            }
        }
        self.image_texture_cache
            .retain(|id, _| referenced_ids.contains(id));
        // Also evict bytes and dims for resources no longer referenced.
        self.image_bytes.retain(|id, _| referenced_ids.contains(id));
        self.image_dims.retain(|id, _| referenced_ids.contains(id));
    }
}

#[cfg(test)]
mod degradation_texture_tests {
    use std::collections::{HashMap, HashSet};

    use super::{downsample_rgba_nearest, unreferenced_resource_ids};
    use tze_hud_scene::types::ResourceId;

    #[test]
    fn current_frame_references_are_never_eviction_candidates() {
        let current = ResourceId::of(b"current-frame");
        let stale = ResourceId::of(b"stale-frame");
        let cached = HashMap::from([(current, ()), (stale, ())]);
        let referenced = HashSet::from([current]);

        let candidates = unreferenced_resource_ids(cached.keys(), &referenced);

        assert_eq!(candidates, vec![stale]);
    }

    #[test]
    fn nearest_downsample_is_deterministic_and_preserves_rgba_pixels() {
        let source = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        assert_eq!(
            downsample_rgba_nearest(&source, 2, 2, 1, 1),
            vec![1, 2, 3, 4]
        );
    }
}
