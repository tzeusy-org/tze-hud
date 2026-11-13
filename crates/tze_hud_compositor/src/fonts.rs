//! Font assets and family resolution for tze_hud.
//!
//! # Why no system-font scan?
//!
//! [`glyphon::FontSystem::new`] calls `db.load_system_fonts()`, which scans
//! every OS font directory at startup.  That is:
//!
//! - **Fragile on kiosk/minimal hosts** — headless servers, Windows Nano,
//!   container images, and similar environments may have zero or very few fonts
//!   installed.
//! - **Slow** — system font discovery takes up to 1 s on debug builds.
//! - **Non-deterministic** — the fonts available (and therefore glyph metrics,
//!   layout widths, and test pass/fail behaviour) vary by host.
//!
//! Instead, every [`FontSystem`] is built with
//! [`glyphon::FontSystem::new_with_locale_and_db`] over a fontdb
//! [`Database`][glyphon::fontdb::Database] that starts with a fixed set of
//! OFL/permissive-licensed faces embedded via `include_bytes!`.  With the
//! default [`FontConfig`] nothing else is loaded, so CI, headless runs and
//! tests are fully deterministic.  ([`glyphon::FontSystem::new_with_fonts`]
//! loads system fonts *in addition to* its sources, so it is never used.)
//!
//! # Font roles
//!
//! [`tze_hud_scene::types::FontFamily`] is a *role*, not a family name: the
//! renderer maps `SystemSansSerif` / `SystemMonospace` / `SystemSerif` to
//! glyphon's generic `SansSerif` / `Monospace` / `Serif`, and each generic is
//! bound to a concrete family when the font system is built.  Changing a role's
//! family therefore restyles every text item using that role, with no scene or
//! protocol change.
//!
//! | Role | Default family | Bundled faces |
//! |------|----------------|---------------|
//! | sans | **IBM Plex Sans** | Regular 400, Italic 400, Medium 500, SemiBold 600, Bold 700 |
//! | mono | **IBM Plex Mono** | Regular 400, Medium 500 |
//! | serif | **DejaVu Serif** | Regular 400, Bold 700 |
//! | (coverage fallback) | **DejaVu Sans**, **DejaVu Sans Mono** | Regular 400 |
//!
//! IBM Plex covers Latin, Greek and Cyrillic (~900 code points) but not the
//! symbols the HUD and agents actually emit (box drawing in sans, `⚠ ✕ ● ◆ ▲
//! ⋯ ⇒ ☾`, Arabic, …).  DejaVu Sans / Sans Mono stay bundled purely as
//! cosmic-text's per-glyph fallback for those code points; they are never a
//! role default.
//!
//! Widget SVG text (resvg, [`crate::widget`]) uses a database of the same
//! bundled faces plus every font file the configured roles loaded (fonts dir,
//! Windows system families), with the same role families, so widgets follow
//! the theme's fonts. A glyph the role family lacks falls back per glyph to
//! another loaded face (normally DejaVu Sans), as cosmic-text does.
//!
//! # Configuring a family
//!
//! A [`FontConfig`] names a family per role (free-form strings, normally the
//! `font.sans` / `font.mono` / `font.serif` design tokens; see
//! [`FontConfig::from_token_map`]) plus the fonts directory, which the runtime
//! sets to `<config dir>/fonts` when it exists (not user-configurable).  [`build_font_system`] resolves each name,
//! first match wins:
//!
//! 1. a bundled family (`"IBM Plex Sans"`, `"IBM Plex Mono"`, `"DejaVu Sans"`,
//!    `"DejaVu Sans Mono"`, `"DejaVu Serif"`);
//! 2. a family in a font file in [`FontConfig::fonts_dir`] (`*.ttf`, `*.otf`,
//!    `*.ttc`, `*.otc`; not recursive).  Every file there is loaded at build
//!    time — the directory is operator-curated, so it is small;
//! 3. on Windows only, an installed system font looked up **by name**: the
//!    `HKLM`/`HKCU` `...\Windows NT\CurrentVersion\Fonts` registry values are
//!    matched against the name and only the matching files are loaded (see
//!    [`system`]).  No directory is scanned.  On other platforms this step is a
//!    no-op.
//!
//! Names match case-insensitively against every family name a face declares
//! (typographic and legacy).  The generic keywords `sans-serif`, `system-ui`,
//! `monospace` and `serif` mean "the role default".  A name that resolves
//! nowhere logs one warning (per name, per process) naming the request and the
//! family used instead, and the role keeps its default.
//!
//! Agent-uploaded fonts are still supported via
//! [`crate::text::TextRasterizer::load_font_bytes`].
//!
//! # System fallback for emoji, CJK and other scripts (hud-ljhku.13)
//!
//! cosmic-text 0.12.1 already falls back per cluster on Windows through a
//! fixed table (`src/font/fallback/windows.rs`: Segoe UI Emoji / Symbol,
//! Nirmala UI for Indic scripts, Malgun Gothic for Hangul, Yu Gothic for
//! kana, and a locale-chosen face for Han), but it only finds faces that are
//! in the font database. [`SystemFallback`] loads them lazily: no fallback
//! face loads at startup; the first shaped glyph no loaded face covers
//! (glyph id 0) names that table's families for the glyph's script plus the
//! emoji/symbol faces ([`fallback_families`]), and a background thread loads
//! them by name through [`system`] (never on the frame thread). Text shows
//! tofu until the faces land; then they are added with
//! `FontSystem::db_mut()` (which clears cosmic-text's font-match cache) and
//! the shaped text is invalidated so it re-lays out once, without the agent
//! republishing. Each family is requested at most once per process; an
//! absent family is skipped with one log line. The locale passed to the
//! font system comes from `LANG`, else (Windows) the user default locale,
//! normalized to cosmic-text's Han keys ([`normalize_locale`]), so kanji take
//! Japanese forms on a Japanese system. No font is bundled for this (the
//! hud-1sycn evaluation keeps fonts system-sourced); widget SVG text keeps
//! the bundled faces.
//!
//! # Licenses
//!
//! - IBM Plex Sans / Mono: SIL Open Font License 1.1, `fonts/ibm-plex/LICENSE.txt`.
//!   Source: <https://github.com/IBM/plex> releases `@ibm/plex-sans@1.1.0` and
//!   `@ibm/plex-mono@2.5.0` (`fonts/complete/ttf/`, unmodified).
//! - DejaVu: Bitstream Vera derived permissive license, `fonts/dejavu/LICENSE`.
//!   Source: <https://dejavu-fonts.github.io/>

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use glyphon::{FontSystem, fontdb};
use tze_hud_scene::render_wake::RenderWakeNotifier;
use unicode_script::{Script, UnicodeScript};

// ── Embedded font bytes ───────────────────────────────────────────────────────

static PLEX_SANS_REGULAR: &[u8] = include_bytes!("../fonts/ibm-plex/IBMPlexSans-Regular.ttf");
static PLEX_SANS_ITALIC: &[u8] = include_bytes!("../fonts/ibm-plex/IBMPlexSans-Italic.ttf");
static PLEX_SANS_MEDIUM: &[u8] = include_bytes!("../fonts/ibm-plex/IBMPlexSans-Medium.ttf");
static PLEX_SANS_SEMIBOLD: &[u8] = include_bytes!("../fonts/ibm-plex/IBMPlexSans-SemiBold.ttf");
static PLEX_SANS_BOLD: &[u8] = include_bytes!("../fonts/ibm-plex/IBMPlexSans-Bold.ttf");
static PLEX_MONO_REGULAR: &[u8] = include_bytes!("../fonts/ibm-plex/IBMPlexMono-Regular.ttf");
static PLEX_MONO_MEDIUM: &[u8] = include_bytes!("../fonts/ibm-plex/IBMPlexMono-Medium.ttf");
/// Glyph-coverage fallback (symbols, box drawing, Arabic, …) for the sans role.
static DEJAVU_SANS: &[u8] = include_bytes!("../fonts/dejavu/DejaVuSans.ttf");
/// Glyph-coverage fallback for the mono role (keeps fallback glyphs fixed-width).
static DEJAVU_SANS_MONO: &[u8] = include_bytes!("../fonts/dejavu/DejaVuSansMono.ttf");
static DEJAVU_SERIF: &[u8] = include_bytes!("../fonts/dejavu/DejaVuSerif.ttf");
static DEJAVU_SERIF_BOLD: &[u8] = include_bytes!("../fonts/dejavu/DejaVuSerif-Bold.ttf");

/// Number of font faces bundled at compile time.
///
/// Used in startup telemetry and tests to confirm the bundled font set is
/// intact.
pub(crate) const BUNDLED_FONT_FACE_COUNT: usize = 11;

static BUNDLED_FACES: [&[u8]; BUNDLED_FONT_FACE_COUNT] = [
    PLEX_SANS_REGULAR,
    PLEX_SANS_ITALIC,
    PLEX_SANS_MEDIUM,
    PLEX_SANS_SEMIBOLD,
    PLEX_SANS_BOLD,
    PLEX_MONO_REGULAR,
    PLEX_MONO_MEDIUM,
    DEJAVU_SANS,
    DEJAVU_SANS_MONO,
    DEJAVU_SERIF,
    DEJAVU_SERIF_BOLD,
];

/// Default family for the sans role ([`tze_hud_scene::types::FontFamily::SystemSansSerif`]).
pub(crate) const DEFAULT_SANS_FAMILY: &str = "IBM Plex Sans";
/// Default family for the mono role ([`tze_hud_scene::types::FontFamily::SystemMonospace`]).
pub(crate) const DEFAULT_MONO_FAMILY: &str = "IBM Plex Mono";
/// Default family for the serif role ([`tze_hud_scene::types::FontFamily::SystemSerif`]).
pub(crate) const DEFAULT_SERIF_FAMILY: &str = "DejaVu Serif";

/// Design-token key naming the sans role's family.
pub(crate) const TOKEN_FONT_SANS: &str = "font.sans";
/// Design-token key naming the mono role's family.
pub(crate) const TOKEN_FONT_MONO: &str = "font.mono";
/// Design-token key naming the serif role's family.
pub(crate) const TOKEN_FONT_SERIF: &str = "font.serif";
/// Runtime-internal token-map key carrying the absolute fonts directory.
///
/// Not user config: the runtime sets it to `<config dir>/fonts` when that
/// directory exists, and config validation rejects it in `[design_tokens]`
/// (a filesystem path is not a design value).  It rides the token map only
/// because that is the existing channel into the compositor.
pub const RUNTIME_FONTS_DIR_KEY: &str = "runtime.fonts_dir";

// ── Configuration ─────────────────────────────────────────────────────────────

/// Which family each font role uses, plus where operator font files live.
///
/// `None` (or a generic keyword such as `"sans-serif"`) means the role default.
/// Build a font system from it with [`build_font_system`], or hand it to
/// [`crate::Compositor::set_font_config`] to apply it to a running compositor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FontConfig {
    /// Family for the sans role (default [`DEFAULT_SANS_FAMILY`]).
    pub(crate) sans: Option<String>,
    /// Family for the mono role (default [`DEFAULT_MONO_FAMILY`]).
    pub(crate) mono: Option<String>,
    /// Family for the serif role (default [`DEFAULT_SERIF_FAMILY`]).
    pub(crate) serif: Option<String>,
    /// Directory whose font files are loaded before names are resolved.
    pub(crate) fonts_dir: Option<PathBuf>,
}

impl FontConfig {
    /// Read the `font.sans`, `font.mono` and `font.serif` tokens and the
    /// runtime-set [`RUNTIME_FONTS_DIR_KEY`] from a resolved token map.  Absent keys leave the role at its default.
    pub(crate) fn from_token_map(tokens: &HashMap<String, String>) -> Self {
        let get = |key: &str| tokens.get(key).cloned();
        Self {
            sans: get(TOKEN_FONT_SANS),
            mono: get(TOKEN_FONT_MONO),
            serif: get(TOKEN_FONT_SERIF),
            fonts_dir: get(RUNTIME_FONTS_DIR_KEY).map(PathBuf::from),
        }
        .normalized()
    }

    /// Canonical form: names trimmed, and empty names, generic keywords and
    /// names equal to the role default all become `None`, so two configs that
    /// select the same fonts compare equal (no pointless rebuilds).
    pub(crate) fn normalized(self) -> Self {
        let norm = |name: Option<String>, default: &str, generics: &[&str]| {
            let name = name?.trim().to_owned();
            let is_default = name.is_empty()
                || name.eq_ignore_ascii_case(default)
                || generics.iter().any(|g| name.eq_ignore_ascii_case(g));
            (!is_default).then_some(name)
        };
        Self {
            sans: norm(self.sans, DEFAULT_SANS_FAMILY, &["sans-serif", "system-ui"]),
            mono: norm(self.mono, DEFAULT_MONO_FAMILY, &["monospace"]),
            serif: norm(self.serif, DEFAULT_SERIF_FAMILY, &["serif"]),
            fonts_dir: self.fonts_dir.filter(|d| !d.as_os_str().is_empty()),
        }
    }
}

/// The concrete family each role resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedFonts {
    pub(crate) sans: String,
    pub(crate) mono: String,
    pub(crate) serif: String,
}

impl Default for ResolvedFonts {
    fn default() -> Self {
        Self {
            sans: DEFAULT_SANS_FAMILY.to_owned(),
            mono: DEFAULT_MONO_FAMILY.to_owned(),
            serif: DEFAULT_SERIF_FAMILY.to_owned(),
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Build a deterministic [`FontSystem`] with only the bundled faces and the
/// role defaults (IBM Plex Sans / IBM Plex Mono / DejaVu Serif).
pub fn bundled_font_system() -> FontSystem {
    build_font_system(&FontConfig::default()).0
}

/// Build a [`FontSystem`] for `config`: bundled faces, then any font files in
/// `config.fonts_dir`, then (Windows only) system faces for configured names
/// not found in either.  Returns the family each role resolved to.
///
/// All faces are loaded before the [`FontSystem`] is constructed because
/// cosmic-text computes its monospace fallback table once, at construction.
pub(crate) fn build_font_system(config: &FontConfig) -> (FontSystem, ResolvedFonts) {
    build_font_system_with_locale(config, locale())
}

/// [`build_font_system`] with an explicit (normalized) locale.
fn build_font_system_with_locale(
    config: &FontConfig,
    locale: String,
) -> (FontSystem, ResolvedFonts) {
    let mut db = fontdb::Database::new();
    for bytes in BUNDLED_FACES.iter().copied() {
        db.load_font_source(fontdb::Source::Binary(Arc::new(bytes)));
    }
    if let Some(dir) = &config.fonts_dir {
        load_fonts_dir(&mut db, dir);
    }

    let resolved = ResolvedFonts {
        sans: resolve_role(&mut db, config.sans.as_deref(), "sans", DEFAULT_SANS_FAMILY),
        mono: resolve_role(&mut db, config.mono.as_deref(), "mono", DEFAULT_MONO_FAMILY),
        serif: resolve_role(
            &mut db,
            config.serif.as_deref(),
            "serif",
            DEFAULT_SERIF_FAMILY,
        ),
    };
    db.set_sans_serif_family(resolved.sans.clone());
    db.set_monospace_family(resolved.mono.clone());
    db.set_serif_family(resolved.serif.clone());

    (new_font_system(db, locale), resolved)
}

/// cosmic-text caches monospace faces with a rayon `par_iter` at construction.
/// Run it in a scoped one-thread pool: otherwise rayon spawns its global pool,
/// one thread per logical CPU, which then idles for the life of the process.
/// The scoped pool's thread exits when the pool drops.
fn new_font_system(db: fontdb::Database, locale: String) -> FontSystem {
    match rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .thread_name(|_| "tze-font-init".to_owned())
        .build()
    {
        Ok(pool) => pool.install(|| FontSystem::new_with_locale_and_db(locale, db)),
        Err(e) => {
            tracing::warn!(error = %e, "font init pool unavailable; using rayon's global pool");
            FontSystem::new_with_locale_and_db(locale, db)
        }
    }
}

/// Raw bytes of every bundled face, for font databases of a different fontdb
/// version (resvg's widget text).
pub(crate) fn bundled_face_bytes() -> impl Iterator<Item = &'static [u8]> {
    BUNDLED_FACES.iter().copied()
}

/// Font files outside the bundle that `db` loaded (fonts-dir and system
/// faces), sorted and deduplicated, for mirroring into resvg's database.
pub(crate) fn loaded_font_files(db: &fontdb::Database) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = db
        .faces()
        .filter_map(|face| match &face.source {
            fontdb::Source::File(path) => Some(path.clone()),
            #[allow(unreachable_patterns)] // only with fontdb's memmap feature
            fontdb::Source::SharedFile(path, _) => Some(path.clone()),
            _ => None,
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

/// The family name `db` knows that equals `name` case-insensitively, if any.
pub(crate) fn find_family(db: &fontdb::Database, name: &str) -> Option<String> {
    db.faces()
        .flat_map(|face| face.families.iter())
        .find(|(family, _)| family.eq_ignore_ascii_case(name))
        .map(|(family, _)| family.clone())
}

// ── Internals ─────────────────────────────────────────────────────────────────

/// The locale handed to the font system: `LANG` (e.g. `en_US.UTF-8` →
/// `en-US`), else on Windows the user default locale
/// (`GetUserDefaultLocaleName`), else `en-US`; then [`normalize_locale`].
fn locale() -> String {
    locale_from(std::env::var("LANG").ok(), user_default_locale)
}

/// [`locale`] over explicit sources, for tests.
fn locale_from(lang: Option<String>, user_default: impl FnOnce() -> Option<String>) -> String {
    let tag = lang
        .and_then(|v| {
            let v = v.replace('_', "-");
            let v = v.split('.').next().unwrap_or("").to_owned();
            if v.is_empty() { None } else { Some(v) }
        })
        .or_else(user_default)
        .unwrap_or_else(|| String::from("en-US"));
    normalize_locale(&tag)
}

/// Map a BCP 47 tag to the key cosmic-text's Windows Han table matches
/// exactly: Japanese → `ja`, Korean → `ko`, Chinese in Hong Kong or Macau →
/// `zh-HK`, in Taiwan (or Traditional script without a region) → `zh-TW`.
/// Anything else is returned as given (Han then uses Microsoft YaHei UI).
/// A raw `ja-JP` would otherwise fall through to YaHei's Chinese forms.
pub(crate) fn normalize_locale(tag: &str) -> String {
    let mut parts = tag.split(['-', '_']);
    let language = parts.next().unwrap_or("").to_ascii_lowercase();
    let mut region = None;
    let mut script = None;
    for part in parts {
        if part.len() == 2 && part.chars().all(|c| c.is_ascii_alphabetic()) {
            region = Some(part.to_ascii_uppercase());
        } else if part.len() == 4 && part.chars().all(|c| c.is_ascii_alphabetic()) {
            script = Some(part.to_ascii_lowercase());
        }
    }
    match language.as_str() {
        "ja" => "ja".to_owned(),
        "ko" => "ko".to_owned(),
        "zh" => match (region.as_deref(), script.as_deref()) {
            (Some("HK" | "MO"), _) => "zh-HK".to_owned(),
            (Some("TW"), _) | (None, Some("hant")) => "zh-TW".to_owned(),
            _ => tag.to_owned(),
        },
        _ => tag.to_owned(),
    }
}

/// The Windows user default locale name, e.g. `ja-JP`.
#[cfg(windows)]
fn user_default_locale() -> Option<String> {
    use windows::Win32::Globalization::GetUserDefaultLocaleName;
    // LOCALE_NAME_MAX_LENGTH is 85 UTF-16 units including the terminator.
    let mut buf = [0u16; 85];
    // SAFETY: the slice is live and its length is passed by the binding.
    let len = unsafe { GetUserDefaultLocaleName(&mut buf) };
    (len > 1).then(|| String::from_utf16_lossy(&buf[..(len - 1) as usize]))
}

#[cfg(not(windows))]
fn user_default_locale() -> Option<String> {
    None
}

fn is_font_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["ttf", "otf", "ttc", "otc"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    })
}

/// Load every font file directly inside `dir` (missing dir is not an error:
/// the runtime only names one that exists, but a stale token may not).
fn load_fonts_dir(db: &mut fontdb::Database, dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn_once(
                &format!("dir:{}", dir.display()),
                || tracing::warn!(dir = %dir.display(), error = %e, "fonts dir unreadable; skipped"),
            );
            return;
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && is_font_file(p))
        .collect();
    paths.sort(); // deterministic face order
    let before = db.len();
    for path in &paths {
        if let Err(e) = db.load_font_file(path) {
            tracing::warn!(file = %path.display(), error = %e, "font file unreadable; skipped");
        }
    }
    tracing::info!(
        dir = %dir.display(),
        files = paths.len(),
        faces = db.len() - before,
        "loaded fonts dir"
    );
}

fn resolve_role(
    db: &mut fontdb::Database,
    requested: Option<&str>,
    role: &'static str,
    default: &str,
) -> String {
    let Some(requested) = requested else {
        return default.to_owned();
    };
    if let Some(found) = find_family(db, requested) {
        return found;
    }
    if system::load_family(db, requested) > 0 {
        if let Some(found) = find_family(db, requested) {
            tracing::info!(role, family = %found, "loaded system font family");
            return found;
        }
    }
    warn_once(&format!("{role}:{requested}"), || {
        tracing::warn!(
            role,
            requested,
            using = default,
            "font family not found (bundled, fonts dir, system); using role default"
        )
    });
    default.to_owned()
}

/// Run `emit` the first time `key` is seen in this process, so rebuilding a
/// font system (re-init, theme switch) does not repeat the same warning.
fn warn_once(key: &str, emit: impl FnOnce()) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let first = SEEN
        .get_or_init(Default::default)
        .lock()
        .map(|mut seen| seen.insert(key.to_owned()))
        .unwrap_or(true);
    if first {
        emit();
    }
}

// ── Lazy system fallback (hud-ljhku.13) ───────────────────────────────────────

/// Families loaded on the first glyph miss whatever its script: cosmic-text's
/// common Windows emoji and symbol faces.
pub(crate) const COMMON_FALLBACK_FAMILIES: [&str; 2] = ["Segoe UI Emoji", "Segoe UI Symbol"];

/// The families cosmic-text 0.12.1's Windows table
/// (`src/font/fallback/windows.rs`, `script_fallback`/`han_unification`)
/// names for `script` under the normalized `locale`. Kept in step with that
/// table: cosmic-text only finds these faces once they are loaded.
pub(crate) fn script_fallback_families(script: Script, locale: &str) -> &'static [&'static str] {
    fn han(locale: &str) -> &'static [&'static str] {
        match locale {
            "ja" => &["Yu Gothic"],
            "ko" => &["Malgun Gothic"],
            "zh-HK" => &["MingLiU_HKSCS"],
            "zh-TW" => &["Microsoft JhengHei UI"],
            _ => &["Microsoft YaHei UI"],
        }
    }
    match script {
        Script::Adlam | Script::Ethiopic | Script::Tifinagh | Script::Vai => &["Ebrima"],
        Script::Bengali
        | Script::Chakma
        | Script::Devanagari
        | Script::Gujarati
        | Script::Gurmukhi
        | Script::Kannada
        | Script::Malayalam
        | Script::Oriya
        | Script::Sinhala
        | Script::Tamil
        | Script::Telugu => &["Nirmala UI"],
        Script::Canadian_Aboriginal | Script::Cherokee => &["Gadugi"],
        Script::Han => han(locale),
        Script::Hangul => han("ko"),
        Script::Hiragana | Script::Katakana => han("ja"),
        Script::Javanese => &["Javanese Text"],
        Script::Khmer | Script::Lao | Script::Thai => &["Leelawadee UI"],
        Script::Mongolian => &["Mongolian Baiti"],
        Script::Myanmar => &["Myanmar Text"],
        Script::Thaana => &["MV Boli"],
        Script::Tibetan => &["Microsoft Himalaya"],
        Script::Yi => &["Microsoft Yi Baiti"],
        _ => &[],
    }
}

/// Every family a glyph miss on `missing` should load: the script families
/// of each character, then [`COMMON_FALLBACK_FAMILIES`], deduplicated in order.
/// Whitespace and control characters never count as misses.
pub(crate) fn fallback_families(missing: &str, locale: &str) -> Vec<&'static str> {
    let mut families: Vec<&'static str> = Vec::new();
    let mut any = false;
    for c in missing.chars() {
        if c.is_whitespace() || c.is_control() {
            continue;
        }
        any = true;
        for family in script_fallback_families(c.script(), locale) {
            if !families.contains(family) {
                families.push(family);
            }
        }
    }
    if any {
        for family in COMMON_FALLBACK_FAMILIES {
            if !families.contains(&family) {
                families.push(family);
            }
        }
    }
    families
}

/// Loads the font files of one family name into a database, returning how
/// many files it loaded. The default is [`system::load_family`]; tests inject
/// open-licensed stand-ins.
pub(crate) type FamilySource = Arc<dyn Fn(&mut fontdb::Database, &str) -> usize + Send + Sync>;

/// Lazy, off-frame loader for the system fallback faces cosmic-text names
/// (see the module docs). Owned by the text rasterizer and carried over when
/// it is rebuilt, so each family loads at most once per process.
pub(crate) struct SystemFallback {
    source: FamilySource,
    /// Every family ever scheduled (present or not).
    requested: HashSet<&'static str>,
    tx: mpsc::Sender<Vec<fontdb::FaceInfo>>,
    rx: mpsc::Receiver<Vec<fontdb::FaceInfo>>,
    /// Background loads not yet received.
    in_flight: usize,
    /// Faces installed so far, replayed into a rebuilt font system.
    installed: Vec<fontdb::FaceInfo>,
    /// Wakes the compositor when a load lands (an idle HUD renders nothing
    /// otherwise).
    wake: RenderWakeNotifier,
}

impl SystemFallback {
    pub(crate) fn new(source: FamilySource) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            source,
            requested: HashSet::new(),
            tx,
            rx,
            in_flight: 0,
            installed: Vec::new(),
            wake: RenderWakeNotifier::default(),
        }
    }

    /// The production loader: Windows system families by name.
    pub(crate) fn system() -> Self {
        Self::new(Arc::new(system::load_family))
    }

    pub(crate) fn set_wake(&mut self, wake: RenderWakeNotifier) {
        self.wake = wake;
    }

    /// Background loads not yet installed.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Faces installed so far.
    #[cfg(test)]
    pub(crate) fn installed_face_count(&self) -> usize {
        self.installed.len()
    }

    /// Record that `missing` shaped to `.notdef` under `locale`, and start one
    /// background load of every fallback family not requested before. Never
    /// loads on the calling (frame) thread.
    pub(crate) fn note_missing(&mut self, missing: &str, locale: &str) {
        let families: Vec<&'static str> = fallback_families(missing, locale)
            .into_iter()
            .filter(|family| self.requested.insert(family))
            .collect();
        if families.is_empty() {
            return;
        }
        let source = Arc::clone(&self.source);
        let tx = self.tx.clone();
        let wake = self.wake.clone();
        let spawned = std::thread::Builder::new()
            .name("tze-font-fallback".to_owned())
            .spawn(move || {
                let started = std::time::Instant::now();
                let mut db = fontdb::Database::new();
                let (mut present, mut absent) = (Vec::new(), Vec::new());
                for family in &families {
                    if source(&mut db, family) > 0 {
                        present.push(*family);
                    } else {
                        absent.push(*family);
                    }
                }
                share_file_data(&mut db);
                let faces: Vec<fontdb::FaceInfo> = db.faces().cloned().collect();
                tracing::info!(
                    ?present,
                    ?absent,
                    faces = faces.len(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "system fallback fonts loaded off the frame thread"
                );
                // The receiver only goes away with the compositor.
                let _ = tx.send(faces);
                wake.notify();
            });
        match spawned {
            Ok(_) => self.in_flight += 1,
            Err(e) => tracing::warn!(error = %e, "fallback font loader thread unavailable"),
        }
    }

    /// Install every face whose background load has landed into `fs`.
    /// Returns `true` when faces were added, so the caller re-lays out the
    /// text shaped without them.
    pub(crate) fn poll(&mut self, fs: &mut FontSystem) -> bool {
        let mut added = false;
        while let Ok(faces) = self.rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            if faces.is_empty() {
                continue;
            }
            // db_mut() clears cosmic-text's font-match cache, so the next
            // shape considers the new faces.
            let db = fs.db_mut();
            for face in faces {
                db.push_face_info(face.clone());
                self.installed.push(face);
            }
            added = true;
        }
        added
    }

    /// Add every installed face to a newly built `fs`.
    pub(crate) fn replay_into(&self, fs: &mut FontSystem) {
        if self.installed.is_empty() {
            return;
        }
        let db = fs.db_mut();
        for face in &self.installed {
            db.push_face_info(face.clone());
        }
    }
}

/// Memory-map file-backed faces now, on the loader thread, so the frame
/// thread's first use of a face never opens or reads its file.
fn share_file_data(db: &mut fontdb::Database) {
    let ids: Vec<fontdb::ID> = db
        .faces()
        .filter(|face| matches!(face.source, fontdb::Source::File(_)))
        .map(|face| face.id)
        .collect();
    for id in ids {
        // SAFETY: maps an installed system font file read-only; Windows keeps
        // in-use font files locked against modification.
        unsafe {
            db.make_shared_face_data(id);
        }
    }
}

// ── System font lookup by name ────────────────────────────────────────────────

/// On-demand system font lookup by family name.
///
/// Windows: reads the value names of
/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts` (machine fonts,
/// relative file names under `%WINDIR%\Fonts`) and the same key under `HKCU`
/// (per-user fonts, absolute paths under `%LOCALAPPDATA%\Microsoft\Windows\Fonts`).
/// A value name such as `"Segoe UI Semibold (TrueType)"` or
/// `"Cambria & Cambria Math (TrueType)"` names the families in its file; only
/// files whose value name matches the requested family are loaded.
///
/// Other platforms: a no-op (Linux builds are headless CI only).
pub(crate) mod system {
    use glyphon::fontdb;

    /// Load the system font files for `family` into `db`; returns how many
    /// files were loaded.  Never scans a directory.
    pub(crate) fn load_family(db: &mut fontdb::Database, family: &str) -> usize {
        imp::load_family(db, family)
    }

    /// Upper bound on files loaded for one name; `"Arial"` style prefixes can
    /// match a large family (Arial, Arial Black, Arial Nova, …).
    #[cfg_attr(not(windows), allow(dead_code))] // used by the Windows impl only
    pub(crate) const MAX_FILES_PER_FAMILY: usize = 64;

    /// Does registry value name `value_name` describe a file containing
    /// `family`?  Strips the `" (TrueType)"`-style suffix, splits collections
    /// on `" & "`, and accepts an exact match or the family followed by a style
    /// word (`"Segoe UI Semibold"` matches `"Segoe UI"`).  Over-matching only
    /// loads an extra file; the caller re-checks real family names after loading.
    #[cfg_attr(not(windows), allow(dead_code))] // Windows impl + unit tests
    pub(crate) fn registry_name_matches(value_name: &str, family: &str) -> bool {
        let base = match value_name.rfind(" (") {
            Some(i) if value_name.ends_with(')') => &value_name[..i],
            _ => value_name,
        };
        let family = family.trim();
        if family.is_empty() {
            return false;
        }
        base.split(" & ").any(|part| {
            let part = part.trim();
            part.len() >= family.len()
                && part.is_char_boundary(family.len())
                && part[..family.len()].eq_ignore_ascii_case(family)
                && (part.len() == family.len() || part.as_bytes()[family.len()] == b' ')
        })
    }

    #[cfg(windows)]
    mod imp {
        use super::{MAX_FILES_PER_FAMILY, registry_name_matches};
        use glyphon::fontdb;
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use std::path::{Path, PathBuf};
        use windows::Win32::Foundation::ERROR_NO_MORE_ITEMS;
        use windows::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ,
            REG_VALUE_TYPE, RegCloseKey, RegEnumValueW, RegOpenKeyExW,
        };
        use windows::core::{PCWSTR, PWSTR, w};

        const FONTS_KEY: PCWSTR = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Fonts");

        pub(super) fn load_family(db: &mut fontdb::Database, family: &str) -> usize {
            let windir = std::env::var_os("WINDIR")
                .or_else(|| std::env::var_os("SystemRoot"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
            let fonts_dir = windir.join("Fonts");
            let mut files: Vec<PathBuf> = Vec::new();
            for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
                for (name, data) in font_values(root) {
                    if registry_name_matches(&name, family) {
                        let path = Path::new(&data);
                        let path = if path.is_absolute() {
                            path.to_path_buf()
                        } else {
                            fonts_dir.join(path)
                        };
                        if !files.contains(&path) {
                            files.push(path);
                        }
                    }
                }
            }
            let mut loaded = 0;
            for path in files.iter().take(MAX_FILES_PER_FAMILY) {
                match db.load_font_file(path) {
                    Ok(()) => loaded += 1,
                    Err(e) => tracing::debug!(
                        file = %path.display(),
                        error = %e,
                        "system font file unreadable; skipped"
                    ),
                }
            }
            loaded
        }

        /// `(value name, string data)` pairs of the `Fonts` key under `root`.
        fn font_values(root: HKEY) -> Vec<(String, String)> {
            let mut out = Vec::new();
            let mut key = HKEY::default();
            // SAFETY: FONTS_KEY is a static NUL-terminated wide string; `key`
            // is written by the call and closed below.
            if unsafe { RegOpenKeyExW(root, FONTS_KEY, 0, KEY_READ, &mut key) }.is_err() {
                return out;
            }
            let mut name_buf = vec![0u16; 1024];
            let mut data_buf = vec![0u8; 2048];
            // Bounded: the walk ends at ERROR_NO_MORE_ITEMS long before this.
            for index in 0..100_000 {
                let mut name_len = name_buf.len() as u32;
                let mut data_len = data_buf.len() as u32;
                let mut kind = 0u32;
                // SAFETY: buffers are live and their lengths are passed in
                // elements (name) / bytes (data) as the API requires.
                let rc = unsafe {
                    RegEnumValueW(
                        key,
                        index,
                        PWSTR(name_buf.as_mut_ptr()),
                        &mut name_len,
                        None,
                        Some(&mut kind),
                        Some(data_buf.as_mut_ptr()),
                        Some(&mut data_len),
                    )
                };
                if rc == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if rc.is_err() {
                    // e.g. ERROR_MORE_DATA: a value larger than the buffers
                    // is not a font entry we can use; skip it.
                    continue;
                }
                let kind = REG_VALUE_TYPE(kind);
                if kind != REG_SZ && kind != REG_EXPAND_SZ {
                    continue;
                }
                let name = OsString::from_wide(&name_buf[..name_len as usize])
                    .to_string_lossy()
                    .into_owned();
                let wide: Vec<u16> = data_buf[..data_len as usize]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&u| u != 0)
                    .collect();
                let data = OsString::from_wide(&wide).to_string_lossy().into_owned();
                out.push((name, data));
            }
            // SAFETY: `key` was opened above and is closed exactly once.
            unsafe {
                let _ = RegCloseKey(key);
            }
            out
        }
    }

    #[cfg(not(windows))]
    mod imp {
        use glyphon::fontdb;

        pub(super) fn load_family(_db: &mut fontdb::Database, _family: &str) -> usize {
            0
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use glyphon::{Attrs, Buffer, Family, Metrics, Shaping, Wrap};

    /// Shape `text` in `family` and return the family name of the face used
    /// for each glyph.
    fn faces_used(fs: &mut FontSystem, family: Family<'_>, text: &str) -> Vec<String> {
        let mut buf = Buffer::new(fs, Metrics::new(16.0, 22.0));
        buf.set_size(fs, Some(2000.0), Some(200.0));
        buf.set_wrap(fs, Wrap::None);
        buf.set_text(fs, text, Attrs::new().family(family), Shaping::Advanced);
        buf.shape_until_scroll(fs, false);
        let ids: Vec<_> = buf
            .layout_runs()
            .flat_map(|r| r.glyphs.iter().map(|g| (g.font_id, g.glyph_id)))
            .collect();
        ids.into_iter()
            .map(|(id, glyph)| {
                assert_ne!(glyph, 0, "notdef glyph shaped for {text:?}");
                fs.db().face(id).unwrap().families[0].0.clone()
            })
            .collect()
    }

    // ── Lazy system fallback (hud-ljhku.13) ──────────────────────────────

    use test_standins::{STANDIN_EMOJI, all_standins, standin_source};

    /// `(family, glyph id)` of every shaped glyph of `text` in the sans role.
    fn shaped(fs: &mut FontSystem, text: &str) -> Vec<(String, u16)> {
        let mut buf = Buffer::new(fs, Metrics::new(16.0, 22.0));
        buf.set_size(fs, Some(2000.0), Some(200.0));
        buf.set_text(
            fs,
            text,
            Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
        );
        buf.shape_until_scroll(fs, false);
        let glyphs: Vec<_> = buf
            .layout_runs()
            .flat_map(|r| {
                r.glyphs
                    .iter()
                    .filter(|g| !r.text[g.start..g.end].trim().is_empty())
                    .map(|g| (g.font_id, g.glyph_id))
                    .collect::<Vec<_>>()
            })
            .collect();
        glyphs
            .into_iter()
            .map(|(id, glyph)| (fs.db().face(id).unwrap().families[0].0.clone(), glyph))
            .collect()
    }

    /// The characters of `text` that shaped to `.notdef`.
    fn notdef_text(fs: &mut FontSystem, text: &str) -> String {
        let mut buf = Buffer::new(fs, Metrics::new(16.0, 22.0));
        buf.set_size(fs, Some(2000.0), Some(200.0));
        buf.set_text(
            fs,
            text,
            Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
        );
        buf.shape_until_scroll(fs, false);
        let mut missing = String::new();
        for run in buf.layout_runs() {
            for g in run.glyphs.iter().filter(|g| g.glyph_id == 0) {
                missing.push_str(&run.text[g.start..g.end]);
            }
        }
        missing
    }

    /// Poll until a background load lands (bounded).
    fn wait_and_install(fallback: &mut SystemFallback, fs: &mut FontSystem) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if fallback.poll(fs) {
                return true;
            }
            if fallback.in_flight() == 0 {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("fallback load did not land in 10 s");
    }

    /// R13.1 S1/S3/S4: nothing loads until a glyph misses; the miss loads the
    /// families cosmic-text names on a background thread; text shaped before
    /// the load lands is tofu, and after one install every glyph is real.
    /// Each family is requested once.
    #[test]
    fn fallback_loads_lazily_off_thread_then_text_has_no_notdef() {
        let text = "\u{2705} \u{5b8c}\u{6210} \u{d14c}\u{c2a4}\u{d2b8}";
        let (source, log) = standin_source(&all_standins());
        let mut fallback = SystemFallback::new(source);
        let mut fs = bundled_font_system();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        fallback.set_wake(RenderWakeNotifier::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        // S3: text the bundled faces cover asks for nothing.
        let covered = notdef_text(&mut fs, "Hello \u{2192} \u{25cf}");
        assert!(covered.is_empty(), "{covered:?}");
        fallback.note_missing(&covered, "en-US");
        assert_eq!(fallback.in_flight(), 0);
        assert!(log.lock().unwrap().is_empty(), "no fallback face loads");
        assert_eq!(fs.db().faces().count(), BUNDLED_FONT_FACE_COUNT);

        // S4: before the load lands the text is tofu.
        let missing = notdef_text(&mut fs, text);
        assert!(!missing.is_empty(), "the bundled faces lack these glyphs");
        fallback.note_missing(&missing, "en-US");
        assert_eq!(fallback.in_flight(), 1, "one background load");
        assert!(shaped(&mut fs, text).iter().any(|(_, g)| *g == 0));

        assert!(wait_and_install(&mut fallback, &mut fs));
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 1);
        // S1: after the install, no shaped glyph is .notdef.
        let after = shaped(&mut fs, text);
        assert!(after.iter().all(|(_, g)| *g != 0), "{after:?}");
        let families: HashSet<&str> = after.iter().map(|(f, _)| f.as_str()).collect();
        for expected in ["TzeStandIn Zh", "TzeStandIn Ko"] {
            assert!(families.contains(expected), "{families:?}");
        }

        // The loader never ran on this (frame) thread.
        let calls = log.lock().unwrap().clone();
        assert!(!calls.is_empty());
        for (family, thread) in &calls {
            assert_eq!(thread.as_deref(), Some("tze-font-fallback"), "{family}");
        }
        // en-US Han -> Microsoft YaHei UI; Hangul -> Malgun Gothic; then the
        // common emoji/symbol faces, each asked for exactly once.
        let asked: Vec<&str> = calls.iter().map(|(f, _)| f.as_str()).collect();
        for family in [
            "Microsoft YaHei UI",
            "Malgun Gothic",
            "Segoe UI Emoji",
            "Segoe UI Symbol",
        ] {
            assert_eq!(
                asked.iter().filter(|f| **f == family).count(),
                1,
                "{asked:?}"
            );
        }

        // A repeat miss (or any later frame) starts no new load.
        fallback.note_missing(&missing, "en-US");
        assert_eq!(fallback.in_flight(), 0);
        assert_eq!(log.lock().unwrap().len(), calls.len());
        assert!(!fallback.poll(&mut fs));
    }

    /// R13.1 S5: with a Japanese user locale, kanji load and shape with the
    /// Yu Gothic face (Japanese forms), never Microsoft YaHei UI.
    #[test]
    fn japanese_locale_shapes_kanji_with_yu_gothic() {
        let locale = locale_from(None, || Some("ja-JP".to_owned()));
        assert_eq!(locale, "ja");
        let (source, log) = standin_source(&all_standins());
        let mut fallback = SystemFallback::new(source);
        let mut fs = bundled_font_system();
        let missing = notdef_text(&mut fs, "\u{5b8c}\u{6210}");
        fallback.note_missing(&missing, &locale);
        assert!(wait_and_install(&mut fallback, &mut fs));
        let used = shaped(&mut fs, "\u{5b8c}\u{6210}");
        assert_eq!(
            used,
            [
                ("TzeStandIn Ja".to_owned(), 1),
                ("TzeStandIn Ja".to_owned(), 2)
            ]
        );
        let asked: Vec<String> = log.lock().unwrap().iter().map(|(f, _)| f.clone()).collect();
        assert!(asked.contains(&"Yu Gothic".to_owned()), "{asked:?}");
        assert!(
            !asked.contains(&"Microsoft YaHei UI".to_owned()),
            "{asked:?}"
        );
    }

    /// R13.1 S2: an absent family is skipped and the next one is used; the
    /// miss is never retried, so nothing repeats per frame.
    #[test]
    fn absent_fallback_family_is_skipped_once() {
        // Segoe UI Emoji is absent; Segoe UI Symbol (next) has the glyph.
        let (source, log) = standin_source(&[("Segoe UI Symbol", STANDIN_EMOJI)]);
        let mut fallback = SystemFallback::new(source);
        let mut fs = bundled_font_system();
        let missing = notdef_text(&mut fs, "\u{2705}");
        assert_eq!(missing, "\u{2705}");
        fallback.note_missing(&missing, "en-US");
        assert!(wait_and_install(&mut fallback, &mut fs));
        assert_eq!(
            shaped(&mut fs, "\u{2705}"),
            [("TzeStandIn Emoji".to_owned(), 1)]
        );
        let asked = log.lock().unwrap().len();
        assert_eq!(asked, 2, "emoji (absent) then symbol");
        for _ in 0..100 {
            fallback.note_missing(&missing, "en-US");
        }
        assert_eq!(fallback.in_flight(), 0);
        assert_eq!(log.lock().unwrap().len(), asked, "no per-frame retries");
    }

    /// A rebuilt font system (theme change) gets the installed faces back
    /// without loading any family again.
    #[test]
    fn installed_fallback_faces_replay_into_a_rebuilt_font_system() {
        let (source, log) = standin_source(&all_standins());
        let mut fallback = SystemFallback::new(source);
        let mut fs = bundled_font_system();
        let missing = notdef_text(&mut fs, "\u{d14c}");
        fallback.note_missing(&missing, "en-US");
        assert!(wait_and_install(&mut fallback, &mut fs));
        let installed = fallback.installed_face_count();
        assert!(installed > 0);
        let calls = log.lock().unwrap().len();

        let mut rebuilt = bundled_font_system();
        fallback.replay_into(&mut rebuilt);
        assert_eq!(
            rebuilt.db().faces().count(),
            BUNDLED_FONT_FACE_COUNT + installed
        );
        assert!(notdef_text(&mut rebuilt, "\u{d14c}").is_empty());
        assert_eq!(log.lock().unwrap().len(), calls);
    }

    /// Tier-W (windows.yml): the real Microsoft faces through the real
    /// registry-by-name loader. Each sample must shape without `.notdef` in
    /// the family cosmic-text's table names, for the default and a Japanese
    /// locale (R13.1 S5); a family the runner lacks is skipped and recorded.
    #[cfg(windows)]
    #[test]
    fn windows_system_fallback_draws_real_faces() {
        for (locale, han) in [("en-US", "Microsoft YaHei UI"), ("ja", "Yu Gothic")] {
            let (mut fs, _) =
                build_font_system_with_locale(&FontConfig::default(), locale.to_owned());
            let samples = [
                ("\u{2705}", "Segoe UI Emoji"),
                ("\u{5b8c}\u{6210}", han),
                ("\u{d14c}\u{c2a4}\u{d2b8}", "Malgun Gothic"),
            ];
            let text: String = samples
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>()
                .join(" ");
            let mut fallback = SystemFallback::system();
            let missing = notdef_text(&mut fs, &text);
            fallback.note_missing(&missing, locale);
            wait_and_install(&mut fallback, &mut fs);
            for (sample, family) in samples {
                let has = |fs: &FontSystem, id| {
                    fs.db().face(id).is_some_and(|face| {
                        face.families
                            .iter()
                            .any(|(n, _)| n.eq_ignore_ascii_case(family))
                    })
                };
                let installed = fs.db().faces().any(|face| {
                    face.families
                        .iter()
                        .any(|(n, _)| n.eq_ignore_ascii_case(family))
                });
                if !installed {
                    println!(
                        "tier-W font fallback [{locale}]: {family} not on this runner; skipped"
                    );
                    continue;
                }
                let mut buf = Buffer::new(&mut fs, Metrics::new(16.0, 22.0));
                buf.set_text(
                    &mut fs,
                    sample,
                    Attrs::new().family(Family::SansSerif),
                    Shaping::Advanced,
                );
                buf.shape_until_scroll(&mut fs, false);
                let glyphs: Vec<_> = buf
                    .layout_runs()
                    .flat_map(|r| r.glyphs.iter().map(|g| (g.font_id, g.glyph_id)))
                    .collect();
                assert!(!glyphs.is_empty());
                for (id, glyph) in glyphs {
                    assert_ne!(glyph, 0, "[{locale}] {sample:?} shaped .notdef");
                    assert!(
                        has(&fs, id),
                        "[{locale}] {sample:?} not drawn with {family}"
                    );
                }
                println!(
                    "tier-W font fallback [{locale}]: {family} present; {sample} drawn with it"
                );
            }
        }
    }

    #[test]
    fn fallback_families_follow_cosmic_text_windows_table() {
        assert_eq!(
            fallback_families("\u{5b8c}", "ja"),
            ["Yu Gothic", "Segoe UI Emoji", "Segoe UI Symbol"]
        );
        assert_eq!(
            fallback_families("\u{5b8c}", "zh-TW")[0],
            "Microsoft JhengHei UI"
        );
        assert_eq!(fallback_families("\u{5b8c}", "zh-HK")[0], "MingLiU_HKSCS");
        assert_eq!(
            fallback_families("\u{5b8c}", "en-US")[0],
            "Microsoft YaHei UI"
        );
        assert_eq!(fallback_families("\u{d14c}", "en-US")[0], "Malgun Gothic");
        assert_eq!(fallback_families("\u{304b}", "en-US")[0], "Yu Gothic");
        assert_eq!(fallback_families("\u{0915}", "en-US")[0], "Nirmala UI");
        assert_eq!(
            fallback_families("\u{2705}", "en-US"),
            COMMON_FALLBACK_FAMILIES
        );
        assert!(fallback_families(" \n\t", "en-US").is_empty());
    }

    /// R13.1 S5 and the other Han keys: cosmic-text matches them exactly.
    #[test]
    fn locale_normalizes_to_cosmic_text_han_keys() {
        for (tag, want) in [
            ("ja-JP", "ja"),
            ("ja", "ja"),
            ("ko-KR", "ko"),
            ("zh-TW", "zh-TW"),
            ("zh-Hant-TW", "zh-TW"),
            ("zh-Hant", "zh-TW"),
            ("zh-HK", "zh-HK"),
            ("zh-MO", "zh-HK"),
            ("zh-Hant-HK", "zh-HK"),
            ("zh-CN", "zh-CN"),
            ("zh-Hans-CN", "zh-Hans-CN"),
            ("en-US", "en-US"),
        ] {
            assert_eq!(normalize_locale(tag), want, "{tag}");
        }
        // LANG wins over the Windows user locale; either is normalized.
        assert_eq!(
            locale_from(Some("ja_JP.UTF-8".into()), || Some("en-US".into())),
            "ja"
        );
        assert_eq!(locale_from(None, || Some("zh-TW".into())), "zh-TW");
        assert_eq!(locale_from(Some(String::new()), || None), "en-US");
        assert_eq!(locale_from(None, || None), "en-US");
    }

    #[test]
    fn bundled_fonts_load_exactly_the_embedded_faces() {
        // Each embedded file is a single-face TTF; more faces would mean
        // system fonts leaked in.
        assert_eq!(
            bundled_font_system().db().faces().count(),
            BUNDLED_FONT_FACE_COUNT
        );
    }

    #[test]
    fn roles_default_to_ibm_plex_and_dejavu_serif() {
        let mut fs = bundled_font_system();
        for (family, expected) in [
            (Family::SansSerif, DEFAULT_SANS_FAMILY),
            (Family::Monospace, DEFAULT_MONO_FAMILY),
            (Family::Serif, DEFAULT_SERIF_FAMILY),
        ] {
            let used = faces_used(&mut fs, family, "Hello world");
            assert!(
                used.iter().all(|f| f == expected),
                "{family:?} used {used:?}"
            );
        }
    }

    #[test]
    fn plex_weights_resolve_to_distinct_faces() {
        let mut fs = bundled_font_system();
        let mut ids = Vec::new();
        for weight in [400, 500, 600, 700] {
            let mut buf = Buffer::new(&mut fs, Metrics::new(16.0, 22.0));
            let attrs = Attrs::new()
                .family(Family::SansSerif)
                .weight(glyphon::Weight(weight));
            buf.set_text(&mut fs, "A", attrs, Shaping::Advanced);
            buf.shape_until_scroll(&mut fs, false);
            ids.push(buf.layout_runs().next().unwrap().glyphs[0].font_id);
        }
        ids.dedup();
        assert_eq!(
            ids.len(),
            4,
            "400/500/600/700 must each map to their own face"
        );
    }

    /// Symbols the HUD and its bundled widgets emit that IBM Plex lacks must
    /// still shape to real glyphs via the DejaVu coverage fallback.
    #[test]
    fn symbols_missing_from_plex_fall_back_to_dejavu() {
        let mut fs = bundled_font_system();
        let symbols = "─▍⚠✕●◐▲◆◇○⋯⇒☾⊘◌☀☕⚡سلام";
        let used = faces_used(&mut fs, Family::SansSerif, symbols);
        assert!(!used.is_empty());
        assert!(used.iter().all(|f| f.starts_with("DejaVu")), "{used:?}");
        // Plex Mono has box drawing itself; other symbols fall back to the
        // monospaced DejaVu face.
        let used = faces_used(&mut fs, Family::Monospace, "─⚠");
        assert_eq!(used, ["IBM Plex Mono", "DejaVu Sans Mono"]);
    }

    #[test]
    fn configured_bundled_name_resolves_case_insensitively() {
        let config = FontConfig {
            sans: Some("dejavu sans".into()),
            mono: Some("DejaVu Sans Mono".into()),
            ..Default::default()
        };
        let (_, resolved) = build_font_system(&config.normalized());
        assert_eq!(resolved.sans, "DejaVu Sans");
        assert_eq!(resolved.mono, "DejaVu Sans Mono");
        assert_eq!(resolved.serif, DEFAULT_SERIF_FAMILY);
    }

    #[test]
    fn missing_family_falls_back_to_role_default() {
        let config = FontConfig {
            sans: Some("No Such Family 1f3a".into()),
            ..Default::default()
        };
        let (mut fs, resolved) = build_font_system(&config);
        assert_eq!(resolved, ResolvedFonts::default());
        assert!(
            faces_used(&mut fs, Family::SansSerif, "Hi")
                .iter()
                .all(|f| f == DEFAULT_SANS_FAMILY)
        );
    }

    #[test]
    fn fonts_dir_family_is_loaded_and_selected() {
        let dir = std::env::temp_dir().join(format!("tze_hud_fonts_dir_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // DejaVu Serif stands in for an operator font: it is a family the
        // roles do not use for sans, so selecting it proves the lookup path.
        std::fs::write(dir.join("Operator.TTF"), DEJAVU_SERIF).unwrap();
        std::fs::write(dir.join("notes.txt"), b"not a font").unwrap();
        let config = FontConfig {
            sans: Some("DejaVu Serif".into()),
            fonts_dir: Some(dir.clone()),
            ..Default::default()
        };
        let (fs, resolved) = build_font_system(&config);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(resolved.sans, "DejaVu Serif");
        assert_eq!(fs.db().faces().count(), BUNDLED_FONT_FACE_COUNT + 1);
        // Widget text mirrors exactly the non-bundled file.
        assert_eq!(loaded_font_files(fs.db()), [dir.join("Operator.TTF")]);
        assert!(loaded_font_files(bundled_font_system().db()).is_empty());
    }

    #[test]
    fn missing_fonts_dir_is_not_fatal() {
        let config = FontConfig {
            fonts_dir: Some(PathBuf::from("/nonexistent/tze_hud/fonts")),
            ..Default::default()
        };
        let (fs, resolved) = build_font_system(&config);
        assert_eq!(resolved, ResolvedFonts::default());
        assert_eq!(fs.db().faces().count(), BUNDLED_FONT_FACE_COUNT);
    }

    #[test]
    fn token_map_config_normalizes_defaults_and_keywords() {
        let tokens: HashMap<String, String> = [
            (TOKEN_FONT_SANS, "IBM Plex Sans"),
            (TOKEN_FONT_MONO, "monospace"),
            (TOKEN_FONT_SERIF, "  Georgia "),
            (RUNTIME_FONTS_DIR_KEY, "/cfg/fonts"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let config = FontConfig::from_token_map(&tokens);
        assert_eq!(
            config,
            FontConfig {
                sans: None,
                mono: None,
                serif: Some("Georgia".into()),
                fonts_dir: Some(PathBuf::from("/cfg/fonts")),
            }
        );
        assert_eq!(
            FontConfig::from_token_map(&HashMap::new()),
            FontConfig::default()
        );
    }

    #[test]
    fn registry_value_names_match_family_and_styles_only() {
        use system::registry_name_matches as m;
        assert!(m("Segoe UI (TrueType)", "Segoe UI"));
        assert!(m("Segoe UI Semibold Italic (TrueType)", "segoe ui"));
        assert!(m("Cambria & Cambria Math (TrueType)", "Cambria Math"));
        assert!(m("Bahnschrift", "Bahnschrift"));
        assert!(!m("Segoe UIX (TrueType)", "Segoe UI"));
        assert!(!m("Arial (TrueType)", "Arial Nova"));
        assert!(!m("Arial (TrueType)", ""));
    }

    #[test]
    fn system_lookup_is_noop_off_windows() {
        if cfg!(windows) {
            return;
        }
        let mut db = fontdb::Database::new();
        assert_eq!(system::load_family(&mut db, "Segoe UI"), 0);
        assert_eq!(db.len(), 0);
    }
}

/// Stand-in "system" fallback faces for tests (hud-ljhku.13).
#[cfg(test)]
pub(crate) mod test_standins {
    use super::FamilySource;
    use glyphon::fontdb;
    use std::sync::{Arc, Mutex};

    /// Open-licensed (CC0) box-glyph stand-ins for the Microsoft faces,
    /// generated by `fonts/test-standins/make_standins.py`.
    pub(crate) static STANDIN_EMOJI: &[u8] =
        include_bytes!("../fonts/test-standins/TzeStandInEmoji.ttf");
    pub(crate) static STANDIN_JA: &[u8] = include_bytes!("../fonts/test-standins/TzeStandInJa.ttf");
    pub(crate) static STANDIN_ZH: &[u8] = include_bytes!("../fonts/test-standins/TzeStandInZh.ttf");
    pub(crate) static STANDIN_KO: &[u8] = include_bytes!("../fonts/test-standins/TzeStandInKo.ttf");

    /// What the stand-in "system" source was asked for, and on which thread.
    pub(crate) type SourceLog = Arc<Mutex<Vec<(String, Option<String>)>>>;

    /// A [`FamilySource`] serving stand-ins for the named Windows families;
    /// families missing from `present` are absent.
    pub(crate) fn standin_source(
        present: &[(&'static str, &'static [u8])],
    ) -> (FamilySource, SourceLog) {
        let present: Vec<(&'static str, &'static [u8])> = present.to_vec();
        let log: SourceLog = Arc::default();
        let calls = Arc::clone(&log);
        let source: FamilySource = Arc::new(move |db: &mut fontdb::Database, family: &str| {
            let thread = std::thread::current().name().map(str::to_owned);
            calls.lock().unwrap().push((family.to_owned(), thread));
            match present.iter().find(|(name, _)| *name == family) {
                Some((_, bytes)) => {
                    db.load_font_source(fontdb::Source::Binary(Arc::new(*bytes)));
                    1
                }
                None => 0,
            }
        });
        (source, log)
    }

    /// Every stand-in, under the family names cosmic-text's table uses.
    pub(crate) fn all_standins() -> Vec<(&'static str, &'static [u8])> {
        vec![
            ("Segoe UI Emoji", STANDIN_EMOJI),
            ("Yu Gothic", STANDIN_JA),
            ("Microsoft YaHei UI", STANDIN_ZH),
            ("Malgun Gothic", STANDIN_KO),
        ]
    }
}
