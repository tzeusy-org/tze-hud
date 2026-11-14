//! Appearance (theme and fonts) and Advanced tokens (every design token):
//! both restyle the HUD on the next frame and save to `[design_tokens]`
//! (CP-5). The panel's own text stays legible whatever an override does:
//! its renderer clamps sizes and falls back on unreadable colors.

use std::sync::Arc;

use tze_hud_config::themes::{
    DEFAULT_THEME, THEME_KEY, builtin_theme, builtin_theme_names, selected_theme_name,
};
use tze_hud_config::tokens::{
    CANONICAL_TOKENS, DesignTokenMap, OS_MOTION_ZEROED_KEYS, UI_SCALE_KEY, UI_SCALE_RANGE,
    os_reduce_motion, ui_scale,
};
use tze_hud_config::{ConfigWriteError, TokenUpdate};

use super::super::row::{BoxFuture, Control, Edit, Row, Section};
use super::super::runtime::TokenWriter;
use crate::appearance::Appearance;

/// The token the Subtitle backdrop toggle sets (0 or absent: no backdrop).
const SUBTITLE_BACKDROP_KEY: &str = "opacity.subtitle.backdrop";

/// The font roles the Appearance card offers: (token, label).
const FONTS: [(&str, &str); 3] = [
    ("font.sans", "Sans font"),
    ("font.mono", "Mono font"),
    ("font.serif", "Serif font"),
];

/// Applies a `[design_tokens]` change live, then saves it.
#[derive(Clone)]
struct Tokens {
    appearance: Arc<Appearance>,
    /// `None`: no config file; changes last until restart.
    save: Option<TokenWriter>,
}

impl Tokens {
    fn table(&self) -> DesignTokenMap {
        self.appearance.live.table()
    }

    fn saved_detail(&self) -> &'static str {
        if self.save.is_some() {
            "Saved to config"
        } else {
            "No config file; lasts until restart"
        }
    }

    /// Apply `updates` to the table in effect now, then save them. A failed
    /// save keeps the change for the session and says why.
    async fn change(&self, updates: &[(&str, TokenUpdate)]) -> Result<(), String> {
        self.change_with(updates, Save::IfChanged).await
    }

    /// Apply `updates` live only (a preview); nothing is saved.
    async fn show(&self, updates: &[(&str, TokenUpdate)]) -> Result<(), String> {
        self.change_with(updates, Save::Never).await
    }

    async fn change_with(&self, updates: &[(&str, TokenUpdate)], save: Save) -> Result<(), String> {
        let mut table = self.table();
        for (key, update) in updates {
            match update {
                TokenUpdate::Set(value) => {
                    table.insert((*key).to_owned(), value.clone());
                }
                TokenUpdate::Remove => {
                    table.remove(*key);
                }
            }
        }
        // Check each update the way the save will, before anything applies.
        for (key, update) in updates {
            tze_hud_config::check_token_update(key, update).map_err(|e| match e {
                ConfigWriteError::InvalidValue { expected, .. } => {
                    format!("{key} takes {expected}")
                }
                ConfigWriteError::UnknownKey(_) => format!("{key} can only be cleared"),
                e => e.to_string(),
            })?;
        }
        let changed = self.appearance.apply(table).await?;
        let save_it = match save {
            Save::Never => false,
            // Already in effect (e.g. Zoom held at its limit): nothing to save.
            Save::IfChanged => changed,
            Save::Always => true,
        };
        if !save_it {
            return Ok(());
        }
        tracing::info!(?updates, "control panel: design tokens changed");
        let Some(save) = &self.save else {
            return Ok(());
        };
        save(updates).map_err(|e| format!("Applied until restart; not saved: {e}"))?;
        self.appearance.saved(updates);
        Ok(())
    }
}

/// Whether [`Tokens::change_with`] saves what it applied.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Save {
    /// A preview: live only.
    Never,
    /// Only if the table changed (the usual edit).
    IfChanged,
    /// Even when already live: keeping a preview saves the shown value.
    Always,
}

/// Whether the table's override of `key` differs from the value it would
/// have unset (the theme's, else the canonical default): only then does the
/// row differ from its default and offer Reset.
fn overrides_default(table: &DesignTokenMap, key: &str) -> bool {
    let Some(set) = table.get(key) else {
        return false;
    };
    let theme = builtin_theme(selected_theme_name(table)).unwrap_or_default();
    let unset = theme.get(key).map(String::as_str).or_else(|| {
        CANONICAL_TOKENS
            .iter()
            .find(|t| t.key == key)
            .map(|t| t.default_value)
    });
    unset != Some(set.as_str())
}

/// The update that puts `key` back to `value` (`None`: unset).
fn restore(value: Option<String>) -> TokenUpdate {
    value.map_or(TokenUpdate::Remove, TokenUpdate::Set)
}

/// "tonal-glass" → "Tonal Glass".
fn theme_title(name: &str) -> String {
    name.split('-')
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Appearance ──────────────────────────────────────────────────────────────

pub(crate) struct AppearanceSection {
    tokens: Tokens,
    /// While a theme previews: the `theme` value in effect before it
    /// (`Some(None)`: unset), which leaving the row restores.
    before_preview: std::sync::Mutex<Option<Option<String>>>,
}

impl AppearanceSection {
    const THEME: &'static str = "appearance.theme";
    const ZOOM: &'static str = "appearance.zoom";
    /// One Ctrl+− / Ctrl+= step of `ui.scale`.
    const ZOOM_STEP: f32 = 0.1;
    const FONT_PREFIX: &'static str = "appearance.";
    const SUBTITLE_BACKDROP: &'static str = "appearance.subtitle_backdrop";
    /// The fill opacity the Subtitle backdrop toggle sets; fine-tune it under
    /// Advanced tokens.
    const SUBTITLE_BACKDROP_ON: &'static str = "0.9";

    pub(crate) fn new(appearance: Arc<Appearance>, save: Option<TokenWriter>) -> Self {
        Self {
            tokens: Tokens { appearance, save },
            before_preview: std::sync::Mutex::new(None),
        }
    }

    /// End a preview; the theme value it replaced, if one was showing.
    fn end_preview(&self) -> Option<Option<String>> {
        self.before_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    async fn apply_theme(&self, edit: Edit) -> Result<(), String> {
        let theme = |i: usize| {
            builtin_theme_names()
                .nth(i)
                .map(|t| TokenUpdate::Set(t.to_owned()))
                .ok_or("No such theme")
        };
        match edit {
            Edit::Preview(step) => {
                // From the theme showing now: steps queue behind a slow
                // restyle and still walk one theme each.
                let names: Vec<&str> = builtin_theme_names().collect();
                let table = self.tokens.table();
                let showing = selected_theme_name(&table);
                let at = names.iter().position(|t| *t == showing).unwrap_or(0) as i64;
                let n = names.len() as i64;
                let update = theme((at + i64::from(step)).rem_euclid(n) as usize)?;
                self.before_preview
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert_with(|| self.tokens.table().get(THEME_KEY).cloned());
                self.tokens.show(&[(THEME_KEY, update)]).await
            }
            Edit::Revert => match self.end_preview() {
                Some(before) => self.tokens.show(&[(THEME_KEY, restore(before))]).await,
                None => Ok(()),
            },
            // Enter keeps the theme the preview shows: save it as it is.
            Edit::Keep => {
                if self.end_preview().is_none() {
                    return Ok(());
                }
                let showing = selected_theme_name(&self.tokens.table()).to_owned();
                self.tokens
                    .change_with(&[(THEME_KEY, TokenUpdate::Set(showing))], Save::Always)
                    .await
            }
            // A click picks a theme outright, ending any preview.
            Edit::Choose(i) => {
                let save = match self.end_preview() {
                    Some(_) => Save::Always,
                    None => Save::IfChanged,
                };
                self.tokens
                    .change_with(&[(THEME_KEY, theme(i)?)], save)
                    .await
            }
            Edit::Reset => {
                self.end_preview();
                self.tokens
                    .change_with(&[(THEME_KEY, TokenUpdate::Remove)], Save::Always)
                    .await
            }
            edit => Err(format!("{} does not support {edit:?}", Self::THEME)),
        }
    }
}

impl Section for AppearanceSection {
    fn id(&self) -> &'static str {
        "appearance"
    }

    fn title(&self) -> &'static str {
        "Appearance"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            let table = self.tokens.table();
            let resolved = self.tokens.appearance.live.resolved();
            let themes: Vec<&str> = builtin_theme_names().collect();
            let current = selected_theme_name(&table);
            // A preview shows on the row; Reset follows the kept theme.
            let saved_theme = match &*self
                .before_preview
                .lock()
                .unwrap_or_else(|e| e.into_inner())
            {
                Some(before) => before.clone(),
                None => table.get(THEME_KEY).cloned(),
            };
            let mut rows = vec![
                Row::new(
                    Self::THEME,
                    "Theme",
                    Control::Choice {
                        options: themes.iter().map(|t| theme_title(t)).collect(),
                        selected: themes.iter().position(|t| *t == current).unwrap_or(0),
                    },
                )
                .detail(format!(
                    "←→ previews · Enter keeps · {}",
                    self.tokens.saved_detail()
                ))
                .keywords(&["theme", "color", "colour", "look", "style", "dark"])
                .previews()
                .resettable(saved_theme.is_some_and(|t| t != DEFAULT_THEME)),
                Row::new(
                    Self::ZOOM,
                    "Zoom",
                    Control::Stepper {
                        text: format!("{:.0}%", ui_scale(&table) * 100.0),
                    },
                )
                .detail(format!(
                    "Applies now · {} · on top of Windows display scaling (widgets: zoom only)",
                    self.tokens.saved_detail()
                ))
                .keywords(&[
                    "zoom", "scale", "size", "bigger", "smaller", "dpi", "ui.scale",
                ])
                .resettable((ui_scale(&table) - 1.0).abs() > f32::EPSILON),
            ];
            let backdrop_on = resolved
                .get(SUBTITLE_BACKDROP_KEY)
                .and_then(|v| v.parse::<f32>().ok())
                .is_some_and(|o| o > 0.0);
            rows.push(
                Row::new(
                    Self::SUBTITLE_BACKDROP,
                    "Subtitle backdrop",
                    Control::Toggle(backdrop_on),
                )
                .detail(format!(
                    "A solid fill behind the subtitle zone, so streamed subtitles stand out · {}",
                    self.tokens.saved_detail()
                ))
                .keywords(&[
                    "subtitle",
                    "caption",
                    "backdrop",
                    "background",
                    "stream",
                ]),
            );
            for (key, label) in FONTS {
                let set = table.get(key);
                let family = set
                    .or_else(|| resolved.get(key))
                    .cloned()
                    .unwrap_or_else(|| "Bundled".to_owned());
                let detail = match set {
                    Some(_) => format!(
                        "Applies now · {} · Reset for the theme's",
                        self.tokens.saved_detail()
                    ),
                    None => {
                        "The theme's · type a font installed on Windows or in fonts/".to_owned()
                    }
                };
                rows.push(
                    Row::new(
                        format!("{}{key}", Self::FONT_PREFIX),
                        label,
                        Control::Field {
                            text: family,
                            confirm: None,
                        },
                    )
                    .detail(detail)
                    .keywords(&["font", "typeface", "family", "text"])
                    .resettable(overrides_default(&table, key)),
                );
            }
            rows
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let described = format!("{edit:?}");
            let unsupported = || format!("{row} does not support {described}");
            if row == Self::ZOOM {
                let update = match edit {
                    Edit::Activate => return Ok(()),
                    Edit::Reset => TokenUpdate::Remove,
                    Edit::Step(n) => {
                        let (lo, hi) = UI_SCALE_RANGE;
                        let next = (ui_scale(&self.tokens.table()) + n as f32 * Self::ZOOM_STEP)
                            .clamp(lo, hi);
                        // Whole percents, so steps never drift (1.1, not 1.1000001).
                        let next = (next * 100.0).round() / 100.0;
                        if (next - 1.0).abs() < f32::EPSILON {
                            TokenUpdate::Remove
                        } else {
                            TokenUpdate::Set(next.to_string())
                        }
                    }
                    _ => return Err(unsupported()),
                };
                return self.tokens.change(&[(UI_SCALE_KEY, update)]).await;
            }
            if row == Self::THEME {
                return self.apply_theme(edit).await;
            }
            if row == Self::SUBTITLE_BACKDROP {
                let update = match edit {
                    Edit::Set(true) => TokenUpdate::Set(Self::SUBTITLE_BACKDROP_ON.to_owned()),
                    Edit::Set(false) | Edit::Reset => TokenUpdate::Remove,
                    _ => return Err(unsupported()),
                };
                return self.tokens.change(&[(SUBTITLE_BACKDROP_KEY, update)]).await;
            }
            let key = row
                .strip_prefix(Self::FONT_PREFIX)
                .filter(|key| FONTS.iter().any(|(k, _)| k == key))
                .ok_or_else(unsupported)?;
            let text = match edit {
                Edit::Text(text) => text,
                Edit::Reset => String::new(),
                Edit::Activate => return Ok(()),
                _ => return Err(unsupported()),
            };
            let family = text.trim();
            let update = if family.is_empty() {
                TokenUpdate::Remove
            } else {
                TokenUpdate::Set(family.to_owned())
            };
            self.tokens.change(&[(key, update)]).await
        })
    }
}

// ─── Advanced tokens ─────────────────────────────────────────────────────────

fn is_canonical(key: &str) -> bool {
    CANONICAL_TOKENS.iter().any(|t| t.key == key)
}

pub(crate) struct TokensSection {
    tokens: Tokens,
}

impl TokensSection {
    const PREFIX: &'static str = "tokens.";
    const RESET_ALL: &'static str = "tokens.reset_all";

    pub(crate) fn new(appearance: Arc<Appearance>, save: Option<TokenWriter>) -> Self {
        Self {
            tokens: Tokens { appearance, save },
        }
    }

    /// The table's overrides: every key but the theme selector.
    fn overrides(table: &DesignTokenMap) -> Vec<String> {
        let mut keys: Vec<String> = table.keys().filter(|k| *k != THEME_KEY).cloned().collect();
        keys.sort();
        keys
    }
}

impl Section for TokensSection {
    fn id(&self) -> &'static str {
        "tokens"
    }

    fn title(&self) -> &'static str {
        "Advanced tokens"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            let table = self.tokens.table();
            let resolved = self.tokens.appearance.live.resolved();
            let theme_name = selected_theme_name(&table);
            let theme = builtin_theme(theme_name).unwrap_or_default();
            let overrides = Self::overrides(&table);
            let os_reduces_motion = os_reduce_motion(&resolved);
            // Always drawn, so a broken override can always be undone.
            let mut rows = vec![
                Row::new(
                    Self::RESET_ALL,
                    "Reset all overrides",
                    Control::Action { confirm: None },
                )
                .detail(match overrides.len() {
                    0 => format!(
                        "None set · the {} theme as shipped",
                        theme_title(theme_name)
                    ),
                    1 => "1 override · back to the theme's values".to_owned(),
                    n => format!("{n} overrides · back to the theme's values"),
                })
                .keywords(&["reset", "tokens", "overrides", "defaults", "undo"]),
            ];
            for token in CANONICAL_TOKENS {
                let key = token.key;
                let detail = if table.contains_key(key) {
                    format!(
                        "Override · {} · Reset for the theme's",
                        self.tokens.saved_detail()
                    )
                } else if os_reduces_motion && OS_MOTION_ZEROED_KEYS.contains(&key) {
                    // The value's real source (hud-ljhku.13): the theme's
                    // duration is zeroed by the OS motion layer.
                    "Windows animations off".to_owned()
                } else if theme.contains_key(key) {
                    format!("From {}", theme_title(theme_name))
                } else {
                    "Default".to_owned()
                };
                rows.push(
                    Row::new(
                        format!("{}{key}", Self::PREFIX),
                        key,
                        Control::Field {
                            text: resolved.get(key).cloned().unwrap_or_default(),
                            confirm: None,
                        },
                    )
                    .detail(detail)
                    .keywords(&["token", "design token", "advanced"])
                    .resettable(overrides_default(&table, key)),
                );
            }
            // Hand-written overrides of keys the panel does not set (e.g.
            // `portal.*` colors): shown so they can be seen and cleared.
            for key in overrides.iter().filter(|k| !is_canonical(k)) {
                rows.push(
                    Row::new(
                        format!("{}{key}", Self::PREFIX),
                        key.clone(),
                        Control::Field {
                            text: table.get(key).cloned().unwrap_or_default(),
                            confirm: None,
                        },
                    )
                    .detail("Override from the config file · Reset removes it")
                    .keywords(&["token", "design token", "advanced"])
                    .resettable(true),
                );
            }
            rows
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let described = format!("{edit:?}");
            let unsupported = || format!("{row} does not support {described}");
            if row == Self::RESET_ALL {
                let Edit::Activate = edit else {
                    return Err(unsupported());
                };
                let table = self.tokens.table();
                let keys = Self::overrides(&table);
                let updates: Vec<(&str, TokenUpdate)> = keys
                    .iter()
                    .map(|k| (k.as_str(), TokenUpdate::Remove))
                    .collect();
                if updates.is_empty() {
                    return Ok(());
                }
                return self.tokens.change(&updates).await;
            }
            let key = row
                .strip_prefix(Self::PREFIX)
                .filter(|key| is_canonical(key) || self.tokens.table().contains_key(*key))
                .ok_or_else(unsupported)?;
            let text = match edit {
                Edit::Text(text) => text,
                Edit::Reset => String::new(),
                Edit::Activate => return Ok(()),
                _ => return Err(unsupported()),
            };
            let value = text.trim();
            let update = if value.is_empty() {
                TokenUpdate::Remove
            } else {
                TokenUpdate::Set(value.to_owned())
            };
            self.tokens.change(&[(key, update)]).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::LiveTokens;
    use std::sync::Mutex;
    use tze_hud_config::themes::resolve_config_tokens;
    use tze_hud_scene::graph::SceneGraph;
    use tze_hud_scene::render_wake::RenderWakeNotifier;

    type Saved = Arc<Mutex<Vec<String>>>;

    fn sections(saves: Option<Result<(), String>>) -> (AppearanceSection, TokensSection, Saved) {
        sections_with(saves, || true)
    }

    fn sections_with(
        saves: Option<Result<(), String>>,
        animations_enabled: fn() -> bool,
    ) -> (AppearanceSection, TokensSection, Saved) {
        let table = DesignTokenMap::new();
        let appearance = Arc::new(Appearance::new(
            Arc::new(LiveTokens::new(
                table.clone(),
                resolve_config_tokens(&table),
                RenderWakeNotifier::default(),
                RenderWakeNotifier::default(),
            )),
            Arc::new(tokio::sync::Mutex::new(SceneGraph::new(800.0, 600.0))),
            None,
            Vec::new(),
            animations_enabled,
        ));
        let saved: Saved = Arc::default();
        let save = saves.map(|outcome| {
            let saved = Arc::clone(&saved);
            Arc::new(move |updates: &[(&str, TokenUpdate)]| {
                saved.lock().unwrap().push(format!("{updates:?}"));
                outcome.clone()
            }) as TokenWriter
        });
        (
            AppearanceSection::new(Arc::clone(&appearance), save.clone()),
            TokensSection::new(appearance, save),
            saved,
        )
    }

    fn row<'a>(rows: &'a [Row], id: &str) -> &'a Row {
        rows.iter().find(|r| r.id == id).unwrap()
    }

    /// hud-ljhku.13 (review N2): with Windows animations off, the zeroed
    /// motion rows name that as their source, not the theme or default; an
    /// explicit override still reads as an override.
    #[tokio::test]
    async fn motion_rows_name_windows_animations_off_as_their_source() {
        let (_, tokens, _) = sections_with(Some(Ok(())), || false);
        let edited: DesignTokenMap = [("theme", "classic"), ("motion.exit.ms", "90")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(tokens.tokens.appearance.apply(edited).await, Ok(true));
        let rows = tokens.rows().await;
        let enter = row(&rows, "tokens.motion.enter.ms");
        assert_eq!(enter.detail.as_deref(), Some("Windows animations off"));
        assert_eq!(
            enter.control,
            Control::Field {
                text: "0".into(),
                confirm: None
            }
        );
        assert!(
            row(&rows, "tokens.motion.exit.ms")
                .detail
                .as_deref()
                .is_some_and(|d| d.starts_with("Override"))
        );

        let (_, tokens, _) = sections(Some(Ok(())));
        let rows = tokens.rows().await;
        assert_ne!(
            row(&rows, "tokens.motion.enter.ms").detail.as_deref(),
            Some("Windows animations off")
        );
    }

    #[test]
    fn theme_names_read_as_titles() {
        assert_eq!(theme_title("tonal-glass"), "Tonal Glass");
        assert_eq!(theme_title("classic"), "Classic");
    }

    /// The Subtitle backdrop toggle sets the backdrop opacity token live and
    /// saves it; off removes the override.
    #[tokio::test]
    async fn subtitle_backdrop_toggle_sets_its_token_and_saves() {
        let (appearance, _, saved) = sections(Some(Ok(())));
        let toggle = |rows: &[Row]| row(rows, "appearance.subtitle_backdrop").control.clone();
        assert_eq!(toggle(&appearance.rows().await), Control::Toggle(false));
        appearance
            .apply("appearance.subtitle_backdrop", Edit::Set(true))
            .await
            .unwrap();
        assert_eq!(toggle(&appearance.rows().await), Control::Toggle(true));
        assert_eq!(
            appearance
                .tokens
                .table()
                .get(SUBTITLE_BACKDROP_KEY)
                .map(String::as_str),
            Some("0.9")
        );
        appearance
            .apply("appearance.subtitle_backdrop", Edit::Set(false))
            .await
            .unwrap();
        assert_eq!(toggle(&appearance.rows().await), Control::Toggle(false));
        assert!(
            !appearance
                .tokens
                .table()
                .contains_key(SUBTITLE_BACKDROP_KEY)
        );
        assert_eq!(saved.lock().unwrap().len(), 2, "each change is saved");
    }

    /// Zoom steps `ui.scale` by 10% within its range, restyles at once,
    /// saves, and Reset returns to 100%.
    #[tokio::test]
    async fn zoom_steps_ui_scale_live_and_saves() {
        let (appearance, _, saved) = sections(Some(Ok(())));
        let zoom = |rows: &[Row]| row(rows, "appearance.zoom").control.clone();
        assert_eq!(
            zoom(&appearance.rows().await),
            Control::Stepper {
                text: "100%".into()
            }
        );
        for _ in 0..2 {
            appearance
                .apply("appearance.zoom", Edit::Step(1))
                .await
                .unwrap();
        }
        assert_eq!(
            zoom(&appearance.rows().await),
            Control::Stepper {
                text: "120%".into()
            }
        );
        let px = tze_hud_config::tokens::px_scale(&appearance.tokens.appearance.live.resolved());
        assert!((px - 1.2).abs() < 1e-6, "every size follows: {px}");
        for _ in 0..40 {
            appearance
                .apply("appearance.zoom", Edit::Step(1))
                .await
                .unwrap();
        }
        assert_eq!(
            zoom(&appearance.rows().await),
            Control::Stepper {
                text: "300%".into()
            },
            "clamped to the range"
        );
        let zoom_row = |rows: &[Row]| {
            rows.iter()
                .find(|r| r.id == "appearance.zoom")
                .unwrap()
                .reset
        };
        assert!(
            zoom_row(&appearance.rows().await),
            "a changed zoom offers Reset"
        );
        appearance
            .apply("appearance.zoom", Edit::Activate)
            .await
            .unwrap();
        assert!(
            appearance.tokens.table().contains_key("ui.scale"),
            "Enter changes nothing"
        );
        appearance
            .apply("appearance.zoom", Edit::Reset)
            .await
            .unwrap();
        assert!(!appearance.tokens.table().contains_key("ui.scale"));
        assert!(!zoom_row(&appearance.rows().await));
        let saved = saved.lock().unwrap();
        assert_eq!(saved[0], r#"[("ui.scale", Set("1.1"))]"#);
        assert_eq!(saved[1], r#"[("ui.scale", Set("1.2"))]"#);
        assert_eq!(saved.last().unwrap(), r#"[("ui.scale", Remove)]"#);
        // 110% to 300% is 20 saves, then Reset's: none once clamped.
        assert_eq!(saved.len(), 21, "{saved:?}");
    }

    /// A previewed theme restyles live without saving; leaving restores the
    /// saved one, keeping saves it (already showing), and Reset goes back to
    /// the default theme.
    #[tokio::test]
    async fn a_theme_preview_restyles_without_saving_until_kept() {
        let (appearance, _, saved) = sections(Some(Ok(())));
        let themes: Vec<&str> = builtin_theme_names().collect();
        let d = themes.iter().position(|t| *t == DEFAULT_THEME).unwrap();
        let next = |k: usize| themes[(d + k) % themes.len()];
        assert!(themes.len() >= 3, "two steps land on a third theme");
        let theme_row = |rows: Vec<Row>| {
            rows.into_iter()
                .find(|r| r.id == "appearance.theme")
                .unwrap()
        };
        let live = |a: &AppearanceSection| a.tokens.table().get(THEME_KEY).cloned();

        appearance
            .apply("appearance.theme", Edit::Preview(1))
            .await
            .unwrap();
        assert_eq!(live(&appearance).as_deref(), Some(next(1)));
        let row = theme_row(appearance.rows().await);
        assert!(row.previews);
        assert!(!row.reset, "Reset follows the saved theme, not the preview");
        assert!(saved.lock().unwrap().is_empty(), "a preview saves nothing");

        appearance
            .apply("appearance.theme", Edit::Revert)
            .await
            .unwrap();
        assert_eq!(live(&appearance), None, "back to the unset theme");

        // Steps walk from the theme showing; Keep saves what shows.
        for _ in 0..2 {
            appearance
                .apply("appearance.theme", Edit::Preview(1))
                .await
                .unwrap();
        }
        assert_eq!(live(&appearance).as_deref(), Some(next(2)));
        appearance
            .apply("appearance.theme", Edit::Keep)
            .await
            .unwrap();
        assert_eq!(
            saved.lock().unwrap().as_slice(),
            [format!(r#"[("theme", Set("{}"))]"#, next(2))],
            "keeping the shown preview saves it"
        );
        assert!(theme_row(appearance.rows().await).reset);
        // Revert after keeping changes nothing.
        appearance
            .apply("appearance.theme", Edit::Revert)
            .await
            .unwrap();
        assert_eq!(live(&appearance).as_deref(), Some(next(2)));
        // Keep without a preview saves nothing more.
        appearance
            .apply("appearance.theme", Edit::Keep)
            .await
            .unwrap();
        assert_eq!(saved.lock().unwrap().len(), 1);

        appearance
            .apply("appearance.theme", Edit::Reset)
            .await
            .unwrap();
        assert_eq!(live(&appearance), None);
        assert_eq!(
            saved.lock().unwrap().last().unwrap(),
            r#"[("theme", Remove)]"#
        );
        assert!(!theme_row(appearance.rows().await).reset);
    }

    /// The theme and a font apply live and save; clearing a font gives the
    /// theme's back; a failed save keeps the change and says so.
    #[tokio::test]
    async fn theme_and_font_apply_live_and_save() {
        let (appearance, _, saved) = sections(Some(Ok(())));
        let themes: Vec<&str> = builtin_theme_names().collect();
        let blueprint = themes.iter().position(|t| *t == "blueprint").unwrap();
        appearance
            .apply("appearance.theme", Edit::Choose(blueprint))
            .await
            .unwrap();
        let rows = appearance.rows().await;
        assert_eq!(
            row(&rows, "appearance.theme").control,
            Control::Choice {
                options: themes.iter().map(|t| theme_title(t)).collect(),
                selected: blueprint,
            }
        );
        appearance
            .apply("appearance.font.mono", Edit::Text(" Consolas ".into()))
            .await
            .unwrap();
        let live = appearance.tokens.appearance.live.resolved();
        assert_eq!(live.get("font.mono").map(String::as_str), Some("Consolas"));
        appearance
            .apply("appearance.font.mono", Edit::Text(String::new()))
            .await
            .unwrap();
        assert!(!appearance.tokens.table().contains_key("font.mono"));
        assert_eq!(
            *saved.lock().unwrap(),
            [
                r#"[("theme", Set("blueprint"))]"#,
                r#"[("font.mono", Set("Consolas"))]"#,
                r#"[("font.mono", Remove)]"#,
            ]
        );

        let (appearance, _, _) = sections(Some(Err("config file C:\\x.toml is read-only".into())));
        let err = appearance
            .apply("appearance.font.sans", Edit::Text("Segoe UI".into()))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "Applied until restart; not saved: config file C:\\x.toml is read-only"
        );
        assert_eq!(
            appearance
                .tokens
                .table()
                .get("font.sans")
                .map(String::as_str),
            Some("Segoe UI"),
            "kept for the session"
        );
    }

    /// A token takes a value of its kind; an override shows as one and Reset
    /// all removes every override but the theme.
    #[tokio::test]
    async fn tokens_override_reset_and_refuse_wrong_kinds() {
        let (appearance, tokens, saved) = sections(Some(Ok(())));
        // Alt+T: the next theme.
        appearance
            .apply("appearance.theme", Edit::Choose(1))
            .await
            .unwrap();
        let err = tokens
            .apply("tokens.color.text.primary", Edit::Text("blue".into()))
            .await
            .unwrap_err();
        assert!(err.contains("color"), "{err}");
        tokens
            .apply("tokens.color.text.primary", Edit::Text("#ff0000".into()))
            .await
            .unwrap();
        let rows = tokens.rows().await;
        let primary = row(&rows, "tokens.color.text.primary");
        assert_eq!(
            primary.control,
            Control::Field {
                text: "#ff0000".into(),
                confirm: None,
            }
        );
        assert!(primary.detail.as_deref().unwrap().starts_with("Override"));
        assert!(primary.reset, "an override offers Reset");
        assert!(!row(&rows, "tokens.color.text.secondary").reset);
        assert!(
            row(&rows, "tokens.reset_all")
                .detail
                .as_deref()
                .unwrap()
                .starts_with("1 override")
        );

        // One row's Reset clears just that override.
        tokens
            .apply("tokens.color.text.primary", Edit::Reset)
            .await
            .unwrap();
        assert!(!tokens.tokens.table().contains_key("color.text.primary"));
        tokens
            .apply("tokens.color.text.primary", Edit::Text("#ff0000".into()))
            .await
            .unwrap();
        tokens
            .apply("tokens.reset_all", Edit::Activate)
            .await
            .unwrap();
        let table = tokens.tokens.table();
        assert!(!table.contains_key("color.text.primary"));
        assert!(table.contains_key(THEME_KEY), "the theme stays");
        assert_eq!(
            saved.lock().unwrap().last().unwrap(),
            r#"[("color.text.primary", Remove)]"#
        );
        assert!(
            tokens
                .apply("tokens.nope", Edit::Text("1".into()))
                .await
                .is_err()
        );
    }

    /// An override equal to the value it would have unset is not a change:
    /// no Reset. A different one is.
    #[test]
    fn only_an_override_off_its_default_offers_reset() {
        let key = "color.text.primary";
        let mut table = DesignTokenMap::new();
        assert!(!overrides_default(&table, key));
        let theme = builtin_theme(selected_theme_name(&table)).unwrap_or_default();
        let unset = theme.get(key).cloned().unwrap_or_else(|| {
            CANONICAL_TOKENS
                .iter()
                .find(|t| t.key == key)
                .unwrap()
                .default_value
                .to_owned()
        });
        table.insert(key.to_owned(), unset);
        assert!(!overrides_default(&table, key));
        table.insert(key.to_owned(), "#123456".to_owned());
        assert!(overrides_default(&table, key));
    }
}
