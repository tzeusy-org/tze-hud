//! Animation-state update methods for the compositor.
//!
//! Moved from `renderer/mod.rs` (the animation-update method cluster, approx.
//! L4288–5095 at plan date) by Step R-5 of the renderer module split
//! (hud-fgryk).  No logic was changed; only visibility modifiers were added
//! where Rust's module-privacy rules require them.
//!
//! Animation-state **types** (`ZoneAnimationState`, `PublicationAnimationState`,
//! `StreamRevealState`) remain in `draw_cmds.rs` (placed there by Step R-1).
//! This file contains only the `impl Compositor` methods that operate on those
//! types.

use std::collections::HashMap;
use std::sync::Arc;

use tze_hud_scene::DegradationLevel;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;

use super::Compositor;
use super::draw_cmds::{
    NOTIFICATION_FADE_OUT_MS, PortalTileStreamReveal, PubKey, PublicationAnimationState,
    StreamRevealState, ZoneAnimationState, common_prefix_len, derive_word_breakpoints,
};

impl Compositor {
    /// Update zone animation states before each frame.
    ///
    /// Starts fade-in animations for newly-published zones and fade-out
    /// animations for zones that just lost their last publish.
    ///
    /// Also handles zone unregistration: zones that were active and have since
    /// been removed from the registry are treated as cleared (no fade-out is
    /// possible since the zone_def is gone, so the state is simply pruned).
    ///
    /// Prunes completed transitions.
    pub(crate) fn update_zone_animations(&mut self, scene: &SceneGraph) {
        // Build current active-zone set (zone_name → has active publishes).
        let current_active: HashMap<String, bool> = scene
            .zone_registry
            .active_publishes
            .iter()
            .map(|(name, pubs)| (name.clone(), !pubs.is_empty()))
            .collect();

        // Detect publish transitions within currently-registered zones.
        for (zone_name, &is_active) in &current_active {
            let was_active = self
                .prev_active_zones
                .get(zone_name)
                .copied()
                .unwrap_or(false);

            if is_active && !was_active {
                // Zone just received its first publish — start fade-in.
                //
                // Transition interrupt semantics: if a fade-out is currently in
                // progress (target_opacity == 0.0), we MUST start the fade-in from
                // the current composite opacity rather than from 0 to prevent a
                // blank frame.  Per spec §Subtitle Contention Policy: "the fade-out
                // MUST be cancelled immediately and the new content MUST begin its
                // transition_in_ms fade-in from the current composite opacity (not
                // from zero)."
                if let Some(zone_def) = scene.zone_registry.zones.get(zone_name) {
                    if let Some(ms) = zone_def.rendering_policy.transition_in_ms {
                        if ms > 0 {
                            let new_state = if let Some(existing) =
                                self.zone_animation_states.get(zone_name)
                            {
                                if existing.target_opacity == 0.0 {
                                    // Interrupt active fade-out: begin fade-in from
                                    // current opacity so there is no blank frame.
                                    ZoneAnimationState::fade_in_from(ms, existing.current_opacity())
                                } else {
                                    ZoneAnimationState::fade_in(ms)
                                }
                            } else {
                                ZoneAnimationState::fade_in(ms)
                            };
                            self.zone_animation_states
                                .insert(zone_name.clone(), new_state);
                        }
                    }
                }
            } else if !is_active && was_active {
                // Zone just lost its last publish — start fade-out.
                if let Some(zone_def) = scene.zone_registry.zones.get(zone_name) {
                    if let Some(ms) = zone_def.rendering_policy.transition_out_ms {
                        if ms > 0 {
                            self.zone_animation_states
                                .insert(zone_name.clone(), ZoneAnimationState::fade_out(ms));
                        }
                    }
                }
            }
        }

        // Detect zone unregistration: zones that were previously tracked but
        // are now absent from active_publishes (zone was removed from registry).
        // Since zone_def is gone, no fade-out animation is possible; we simply
        // prune any in-flight animation state for that zone immediately.
        self.zone_animation_states
            .retain(|zone_name, _| current_active.contains_key(zone_name));

        // Prune completed transitions (reached target opacity).
        self.zone_animation_states
            .retain(|_, state| !state.is_complete());

        self.prev_active_zones = current_active;
    }

    /// Update per-portal-tile fade animation state (§6.3 — transition tokens).
    ///
    /// Runs the same appear/disappear transition logic as [`update_zone_animations`]
    /// but for portal tiles (scrollable tiles identified by a registered
    /// [`TileScrollConfig`]).  Durations are sourced from the
    /// `portal.transition.in_ms` and `portal.transition.out_ms` design tokens
    /// in `self.token_map`; no literal durations appear here.
    ///
    /// A tile is considered "has content" when `tile.root_node.is_some()`.
    ///
    /// - **Appear**: a scrollable tile whose `root_node` just became `Some`
    ///   starts a `fade_in(transition_in_ms)` animation (if `> 0`).
    /// - **Disappear**: a scrollable tile whose `root_node` just became `None`
    ///   starts a `fade_out(transition_out_ms)` animation (if `> 0`).
    ///
    /// Completed transitions are pruned after each update.
    ///
    /// Must be called once per frame alongside `update_zone_animations`.
    pub(crate) fn update_portal_tile_animations(&mut self, scene: &SceneGraph) {
        // Resolve transition durations from design tokens (§6.1 — no literals).
        let transition_in_ms: u32 = self
            .token_map
            .get("portal.transition.in_ms")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(120); // matches PortalPartTokens::defaults::TRANSITION_IN_MS
        let transition_out_ms: u32 = self
            .token_map
            .get("portal.transition.out_ms")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(80); // matches PortalPartTokens::defaults::TRANSITION_OUT_MS

        // Build current content state for all scrollable tiles.
        let current: HashMap<SceneId, bool> = scene
            .tiles
            .values()
            .filter(|tile| scene.tile_scroll_config(tile.id).is_some())
            .map(|tile| (tile.id, tile.root_node.is_some()))
            .collect();

        for (&tile_id, &has_content) in &current {
            let had_content = self
                .prev_portal_tile_has_content
                .get(&tile_id)
                .copied()
                .unwrap_or(false);

            if has_content && !had_content {
                // Tile just received content — start fade-in.
                // Interrupt semantics mirror zone animation: start from the
                // current opacity if a fade-out is in progress (no blank frame).
                //
                // Portal tiles are *displayed* through the EaseInOut curve (see
                // `portal_tile_anim_opacity`), so the interrupted fade-in MUST be
                // seeded from the eased (on-screen) opacity rather than the linear
                // `current_opacity()`. Seeding from the linear value would make the
                // new fade-in start at a different opacity than the frame just
                // rendered, producing a visible jump (hud-uir0w). Reusing
                // `portal_tile_anim_opacity` keeps the seed tied to the exact
                // displayed value, so the easing curve can never drift between the
                // two paths.
                if transition_in_ms > 0 {
                    let new_state =
                        if let Some(existing) = self.portal_tile_anim_states.get(&tile_id) {
                            if existing.target_opacity == 0.0 {
                                ZoneAnimationState::fade_in_from(
                                    transition_in_ms,
                                    self.portal_tile_anim_opacity(tile_id),
                                )
                            } else {
                                ZoneAnimationState::fade_in(transition_in_ms)
                            }
                        } else {
                            ZoneAnimationState::fade_in(transition_in_ms)
                        };
                    self.portal_tile_anim_states.insert(tile_id, new_state);
                }
            } else if !has_content && had_content {
                // Tile just lost content — start fade-out.
                if transition_out_ms > 0 {
                    self.portal_tile_anim_states
                        .insert(tile_id, ZoneAnimationState::fade_out(transition_out_ms));
                }
            }
        }

        // Prune animation states for tiles that are no longer scrollable.
        self.portal_tile_anim_states
            .retain(|tile_id, _| current.contains_key(tile_id));

        // Prune completed transitions.
        self.portal_tile_anim_states
            .retain(|_, state| !state.is_complete());

        self.prev_portal_tile_has_content = current;
    }

    /// Return the current portal tile animation opacity for `tile_id`.
    ///
    /// Returns `1.0` when no animation is in progress (fully visible).
    /// Used by tile text collection and background rendering to fade portal
    /// tiles in/out using the token-configured duration.
    ///
    /// The fade is shaped by [`Easing::EaseInOut`] (hud-bq0gl.10) so portal
    /// collapse/expand transitions accelerate and decelerate rather than ramping
    /// linearly. Endpoints (`t=0`, `t=1`) are unchanged, so this is a motion-only
    /// refinement: a freshly-appearing tile still starts fully transparent and a
    /// completed transition still rests at full opacity.
    #[inline]
    pub(crate) fn portal_tile_anim_opacity(&self, tile_id: SceneId) -> f32 {
        if self.degradation_policy.level >= DegradationLevel::Simplified {
            return 1.0;
        }
        self.portal_tile_anim_states
            .get(&tile_id)
            .map(|s| s.current_opacity_eased(super::easing::Easing::EaseInOut))
            .unwrap_or(1.0)
    }

    /// Update per-zone streaming word-by-word reveal state.
    ///
    /// Must be called once per frame (after `update_zone_animations`).
    ///
    /// For each zone with a `LatestWins` or `Replace` publication that has
    /// non-empty breakpoints:
    /// - If no reveal state exists or the current pub key doesn't match, start
    ///   a fresh reveal from segment 0 (latest-wins cancels previous streaming).
    /// - If reveal state exists for the current pub key, advance by one frame.
    /// - Zones with empty breakpoints (or no StreamText) have their reveal state
    ///   pruned so text renders at full immediately.
    ///
    /// Per spec §Subtitle Streaming Word-by-Word Reveal:
    /// - Breakpoints identify byte offsets for progressive reveal.
    /// - Empty breakpoints → reveal all at once.
    /// - Replacement during streaming → cancel old reveal, start new.
    pub(crate) fn update_stream_reveals(&mut self, scene: &SceneGraph) {
        // Collect zones whose latest publish has breakpoints.
        let mut active_keys: HashMap<String, PubKey> = HashMap::new();

        for (zone_name, publishes) in &scene.zone_registry.active_publishes {
            if publishes.is_empty() {
                continue;
            }
            let zone_def = match scene.zone_registry.zones.get(zone_name) {
                Some(z) => z,
                None => continue,
            };
            // Only LatestWins/Replace zones get streaming reveal (single occupant).
            if !matches!(
                zone_def.contention_policy,
                ContentionPolicy::LatestWins | ContentionPolicy::Replace
            ) {
                continue;
            }
            let latest = &publishes[publishes.len() - 1];
            // Only StreamText with non-empty breakpoints gets progressive reveal.
            if !matches!(&latest.content, ZoneContent::StreamText(_))
                || latest.breakpoints.is_empty()
            {
                continue;
            }
            let pub_key: PubKey = (
                latest.published_at_wall_us,
                latest.publisher_namespace.clone(),
            );
            active_keys.insert(zone_name.clone(), pub_key);
        }

        // Prune reveal states for zones no longer streaming.
        self.stream_reveal_states
            .retain(|zone_name, _| active_keys.contains_key(zone_name));

        // Update or create reveal states.
        for (zone_name, pub_key) in &active_keys {
            let publishes = match scene.zone_registry.active_publishes.get(zone_name) {
                Some(p) if !p.is_empty() => p,
                _ => continue,
            };
            let latest = &publishes[publishes.len() - 1];

            let state = self.stream_reveal_states.get(zone_name);
            let need_reset = state.map(|s| &s.pub_key != pub_key).unwrap_or(true);

            if need_reset {
                // New publication (latest-wins replaced) or first reveal — start fresh.
                let new_state = StreamRevealState::new(pub_key.clone(), latest.breakpoints.clone());
                self.stream_reveal_states
                    .insert(zone_name.clone(), new_state);
            } else if let Some(state) = self.stream_reveal_states.get_mut(zone_name) {
                // Advance existing reveal by one frame.
                state.advance();
            }
        }
    }

    /// Update per-portal-tile streaming-reveal state (hud-bl7yi).
    ///
    /// Must be called once per frame (after `update_portal_tile_animations`),
    /// the portal-tile analogue of [`Compositor::update_stream_reveals`].
    ///
    /// For each scrollable (portal) tile whose root subtree contains a markdown
    /// content node:
    /// - **First sight** of the tile → record a *settled* anchor so pre-existing
    ///   content is not faded in (the whole-tile appear fade in
    ///   `update_portal_tile_animations` already covers first appearance).
    /// - **Unchanged** plain-text → advance any in-flight reveal by one frame.
    /// - **Grown** plain-text (a genuine append) → start a fresh reveal that
    ///   fades the `[common_prefix, new_len)` region in segment-by-segment.
    /// - **Other change** (same-length edit / shrink such as caret blink or a
    ///   head-trim) → re-anchor settled, with no fade, so same-length churn never
    ///   re-triggers an animation.
    ///
    /// Plain-text is sourced from the commit-time-primed markdown cache so the
    /// reveal's byte offsets line up exactly with the `styled_runs` the renderer
    /// lays out. Tiles whose content node carries pixel-bearing color runs (the
    /// legacy raw path) are skipped — their offsets index raw content, not the
    /// stripped plain-text, so a fade cannot be aligned safely.
    pub(crate) fn update_portal_tile_reveals(&mut self, scene: &SceneGraph) {
        // Phase 1: snapshot (tile_id, node_id, plain_text) for **every eligible
        // markdown node** under every portal tile — not just the first-eligible
        // one (hud-tbdfx). Done in a separate pass so the immutable cache/scene
        // borrow is released before we mutate `self.portal_tile_reveal_states`.
        //
        // Reveal state is keyed per `(tile_id, node_id)`. Keying per node (vs
        // per tile) is what makes the tracker robust to the *first eligible*
        // markdown node changing between frames: on a portal input tile the
        // composer draft node's pixel-bearing color runs toggle its eligibility,
        // so a tile-keyed tracker flips between the draft node's plain-text and
        // the (larger) settled history node's plain-text and treats the swap as
        // growth — spuriously word-revealing the whole history on every submit.
        // Per-node keying diffs each node only against its own prior snapshot.
        let cache = self.markdown_cache();
        let mut current: Vec<(SceneId, SceneId, Arc<str>)> = Vec::new();
        for tile in scene.tiles.values() {
            if scene.tile_scroll_config(tile.id).is_none() {
                continue; // not a portal/scrollable tile
            }
            let Some(root) = tile.root_node else {
                continue;
            };
            self.collect_portal_tile_plain_texts(scene, tile.id, root, &cache, &mut current);
        }

        // Prune reveal states for `(tile, node)` pairs no longer present.
        // `current` holds only a handful of nodes, so a direct membership scan
        // avoids the per-frame heap allocation a temporary `HashSet` would cost.
        self.portal_tile_reveal_states
            .retain(|(tile_id, node_id), _| {
                current
                    .iter()
                    .any(|(tid, nid, _)| tid == tile_id && nid == node_id)
            });

        for (tile_id, node_id, plain) in current {
            match self.portal_tile_reveal_states.get_mut(&(tile_id, node_id)) {
                None => {
                    // First sight of this node — anchor without fading
                    // pre-existing content (a node that just became eligible,
                    // e.g. the draft node losing its pixel runs, must NOT fade
                    // its already-settled text in).
                    self.portal_tile_reveal_states
                        .insert((tile_id, node_id), PortalTileStreamReveal::settled(plain));
                }
                Some(state) => {
                    if state.plain_text.as_ref() == plain.as_ref() {
                        // No content change — advance any in-flight reveal.
                        state.advance();
                    } else if plain.len() > state.plain_text.len() {
                        // This node's content grew → fade in the appended
                        // (changed-suffix) region. `common_prefix_len` locates
                        // where the new snapshot diverges from *this same node's*
                        // prior snapshot; everything after fades segment-by-
                        // segment. (Transcript lines land mid-string before the
                        // trailing composer line, so we reveal the changed suffix
                        // rather than requiring a strict tail append.)
                        let start = common_prefix_len(state.plain_text.as_ref(), plain.as_ref());
                        let breakpoints = derive_word_breakpoints(plain.as_ref(), start);
                        *state = PortalTileStreamReveal::new(plain, start, breakpoints);
                    } else {
                        // Same-length or shrinking change (caret blink, status
                        // edit, head-trim) → re-anchor settled, no fade.
                        *state = PortalTileStreamReveal::settled(plain);
                    }
                }
            }
        }
    }

    /// Collect `(tile_id, node_id, plain_text)` for **every eligible
    /// `TextMarkdownNode`** in the subtree rooted at `node_id`, using the
    /// commit-time-primed markdown cache (hud-tbdfx).
    ///
    /// A node contributes an entry when it is a `TextMarkdownNode` that does not
    /// carry pixel-bearing color runs (the legacy raw path indexes raw content,
    /// not the stripped plain-text, so a fade cannot be aligned) and the cache
    /// already holds its parse (a cold first frame is picked up on the next).
    /// The pushed `Arc<str>` is a refcount bump on the cache's shared plain-text
    /// — no string copy. Traversal always descends into children so *all*
    /// eligible nodes in the tile are tracked independently, not just the first.
    fn collect_portal_tile_plain_texts(
        &self,
        scene: &SceneGraph,
        tile_id: SceneId,
        node_id: SceneId,
        cache: &crate::markdown::MarkdownCache,
        out: &mut Vec<(SceneId, SceneId, Arc<str>)>,
    ) {
        let Some(node) = scene.nodes.get(&node_id) else {
            return;
        };
        if let NodeData::TextMarkdown(tm) = &node.data {
            if !crate::text::markdown_node_has_pixel_runs(tm) {
                let key = self
                    .node_key_cache
                    .get(&node_id)
                    .copied()
                    .unwrap_or_else(|| {
                        // Portal transcript nodes key with the portal token set
                        // (hud-3ryie); the primed `node_key_cache` normally makes
                        // this fallback unreachable.
                        crate::markdown::MarkdownCache::compute_key(
                            &tm.content,
                            &self.markdown_tokens,
                        )
                    });
                if let Some(parsed) = cache.get_by_key(&key) {
                    out.push((tile_id, node_id, Arc::clone(&parsed.plain_text)));
                }
            }
        }
        for child_id in &node.children {
            self.collect_portal_tile_plain_texts(scene, tile_id, *child_id, cache, out);
        }
    }

    /// Update per-publication fade-out animation state for Stack zone publications.
    ///
    /// For each active publication in a Stack zone:
    ///
    /// 1. If it is new (not in `pub_animation_states`), insert a fresh
    ///    [`PublicationAnimationState`] whose fade delay comes from
    ///    [`Compositor::publication_fade_delay_ms`]. A publication with no expiry
    ///    is held until cleared and never fades.
    /// 2. If its `expires_at_wall_us` changed (`hud_hold`), retarget the state
    ///    so the single fade deadline moves (or disappears when held).
    /// 3. Call `tick()` to check whether the delay has elapsed and start the fade.
    ///
    /// Stale entries (publications no longer present in `active_publishes`) are
    /// pruned from `pub_animation_states` by this method.
    ///
    /// After this call, use [`Compositor::prune_faded_publications`] to remove
    /// publications whose fade-out has fully completed from the scene graph.
    ///
    /// Call order per frame: `update_zone_animations` → `update_publication_animations`
    /// → `prune_faded_publications(scene)` → render.
    pub fn update_publication_animations(&mut self, scene: &SceneGraph) {
        let now_us = scene.now_wall_us();
        for (zone_name, publishes) in &scene.zone_registry.active_publishes {
            let zone_def = match scene.zone_registry.zones.get(zone_name) {
                Some(z) => z,
                None => continue,
            };
            // Only Stack zones get per-publication TTL fade-out.
            if !matches!(zone_def.contention_policy, ContentionPolicy::Stack { .. }) {
                continue;
            }

            let zone_states = self
                .pub_animation_states
                .entry(zone_name.clone())
                .or_default();

            // Build the set of currently-active pub keys for this zone.
            let active_keys: std::collections::HashSet<PubKey> = publishes
                .iter()
                .map(|r| (r.published_at_wall_us, r.publisher_namespace.clone()))
                .collect();

            // Prune stale entries (publications removed from active_publishes).
            zone_states.retain(|k, _| active_keys.contains(k));

            // Ensure every active publication has an animation state; tick existing ones.
            for record in publishes {
                let ttl_ms = Self::publication_fade_delay_ms(record, now_us);
                let key: PubKey = (
                    record.published_at_wall_us,
                    record.publisher_namespace.clone(),
                );
                let state = zone_states.entry(key).or_insert_with(|| {
                    PublicationAnimationState::new(ttl_ms, record.expires_at_wall_us)
                });
                if state.source_expiry_us != record.expires_at_wall_us {
                    state.retarget(ttl_ms, record.expires_at_wall_us);
                }
                state.tick();
            }
        }

        // Prune zones no longer present in active_publishes.
        self.pub_animation_states
            .retain(|zone_name, _| scene.zone_registry.active_publishes.contains_key(zone_name));
    }

    /// Delay (ms from `now_us`) until the fade-out of one publication begins,
    /// or `None` when it has no expiry and is held until cleared.
    ///
    /// The fade starts `NOTIFICATION_FADE_OUT_MS` before the expiry so it
    /// completes before `drain_expired_zone_publications` removes the record
    /// (e.g. 14 850 ms for a 15 s warning). Already expired: fade at once.
    pub(super) fn publication_fade_delay_ms(
        record: &ZonePublishRecord,
        now_us: u64,
    ) -> Option<u64> {
        record.expires_at_wall_us.map(|exp_us| {
            (exp_us.saturating_sub(now_us) / 1_000).saturating_sub(NOTIFICATION_FADE_OUT_MS as u64)
        })
    }

    /// Look up the current opacity for a publication in `pub_animation_states`.
    ///
    /// Returns 1.0 if no animation state is found (publication is fully visible).
    pub(crate) fn pub_opacity(&self, zone_name: &str, record: &ZonePublishRecord) -> f32 {
        if self.degradation_policy.level >= DegradationLevel::Simplified {
            return 1.0;
        }
        let key: PubKey = (
            record.published_at_wall_us,
            record.publisher_namespace.clone(),
        );
        self.pub_animation_states
            .get(zone_name)
            .and_then(|zone_states| zone_states.get(&key))
            .map(|s| s.current_opacity())
            .unwrap_or(1.0)
    }

    /// Remove publications from the scene whose fade-out animation has completed.
    ///
    /// This method MUST be called before rendering so that fully-faded publications
    /// are absent from `active_publishes` during the frame.  After removal,
    /// remaining notifications reflow naturally in the next frame (slot positions
    /// are recalculated from the updated `active_publishes` slice each frame).
    ///
    /// Intended call site: runtime frame loop, between scene commit and render,
    /// alongside `SceneGraph::drain_expired_zone_publications`.
    pub fn prune_faded_publications(&mut self, scene: &mut SceneGraph) {
        for (zone_name, zone_states) in &self.pub_animation_states {
            let publishes = match scene.zone_registry.active_publishes.get_mut(zone_name) {
                Some(p) => p,
                None => continue,
            };
            let before = publishes.len();
            publishes.retain(|record| {
                let key: PubKey = (
                    record.published_at_wall_us,
                    record.publisher_namespace.clone(),
                );
                !zone_states
                    .get(&key)
                    .map(|s| s.is_fade_complete())
                    .unwrap_or(false)
            });
            if publishes.len() < before {
                scene.version += 1;
            }
        }
        // Remove empty active_publishes entries.
        scene
            .zone_registry
            .active_publishes
            .retain(|_, v| !v.is_empty());
    }

    /// Advance per-portal-tile scroll smoothing toward the scene's authoritative
    /// scroll targets (smooth scroll / animated follow-tail, hud-bq0gl.10).
    ///
    /// Must be called once per frame, before any render pass reads a tile's
    /// displayed scroll offset via [`Compositor::display_tile_scroll_offset`].
    /// The shared frame body ([`Compositor::build_frame_vertices`]) drives it so
    /// all three render entry points advance the smoothers exactly once.
    ///
    /// No-op when [`scroll_smoothing_enabled`](Compositor::scroll_smoothing_enabled)
    /// is `false` (headless): those paths read the raw scene offset directly so
    /// deterministic golden tests are unaffected.
    ///
    /// A newly-observed tile starts *settled* on its current offset (no initial
    /// jump); only subsequent target changes — user scroll or follow-tail
    /// content appends — animate. User scroll stays authoritative (RFC 0013
    /// §3.2): only the visual catch-up is eased, never the target.
    pub(crate) fn update_scroll_smoothing(&mut self, scene: &SceneGraph) {
        if !self.scroll_smoothing_enabled {
            return;
        }

        let now = std::time::Instant::now();
        let dt_ms = self
            .last_scroll_smooth_at
            .map(|t| now.duration_since(t).as_secs_f32() * 1_000.0)
            .unwrap_or(0.0);
        self.last_scroll_smooth_at = Some(now);

        for tile in scene.tiles.values() {
            // Only portal (scrollable) tiles smooth their scroll offset.
            if scene.tile_scroll_config(tile.id).is_none() {
                continue;
            }
            let (target_x, target_y) = scene.tile_scroll_offset_local(tile.id);
            self.scroll_smoothers
                .entry(tile.id)
                .or_insert_with(|| super::easing::ScrollSmoother::new(target_x, target_y))
                .advance(target_x, target_y, dt_ms);
        }

        // Prune smoothers for tiles that are no longer scrollable / present.
        self.scroll_smoothers
            .retain(|id, _| scene.tile_scroll_config(*id).is_some());
    }

    /// Return the *displayed* (smoothed) scroll offset for a tile.
    ///
    /// When smoothing is enabled (windowed) and a smoother exists for the tile,
    /// returns the eased displayed offset; otherwise returns the scene's raw
    /// authoritative offset. The fallback keeps non-portal tiles and the
    /// headless path exact.
    #[inline]
    pub(crate) fn display_tile_scroll_offset(
        &self,
        scene: &SceneGraph,
        tile_id: SceneId,
    ) -> (f32, f32) {
        if self.scroll_smoothing_enabled {
            if let Some(smoother) = self.scroll_smoothers.get(&tile_id) {
                return smoother.displayed();
            }
        }
        scene.tile_scroll_offset_local(tile_id)
    }

    /// Publish the per-tile *displayed* (smoothed/lagged) scroll offsets into the
    /// scene so the live hit-test path maps pointer coordinates against the same
    /// offset the renderer drew with (hud-3lynp).
    ///
    /// Must be called once per frame, after [`update_scroll_smoothing`] has
    /// advanced the smoothers (so the published offsets reflect this frame's
    /// displayed state) and with `&mut SceneGraph` available.
    ///
    /// When smoothing is disabled (headless/snap) this clears any published
    /// overrides so hit-testing falls back to the authoritative offset and
    /// deterministic golden tests are unaffected (acceptance: behavior unchanged
    /// when no smoothing is active).
    ///
    /// [`update_scroll_smoothing`]: Compositor::update_scroll_smoothing
    pub(crate) fn publish_displayed_scroll_offsets(&self, scene: &mut SceneGraph) {
        if !self.scroll_smoothing_enabled {
            scene.clear_displayed_tile_scroll_offsets();
            return;
        }
        // Drop overrides for tiles that no longer smooth (no longer scrollable /
        // removed) so a stale displayed offset can never outlive its smoother.
        scene.retain_displayed_tile_scroll_offsets(|id| self.scroll_smoothers.contains_key(&id));
        for (tile_id, smoother) in &self.scroll_smoothers {
            let (x, y) = smoother.displayed();
            scene.set_displayed_tile_scroll_offset(*tile_id, x, y);
        }
    }

    /// Earliest future instant at which idle content needs a frame with no
    /// other event: the next notification fade start or composer caret toggle.
    /// The windowed loop folds this into its wake deadline so countdowns and a
    /// blinking caret cost one wake per transition, not a frame per vsync.
    pub fn next_animation_deadline(&self) -> Option<std::time::Instant> {
        let fade_starts = self
            .pub_animation_states
            .values()
            .flat_map(|zone| zone.values())
            .filter_map(|s| s.fade_start_deadline());
        let caret_toggle = self.local_composer.as_ref().map(|_| {
            let half = crate::renderer::image_cache::CARET_BLINK_HALF_PERIOD.as_nanos();
            let elapsed = self.composer_caret_blink_start.elapsed().as_nanos();
            let next_toggle_ns = (elapsed / half + 1) * half;
            self.composer_caret_blink_start + std::time::Duration::from_nanos(next_toggle_ns as u64)
        });
        // A widget mid-transition changes pixels every tick until it lands;
        // an idle widget contributes nothing.
        let widget_tick = self
            .widget_renderer
            .as_ref()
            .filter(|wr| wr.has_active_transition())
            .map(|_| std::time::Instant::now() + crate::widget::WIDGET_TRANSITION_TICK);
        fade_starts.chain(caret_toggle).chain(widget_tick).min()
    }

    /// Whether any per-frame animation, fade, reveal, or scroll smoothing is
    /// still in flight — i.e. the next presented frame's pixels would differ
    /// from the last even though `scene.version` is unchanged.
    ///
    /// The windowed frame loop's idle render gate (hud-ilivg) consults this so it
    /// can skip the build/encode/present pass only when BOTH the scene version is
    /// unchanged AND nothing is animating. Returning `true` here is what keeps an
    /// in-flight eased transition, TTL fade-out, word-by-word reveal, or
    /// smooth-scroll catch-up advancing instead of freezing.
    ///
    /// All checks read animation state seeded by a prior rendered frame: the
    /// scene-version bump that *starts* any animation forces at least one render
    /// (via the version check), which populates these maps; from then on this
    /// predicate sustains the animation until it completes and self-prunes.
    pub fn has_inflight_animation(&self, scene: &SceneGraph) -> bool {
        // Zone fade-in / fade-out transitions (subtitles, content/chrome zones).
        // Completed transitions are pruned by `update_zone_animations`, but a
        // just-completed entry may linger for one frame — `!is_complete()` is the
        // authoritative in-flight test.
        if self
            .zone_animation_states
            .values()
            .any(|s| !s.is_complete())
        {
            return true;
        }

        // Portal-tile fade-in / fade-out transitions (§6.3 collapse/expand).
        if self
            .portal_tile_anim_states
            .values()
            .any(|s| !s.is_complete())
        {
            return true;
        }

        // Per-publication TTL fade-out (Stack notifications). Only a publication
        // that has started fading is in flight. One still counting down to its
        // fade is idle: `next_animation_deadline` wakes the loop at the fade
        // start instead of rendering identical frames for the whole TTL.
        if self
            .pub_animation_states
            .values()
            .any(|zone| zone.values().any(|s| s.is_fading()))
        {
            return true;
        }

        // Widget parameter transition still easing toward its target.
        if self
            .widget_renderer
            .as_ref()
            .is_some_and(|wr| wr.has_active_transition())
        {
            return true;
        }

        // Streaming word-by-word reveal still progressing through breakpoints.
        if self.stream_reveal_states.values().any(|s| s.is_revealing()) {
            return true;
        }

        // Portal-tile per-segment streaming-reveal fade still in flight
        // (hud-bl7yi). An active StreamFadeRamp must keep the tile rendering so
        // the fade advances instead of freezing mid-reveal (#943).
        if self
            .portal_tile_reveal_states
            .values()
            .any(|s| s.is_revealing())
        {
            return true;
        }

        // Smooth-scroll / animated follow-tail catch-up still moving toward its
        // authoritative target. Only meaningful when smoothing is enabled
        // (windowed); headless snaps and never registers smoothers.
        if self.scroll_smoothing_enabled {
            for (&tile_id, smoother) in &self.scroll_smoothers {
                let (target_x, target_y) = scene.tile_scroll_offset_local(tile_id);
                if !smoother.is_settled(target_x, target_y) {
                    return true;
                }
            }
        }

        false
    }
}
