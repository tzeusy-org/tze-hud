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
//! bundled faces with the default role families; it does not follow a
//! configured [`FontConfig`].
//!
//! # Configuring a family
//!
//! A [`FontConfig`] names a family per role (free-form strings, normally the
//! `font.sans` / `font.mono` / `font.serif` design tokens; see
//! [`FontConfig::from_token_map`]).  [`build_font_system`] resolves each name,
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
//! # Licenses
//!
//! - IBM Plex Sans / Mono: SIL Open Font License 1.1, `fonts/ibm-plex/LICENSE.txt`.
//!   Source: <https://github.com/IBM/plex> releases `@ibm/plex-sans@1.1.0` and
//!   `@ibm/plex-mono@2.5.0` (`fonts/complete/ttf/`, unmodified).
//! - DejaVu: Bitstream Vera derived permissive license, `fonts/dejavu/LICENSE`.
//!   Source: <https://dejavu-fonts.github.io/>

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use glyphon::{FontSystem, fontdb};

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
pub const BUNDLED_FONT_FACE_COUNT: usize = 11;

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
pub const DEFAULT_SANS_FAMILY: &str = "IBM Plex Sans";
/// Default family for the mono role ([`tze_hud_scene::types::FontFamily::SystemMonospace`]).
pub const DEFAULT_MONO_FAMILY: &str = "IBM Plex Mono";
/// Default family for the serif role ([`tze_hud_scene::types::FontFamily::SystemSerif`]).
pub const DEFAULT_SERIF_FAMILY: &str = "DejaVu Serif";

/// Design-token key naming the sans role's family.
pub const TOKEN_FONT_SANS: &str = "font.sans";
/// Design-token key naming the mono role's family.
pub const TOKEN_FONT_MONO: &str = "font.mono";
/// Design-token key naming the serif role's family.
pub const TOKEN_FONT_SERIF: &str = "font.serif";
/// Design-token key holding the absolute fonts directory (set by the runtime
/// to `<config dir>/fonts` when that directory exists).
pub const TOKEN_FONT_DIR: &str = "font.dir";

// ── Configuration ─────────────────────────────────────────────────────────────

/// Which family each font role uses, plus where operator font files live.
///
/// `None` (or a generic keyword such as `"sans-serif"`) means the role default.
/// Build a font system from it with [`build_font_system`], or hand it to
/// [`crate::Compositor::set_font_config`] to apply it to a running compositor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FontConfig {
    /// Family for the sans role (default [`DEFAULT_SANS_FAMILY`]).
    pub sans: Option<String>,
    /// Family for the mono role (default [`DEFAULT_MONO_FAMILY`]).
    pub mono: Option<String>,
    /// Family for the serif role (default [`DEFAULT_SERIF_FAMILY`]).
    pub serif: Option<String>,
    /// Directory whose font files are loaded before names are resolved.
    pub fonts_dir: Option<PathBuf>,
}

impl FontConfig {
    /// Read the `font.sans`, `font.mono`, `font.serif` and `font.dir` keys of a
    /// resolved design-token map.  Absent keys leave the role at its default.
    pub fn from_token_map(tokens: &HashMap<String, String>) -> Self {
        let get = |key: &str| tokens.get(key).cloned();
        Self {
            sans: get(TOKEN_FONT_SANS),
            mono: get(TOKEN_FONT_MONO),
            serif: get(TOKEN_FONT_SERIF),
            fonts_dir: get(TOKEN_FONT_DIR).map(PathBuf::from),
        }
        .normalized()
    }

    /// Canonical form: names trimmed, and empty names, generic keywords and
    /// names equal to the role default all become `None`, so two configs that
    /// select the same fonts compare equal (no pointless rebuilds).
    pub fn normalized(self) -> Self {
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
pub struct ResolvedFonts {
    pub sans: String,
    pub mono: String,
    pub serif: String,
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
pub fn build_font_system(config: &FontConfig) -> (FontSystem, ResolvedFonts) {
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

    (FontSystem::new_with_locale_and_db(locale(), db), resolved)
}

/// Return all bundled font faces as [`fontdb::Source::Binary`] sources.
///
/// Useful when adding the bundled faces to an existing [`fontdb::Database`].
pub fn bundled_font_sources() -> impl Iterator<Item = fontdb::Source> {
    BUNDLED_FACES
        .iter()
        .copied()
        .map(|bytes| fontdb::Source::Binary(Arc::new(bytes)))
}

/// Raw bytes of every bundled face, for font databases of a different fontdb
/// version (resvg's widget text).
pub fn bundled_face_bytes() -> impl Iterator<Item = &'static [u8]> {
    BUNDLED_FACES.iter().copied()
}

/// The family name `db` knows that equals `name` case-insensitively, if any.
pub fn find_family(db: &fontdb::Database, name: &str) -> Option<String> {
    db.faces()
        .flat_map(|face| face.families.iter())
        .find(|(family, _)| family.eq_ignore_ascii_case(name))
        .map(|(family, _)| family.clone())
}

// ── Internals ─────────────────────────────────────────────────────────────────

/// Locale from `LANG` (e.g. `en_US.UTF-8` → `en-US`), else `en-US` — the same
/// fallback cosmic-text uses, without a direct `sys-locale` dependency.
fn locale() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|v| {
            let v = v.replace('_', "-");
            let v = v.split('.').next().unwrap_or("").to_owned();
            if v.is_empty() { None } else { Some(v) }
        })
        .unwrap_or_else(|| String::from("en-US"))
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
pub mod system {
    use glyphon::fontdb;

    /// Load the system font files for `family` into `db`; returns how many
    /// files were loaded.  Never scans a directory.
    pub fn load_family(db: &mut fontdb::Database, family: &str) -> usize {
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
            (TOKEN_FONT_DIR, "/cfg/fonts"),
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
