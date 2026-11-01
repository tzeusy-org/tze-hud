//! Live appearance (hud-h51u7.5): the design tokens the HUD draws with,
//! changed while it runs by the control panel or by editing
//! `[design_tokens]` in the config file.
//!
//! [`Appearance::apply`] takes a new `[design_tokens]` table, checks it as
//! config loading would, resolves it (canonical defaults → theme →
//! overrides), restyles the scene (zone policies, widget SVGs re-resolved
//! from their bundles) and publishes the result in [`LiveTokens`]. The
//! compositor thread and the main thread pick it up on their next turn
//! through [`LiveTokens::if_changed`], an atomic load when nothing changed.
//!
//! What still needs a restart: tile placement and the MCP widget transition
//! time, both read once at startup, and widget text fonts (widgets render
//! with the bundled faces).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::Mutex;
use tze_hud_config::policy_builder::build_all_effective_policies;
use tze_hud_config::themes::{check_design_tokens, resolve_config_tokens};
use tze_hud_config::tokens::DesignTokenMap;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::render_wake::RenderWakeNotifier;
use tze_hud_scene::types::{RenderingPolicy, ZoneRegistry};

/// The resolved tokens now, and the `[design_tokens]` table they came from.
pub(crate) struct LiveTokens {
    current: RwLock<(DesignTokenMap, Arc<DesignTokenMap>)>,
    generation: AtomicU64,
    compositor_wake: RenderWakeNotifier,
    main_wake: RenderWakeNotifier,
}

impl LiveTokens {
    pub(crate) fn new(
        table: DesignTokenMap,
        resolved: DesignTokenMap,
        compositor_wake: RenderWakeNotifier,
        main_wake: RenderWakeNotifier,
    ) -> Self {
        Self {
            current: RwLock::new((table, Arc::new(resolved))),
            generation: AtomicU64::new(0),
            compositor_wake,
            main_wake,
        }
    }

    /// The `[design_tokens]` table in effect.
    pub(crate) fn table(&self) -> DesignTokenMap {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone()
    }

    /// The resolved token map in effect.
    pub(crate) fn resolved(&self) -> Arc<DesignTokenMap> {
        Arc::clone(&self.current.read().unwrap_or_else(|e| e.into_inner()).1)
    }

    /// The resolved map when it changed since `seen`, updating `seen`.
    pub(crate) fn if_changed(&self, seen: &mut u64) -> Option<Arc<DesignTokenMap>> {
        let now = self.generation.load(Ordering::Acquire);
        (now != *seen).then(|| {
            *seen = now;
            self.resolved()
        })
    }

    /// Replace the table with one that resolves the same; nobody repaints.
    fn respell(&self, table: DesignTokenMap) {
        self.current.write().unwrap_or_else(|e| e.into_inner()).0 = table;
    }

    fn publish(&self, table: DesignTokenMap, resolved: DesignTokenMap) {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = (table, Arc::new(resolved));
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.compositor_wake.notify();
        self.main_wake.notify();
    }
}

/// The last reload of `[design_tokens]` from the config file, for
/// `/admin/status`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ReloadStatus {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Unix time of the reload, in ms.
    pub at_ms: u64,
}

/// Applies `[design_tokens]` tables to the running HUD. Public only so
/// [`crate::mcp::McpServerConfig`] can name it; it is built inside the
/// runtime, and other crates pass `None`.
pub struct Appearance {
    pub(crate) live: Arc<LiveTokens>,
    scene: Arc<Mutex<SceneGraph>>,
    /// Where `runtime.fonts_dir` points (`<config dir>/fonts`).
    config_parent: Option<PathBuf>,
    /// The on-disk widget bundles, re-read to re-resolve their SVGs.
    bundle_roots: Vec<PathBuf>,
    /// One apply at a time, so the newest table always wins.
    applying: Mutex<()>,
    reload: std::sync::Mutex<Option<ReloadStatus>>,
    /// The `[design_tokens]` the config file holds, as far as this process
    /// knows: a reload that finds the same table is not an edit, so it never
    /// reverts a change the panel could not save.
    file_table: std::sync::Mutex<DesignTokenMap>,
    /// File reloads attempted, changed or not.
    reloads: AtomicU64,
}

impl std::fmt::Debug for Appearance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Appearance")
            .field("table", &self.live.table())
            .finish_non_exhaustive()
    }
}

impl Appearance {
    pub(crate) fn new(
        live: Arc<LiveTokens>,
        scene: Arc<Mutex<SceneGraph>>,
        config_parent: Option<PathBuf>,
        bundle_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            file_table: std::sync::Mutex::new(live.table()),
            live,
            scene,
            config_parent,
            bundle_roots,
            applying: Mutex::new(()),
            reload: std::sync::Mutex::new(None),
            reloads: AtomicU64::new(0),
        }
    }

    /// The HUD's appearance as it started: `raw`'s `[design_tokens]`,
    /// already resolved into `resolved`, with fonts and widget bundles found
    /// beside `config_file`.
    pub(crate) fn for_startup(
        raw: Option<&tze_hud_config::raw::RawConfig>,
        config_file: Option<&std::path::Path>,
        resolved: DesignTokenMap,
        scene: Arc<Mutex<SceneGraph>>,
        compositor_wake: RenderWakeNotifier,
        main_wake: RenderWakeNotifier,
    ) -> Self {
        let table = raw
            .and_then(|r| r.design_tokens.as_ref())
            .map(|t| t.0.clone())
            .unwrap_or_default();
        let config_parent = config_file.and_then(|p| p.parent()).map(PathBuf::from);
        let bundle_roots = raw
            .map(|r| crate::widget_startup::bundle_roots(r, config_parent.as_deref()))
            .unwrap_or_default();
        Self::new(
            Arc::new(LiveTokens::new(table, resolved, compositor_wake, main_wake)),
            scene,
            config_parent,
            bundle_roots,
        )
    }

    /// Restyle the HUD with `table` as its `[design_tokens]`. Refused (and
    /// nothing changes) when config loading would refuse it. Returns whether
    /// anything changed.
    pub(crate) async fn apply(&self, table: DesignTokenMap) -> Result<bool, String> {
        check_design_tokens(&table)?;
        let _one_at_a_time = self.applying.lock().await;
        if table == self.live.table() {
            return Ok(false);
        }
        let mut resolved = resolve_config_tokens(&table);
        crate::scene_startup::resolve_font_dir(&mut resolved, self.config_parent.as_deref());
        if resolved == *self.live.resolved() {
            // Only the table's spelling changed (e.g. an override equal to
            // the theme's value): nothing to repaint.
            self.live.respell(table);
            return Ok(true);
        }
        // Bundles are files: read them off the async workers.
        let roots = self.bundle_roots.clone();
        let tokens = resolved.clone();
        let svgs = tokio::task::spawn_blocking(move || {
            crate::widget_startup::widget_svg_assets(&roots, &tokens)
        })
        .await
        .map_err(|e| format!("Widget bundles could not be re-read: {e}"))?;
        {
            let mut scene = self.scene.lock().await;
            restyle_zones(&mut scene.zone_registry, &resolved);
            for (widget_type, file, bytes) in svgs {
                scene.enqueue_widget_svg_asset(&widget_type, &file, bytes);
            }
            scene.version += 1;
            tracing::info!(
                theme = tze_hud_config::themes::selected_theme_name(&table),
                overrides = table.len(),
                "appearance: design tokens applied"
            );
            // Under the scene lock: no frame sees new zone styles with old
            // tokens.
            self.live.publish(table, resolved);
        }
        Ok(true)
    }

    /// Apply the `[design_tokens]` of config text `src` (a file reload),
    /// recording the outcome for `/admin/status`. A file that does not parse
    /// or a table that is refused keeps the current tokens.
    pub(crate) async fn reload_from(&self, src: &str) -> Result<bool, String> {
        let outcome = match toml::from_str::<tze_hud_config::raw::RawConfig>(src) {
            Ok(raw) => {
                let table = raw.design_tokens.map(|t| t.0).unwrap_or_default();
                let edited = {
                    let mut file = self.file_table.lock().unwrap_or_else(|e| e.into_inner());
                    (*file != table).then(|| *file = table.clone()).is_some()
                };
                if edited {
                    self.apply(table).await
                } else {
                    Ok(false)
                }
            }
            Err(e) => Err(format!("The config file is not valid TOML: {e}")),
        };
        if let Err(error) = &outcome {
            tracing::warn!(%error, "appearance: config reload refused; keeping current tokens");
        }
        {
            let mut status = self.reload.lock().unwrap_or_else(|e| e.into_inner());
            // Record changes and failures, and clear a failure once the file
            // reads cleanly again (even back to the tokens in effect).
            let was_failing = status.as_ref().is_some_and(|s| !s.ok);
            if !matches!(outcome, Ok(false)) || was_failing {
                *status = Some(ReloadStatus {
                    ok: outcome.is_ok(),
                    error: outcome.as_ref().err().cloned(),
                    at_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis() as u64),
                });
            }
        }
        // Counted once the outcome is published, so a test that sees the
        // count rise also sees its effect.
        self.reloads.fetch_add(1, Ordering::AcqRel);
        outcome
    }

    /// The panel saved `updates` to the config file.
    pub(crate) fn saved(&self, updates: &[(&str, tze_hud_config::TokenUpdate)]) {
        let mut file = self.file_table.lock().unwrap_or_else(|e| e.into_inner());
        for (key, update) in updates {
            match update {
                tze_hud_config::TokenUpdate::Set(value) => {
                    file.insert((*key).to_owned(), value.clone());
                }
                tze_hud_config::TokenUpdate::Remove => {
                    file.remove(*key);
                }
            }
        }
    }

    /// File reloads attempted so far.
    #[cfg(test)]
    pub(crate) fn reloads(&self) -> u64 {
        self.reloads.load(Ordering::Acquire)
    }

    /// The last file reload that changed something or failed; `None` until
    /// one has.
    pub(crate) fn reload_status(&self) -> Option<ReloadStatus> {
        self.reload
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Re-derive every built-in zone's rendering policy from `tokens`, as
/// startup does.
pub(crate) fn restyle_zones(registry: &mut ZoneRegistry, tokens: &DesignTokenMap) {
    let defaults: HashMap<String, RenderingPolicy> = ZoneRegistry::with_defaults()
        .zones
        .into_iter()
        .map(|(name, def)| (name, def.rendering_policy))
        .collect();
    for (zone, policy) in build_all_effective_policies(&defaults, tokens) {
        if let Some(def) = registry.zones.get_mut(&zone) {
            def.rendering_policy = policy;
        }
    }
}

/// Watches the config file and applies `[design_tokens]` edits
/// (hud-h51u7.5).
///
/// The watcher reports the config's directory (editors replace files by
/// rename), filtered to the file, on a thread that blocks between events,
/// so an idle HUD gets no wakes. A burst of events applies once, after
/// [`DEBOUNCE`] of quiet. A table equal to the one in effect (the panel's
/// own saves) changes nothing.
pub(crate) mod watch {
    use super::Appearance;
    use notify::{RecursiveMode, Watcher};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Quiet time after the last event before the file is read.
    pub(crate) const DEBOUNCE: Duration = Duration::from_millis(250);
    /// The longest a steady stream of events can delay the read.
    const MAX_DELAY: Duration = Duration::from_secs(2);
    /// Reads of a file caught mid-replace before waiting for the next event.
    const READ_TRIES: u32 = 3;

    /// Keeps the watch alive; dropping it stops watching.
    pub(crate) struct ConfigWatch {
        _watcher: notify::RecommendedWatcher,
    }

    /// Start watching `config` for `appearance` on `rt`. `None` (logged)
    /// when the OS refuses.
    pub(crate) fn spawn(
        config: PathBuf,
        appearance: Arc<Appearance>,
        rt: tokio::runtime::Handle,
    ) -> Option<ConfigWatch> {
        let dir = config.parent()?.to_path_buf();
        let name = config.file_name()?.to_owned();
        let (tx, rx) = mpsc::channel::<()>();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event
                    && event.paths.iter().any(|p| p.file_name() == Some(&name))
                {
                    let _ = tx.send(());
                }
            })
            .inspect_err(|error| tracing::warn!(%error, "appearance: cannot watch the config"))
            .ok()?;
        if let Err(error) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            tracing::warn!(%error, dir = %dir.display(), "appearance: cannot watch the config");
            return None;
        }
        std::thread::Builder::new()
            .name("hud-config-watch".into())
            .spawn(move || run(&config, &rx, &appearance, &rt))
            .inspect_err(|error| tracing::warn!(%error, "appearance: no watch thread"))
            .ok()?;
        tracing::info!(config = %dir.display(), "appearance: watching the config for [design_tokens] edits");
        Some(ConfigWatch { _watcher: watcher })
    }

    /// Block until the file changes, wait out the burst, apply; until the
    /// watcher is dropped.
    fn run(
        config: &Path,
        events: &mpsc::Receiver<()>,
        appearance: &Appearance,
        rt: &tokio::runtime::Handle,
    ) {
        while events.recv().is_ok() {
            let first = std::time::Instant::now();
            while first.elapsed() < MAX_DELAY && events.recv_timeout(DEBOUNCE).is_ok() {}
            for attempt in 1..=READ_TRIES {
                match std::fs::read_to_string(config) {
                    Ok(src) => {
                        let _ = rt.block_on(appearance.reload_from(&src));
                        break;
                    }
                    // Mid-replace or deleted: try again shortly, then wait
                    // for the next event.
                    Err(error) => {
                        tracing::debug!(%error, attempt, "appearance: config not readable");
                        if attempt < READ_TRIES {
                            std::thread::sleep(DEBOUNCE);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn appearance(scene: SceneGraph) -> Appearance {
        let table = DesignTokenMap::new();
        let resolved = resolve_config_tokens(&table);
        Appearance::new(
            Arc::new(LiveTokens::new(
                table,
                resolved,
                RenderWakeNotifier::default(),
                RenderWakeNotifier::default(),
            )),
            Arc::new(Mutex::new(scene)),
            None,
            Vec::new(),
        )
    }

    fn table(pairs: &[(&str, &str)]) -> DesignTokenMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// A new theme reaches the live map once, restyles zones and queues the
    /// widget SVGs re-resolved; the same table again changes nothing; a
    /// refused table keeps the current one.
    #[tokio::test]
    async fn a_new_table_restyles_once_and_a_refused_one_changes_nothing() {
        let mut scene = SceneGraph::new(800.0, 600.0);
        scene.zone_registry = ZoneRegistry::with_defaults();
        restyle_zones(
            &mut scene.zone_registry,
            &resolve_config_tokens(&DesignTokenMap::new()),
        );
        let appearance = appearance(scene);
        let mut seen = 0;
        assert!(appearance.live.if_changed(&mut seen).is_none());
        let policies = |registry: &ZoneRegistry| {
            let mut all: Vec<(String, String)> = registry
                .zones
                .iter()
                .map(|(name, def)| (name.clone(), format!("{:?}", def.rendering_policy)))
                .collect();
            all.sort();
            all
        };
        let before = policies(&appearance.scene.lock().await.zone_registry);
        let version = appearance.scene.lock().await.version;

        let blueprint = table(&[("theme", "blueprint")]);
        assert_eq!(appearance.apply(blueprint.clone()).await, Ok(true));
        let map = appearance.live.if_changed(&mut seen).expect("changed");
        assert_eq!(*map, resolve_config_tokens(&blueprint));
        assert!(appearance.live.if_changed(&mut seen).is_none(), "once");
        {
            let mut scene = appearance.scene.lock().await;
            assert_ne!(policies(&scene.zone_registry), before, "zones restyled");
            assert!(scene.version > version);
            let svgs = scene.drain_pending_widget_svg_assets();
            assert!(!svgs.is_empty(), "built-in widget SVGs re-resolved");
        }

        assert_eq!(appearance.apply(blueprint.clone()).await, Ok(false));
        assert!(appearance.live.if_changed(&mut seen).is_none());

        let err = appearance
            .apply(table(&[("theme", "nope")]))
            .await
            .unwrap_err();
        assert!(err.contains("unknown theme"), "{err}");
        let err = appearance
            .apply(table(&[("runtime.fonts_dir", "/x")]))
            .await
            .unwrap_err();
        assert!(err.contains("set by the runtime"), "{err}");
        assert_eq!(appearance.live.table(), blueprint, "kept");
    }

    /// A reload reports refusals and real changes in status; an unchanged
    /// table (the panel's own save) leaves status alone.
    #[tokio::test]
    async fn file_reloads_report_their_outcome() {
        let appearance = appearance(SceneGraph::new(800.0, 600.0));
        assert_eq!(appearance.reload_status(), None);
        assert_eq!(
            appearance.reload_from("[runtime]\nfps = 60\n").await,
            Ok(false)
        );
        assert_eq!(appearance.reload_status(), None, "nothing changed");

        let err = appearance
            .reload_from("[design_tokens\n")
            .await
            .unwrap_err();
        assert!(err.contains("not valid TOML"), "{err}");
        let status = appearance.reload_status().unwrap();
        assert!(!status.ok);
        assert_eq!(status.error.as_deref(), Some(err.as_str()));

        let src = "[design_tokens]\ntheme = \"classic\"\n";
        assert_eq!(appearance.reload_from(src).await, Ok(true));
        assert!(appearance.reload_status().unwrap().ok);

        // A failure fixed back to the tokens in effect clears the error,
        // for bad TOML and for a refused table alike.
        for broken in ["[design_tokens\n", "[design_tokens]\ntheme = \"nope\"\n"] {
            assert!(appearance.reload_from(broken).await.is_err(), "{broken}");
            assert!(!appearance.reload_status().unwrap().ok);
            assert_eq!(appearance.reload_from(src).await, Ok(false), "{broken}");
            assert!(appearance.reload_status().unwrap().ok, "{broken}");
        }
        assert_eq!(appearance.live.table(), table(&[("theme", "classic")]));

        // A change the panel could not save survives a reload of the
        // unchanged file (e.g. after an unrelated edit), and a save is not
        // re-applied when the watcher reads it back.
        let session = table(&[("theme", "blueprint")]);
        assert_eq!(appearance.apply(session.clone()).await, Ok(true));
        let unrelated = "[runtime]\nfps = 30\n[design_tokens]\ntheme = \"classic\"\n";
        assert_eq!(appearance.reload_from(unrelated).await, Ok(false));
        assert_eq!(appearance.live.table(), session);
        appearance.saved(&[(
            "theme",
            tze_hud_config::TokenUpdate::Set("blueprint".into()),
        )]);
        let saved = "[design_tokens]\ntheme = \"blueprint\"\n";
        assert_eq!(appearance.reload_from(saved).await, Ok(false));
        assert_eq!(
            appearance.reload_from(src).await,
            Ok(true),
            "a real edit applies"
        );
    }

    /// Editing the watched file applies it after the debounce; writing the
    /// same table again applies nothing.
    #[test]
    fn the_watched_file_applies_edits() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("tze_hud.toml");
        std::fs::write(&config, "# mine\n[runtime]\nfps = 60\n").unwrap();
        let appearance = Arc::new(appearance(SceneGraph::new(800.0, 600.0)));
        let _watch = watch::spawn(config.clone(), Arc::clone(&appearance), rt.handle().clone())
            .expect("watch");
        let mut seen = 0;
        let applied = |seen: &mut u64| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if appearance.live.if_changed(seen).is_some() {
                    return true;
                }
                if std::time::Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        std::fs::write(
            &config,
            "# mine\n[runtime]\nfps = 60\n[design_tokens]\ntheme = \"blueprint\"\n",
        )
        .unwrap();
        assert!(applied(&mut seen), "applied");
        assert_eq!(appearance.live.table(), table(&[("theme", "blueprint")]));

        // A runtime-only edit: the table is unchanged, so nothing applies.
        let reloads = appearance.reloads();
        std::fs::write(
            &config,
            "# mine\n[runtime]\nfps = 30\n[design_tokens]\ntheme = \"blueprint\"\n",
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while appearance.reloads() == reloads {
            assert!(std::time::Instant::now() < deadline, "the edit was read");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(appearance.live.if_changed(&mut seen).is_none());
    }
}
