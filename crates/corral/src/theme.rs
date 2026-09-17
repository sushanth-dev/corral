//! Chrome theming: the statusline, gutters, cursor, search highlights,
//! and the binding picker.
//!
//! Pane content is not themed here. It keeps resolving `CellColor` through
//! the terminal's own palette, which is the correct behavior for a terminal
//! application; only the client's own paint uses these colors.

use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Catppuccin Mocha, compiled in so the default needs no file read.
const BUNDLED_JSON: &str = include_str!("../themes/catppuccin-mocha.json");

#[derive(Deserialize)]
struct RawPalette {
    gutter: String,
    focused_gutter: String,
    cursor_bg: String,
    cursor_fg: String,
    search_bg: String,
    search_fg: String,
    current_hit_bg: String,
    current_hit_fg: String,
    hint_bg: String,
    hint_fg: String,
    // The statusline and picker colors landed after the first theme files
    // shipped. Serde defaults (the bundled Mocha values) keep an older
    // theme file loading unchanged; the ten fields above stay required, so
    // a genuinely malformed file still fails to parse.
    #[serde(default = "default_bar_bg")]
    bar_bg: String,
    #[serde(default = "default_bar_fg")]
    bar_fg: String,
    #[serde(default = "default_mode_bg")]
    mode_bg: String,
    #[serde(default = "default_mode_fg")]
    mode_fg: String,
    #[serde(default = "default_muted")]
    muted: String,
    #[serde(default = "default_picker_bg")]
    picker_bg: String,
    #[serde(default = "default_picker_selected_bg")]
    picker_selected_bg: String,
}

// Defaults mirror the bundled Catppuccin Mocha chrome: mantle for the bar
// and picker, blue for the mode block, overlay0 for what the eye passes
// over.
fn default_bar_bg() -> String {
    "#181825".into()
}
fn default_bar_fg() -> String {
    "#a6adc8".into()
}
fn default_mode_bg() -> String {
    "#89b4fa".into()
}
fn default_mode_fg() -> String {
    "#11111b".into()
}
fn default_muted() -> String {
    "#7f849c".into()
}
fn default_picker_bg() -> String {
    "#181825".into()
}
fn default_picker_selected_bg() -> String {
    "#313244".into()
}

#[derive(Deserialize)]
struct RawTheme {
    name: String,
    palette: RawPalette,
}

/// The client's chrome colors, one per surface. Every color is
/// `Color::Rgb`, parsed from the theme file's `#rrggbb` strings: the theme
/// is exact, unlike pane content, which keeps its indices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub gutter: Color,
    pub focused_gutter: Color,
    pub cursor_bg: Color,
    pub cursor_fg: Color,
    pub search_bg: Color,
    pub search_fg: Color,
    pub current_hit_bg: Color,
    pub current_hit_fg: Color,
    pub hint_bg: Color,
    pub hint_fg: Color,
    /// The statusline's surface, darker than the terminal's base so the
    /// bar reads as chrome and not as a pane.
    pub bar_bg: Color,
    /// Ordinary statusline text.
    pub bar_fg: Color,
    /// The mode block's background and the accent the chrome points with.
    pub mode_bg: Color,
    /// Text on the mode block and the prompt.
    pub mode_fg: Color,
    /// Separators, footers, anything the eye can pass over.
    pub muted: Color,
    /// The picker window's surface.
    pub picker_bg: Color,
    /// The picker's selected row.
    pub picker_selected_bg: Color,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub palette: Palette,
}

/// A `#rrggbb` string to `Color::Rgb`. The `#` and exactly six hex digits
/// are required; anything else is a malformed theme file, not a color to
/// guess at.
fn parse_hex(field: &str, value: &str) -> Result<Color> {
    let digits = value
        .strip_prefix('#')
        .with_context(|| format!("{field}: {value:?} must start with '#'"))?;
    anyhow::ensure!(
        digits.len() == 6 && digits.chars().all(|c| c.is_ascii_hexdigit()),
        "{field}: {value:?} must be '#' followed by six hex digits"
    );
    let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).unwrap();
    Ok(Color::Rgb(byte(0), byte(2), byte(4)))
}

impl Palette {
    fn from_raw(raw: RawPalette) -> Result<Self> {
        Ok(Self {
            gutter: parse_hex("gutter", &raw.gutter)?,
            focused_gutter: parse_hex("focused_gutter", &raw.focused_gutter)?,
            cursor_bg: parse_hex("cursor_bg", &raw.cursor_bg)?,
            cursor_fg: parse_hex("cursor_fg", &raw.cursor_fg)?,
            search_bg: parse_hex("search_bg", &raw.search_bg)?,
            search_fg: parse_hex("search_fg", &raw.search_fg)?,
            current_hit_bg: parse_hex("current_hit_bg", &raw.current_hit_bg)?,
            current_hit_fg: parse_hex("current_hit_fg", &raw.current_hit_fg)?,
            hint_bg: parse_hex("hint_bg", &raw.hint_bg)?,
            hint_fg: parse_hex("hint_fg", &raw.hint_fg)?,
            bar_bg: parse_hex("bar_bg", &raw.bar_bg)?,
            bar_fg: parse_hex("bar_fg", &raw.bar_fg)?,
            mode_bg: parse_hex("mode_bg", &raw.mode_bg)?,
            mode_fg: parse_hex("mode_fg", &raw.mode_fg)?,
            muted: parse_hex("muted", &raw.muted)?,
            picker_bg: parse_hex("picker_bg", &raw.picker_bg)?,
            picker_selected_bg: parse_hex("picker_selected_bg", &raw.picker_selected_bg)?,
        })
    }
}

impl Theme {
    fn from_json(text: &str) -> Result<Self> {
        let raw: RawTheme = serde_json::from_str(text).context("malformed theme file")?;
        Ok(Self {
            name: raw.name,
            palette: Palette::from_raw(raw.palette)?,
        })
    }

    /// Catppuccin Mocha. Its own test pins that this always parses, so a
    /// broken bundled file fails the build rather than the field.
    pub fn bundled() -> Self {
        Self::from_json(BUNDLED_JSON).expect("the bundled theme must parse")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading theme file {}", path.display()))?;
        Self::from_json(&text)
    }

    /// `$CORRAL_THEME` if it names a theme that loads, else the user's
    /// config file if one exists and loads, else the bundled theme. A
    /// theme that fails to load falls back rather than refusing to start:
    /// a typo in a color should not keep the terminal from opening.
    pub fn resolve() -> Self {
        if let Ok(path) = std::env::var("CORRAL_THEME") {
            match Self::load(Path::new(&path)) {
                Ok(theme) => return theme,
                Err(e) => eprintln!("corral: ignoring $CORRAL_THEME={path:?}: {e:#}"),
            }
        } else if let Some(path) = user_config_path()
            && path.exists()
        {
            match Self::load(&path) {
                Ok(theme) => return theme,
                Err(e) => eprintln!("corral: ignoring {}: {e:#}", path.display()),
            }
        }
        Self::bundled()
    }
}

/// `~/.config/corral/theme.json`. No `XDG_CONFIG_HOME` override and no new
/// dependency: one fixed path is enough until config grows a second file.
fn user_config_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config/corral/theme.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // CORRAL_THEME and HOME are process-global; resolve() tests share one
    // lock so they cannot interleave their env mutations.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn the_bundled_theme_carries_the_documented_catppuccin_mocha_values() {
        let theme = Theme::bundled();
        assert_eq!(theme.name, "Catppuccin Mocha");
        assert_eq!(theme.palette.gutter, Color::Rgb(0x45, 0x47, 0x5a));
        assert_eq!(theme.palette.focused_gutter, Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(theme.palette.search_bg, Color::Rgb(0xf9, 0xe2, 0xaf));
        assert_eq!(theme.palette.current_hit_bg, Color::Rgb(0xcb, 0xa6, 0xf7));
        assert_eq!(theme.palette.bar_bg, Color::Rgb(0x18, 0x18, 0x25));
        assert_eq!(theme.palette.bar_fg, Color::Rgb(0xa6, 0xad, 0xc8));
        assert_eq!(theme.palette.mode_bg, Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(theme.palette.mode_fg, Color::Rgb(0x11, 0x11, 0x1b));
        assert_eq!(theme.palette.muted, Color::Rgb(0x7f, 0x84, 0x9c));
        assert_eq!(theme.palette.picker_bg, Color::Rgb(0x18, 0x18, 0x25));
        assert_eq!(
            theme.palette.picker_selected_bg,
            Color::Rgb(0x31, 0x32, 0x44)
        );
    }

    #[test]
    fn a_theme_written_before_the_chrome_fields_still_loads_with_defaults() {
        let legacy = r##"{
            "name": "Legacy",
            "palette": {
                "gutter": "#45475a", "focused_gutter": "#89b4fa",
                "cursor_bg": "#7f849c", "cursor_fg": "#11111b",
                "search_bg": "#f9e2af", "search_fg": "#11111b",
                "current_hit_bg": "#cba6f7", "current_hit_fg": "#11111b",
                "hint_bg": "#585b70", "hint_fg": "#cdd6f4"
            }
        }"##;
        let theme = Theme::from_json(legacy).expect("legacy theme must parse");
        assert_eq!(theme.name, "Legacy");
        assert_eq!(theme.palette.bar_bg, Color::Rgb(0x18, 0x18, 0x25));
        assert_eq!(
            theme.palette.picker_selected_bg,
            Color::Rgb(0x31, 0x32, 0x44)
        );
        // Explicit values still win over the defaults.
        let mut json: serde_json::Value = serde_json::from_str(BUNDLED_JSON).unwrap();
        json["palette"]["bar_bg"] = "#ff0000".into();
        let themed = Theme::from_json(&serde_json::to_string(&json).unwrap()).unwrap();
        assert_eq!(themed.palette.bar_bg, Color::Rgb(0xff, 0, 0));
    }

    #[test]
    fn a_second_theme_file_parses_and_changes_a_palette_entry() {
        let mut json: serde_json::Value = serde_json::from_str(BUNDLED_JSON).unwrap();
        json["name"] = "Custom".into();
        json["palette"]["gutter"] = "#ff0000".into();
        let dir = std::env::temp_dir().join(format!("corral-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("custom.json");
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

        let theme = Theme::load(&path).unwrap();
        assert_eq!(theme.name, "Custom");
        assert_eq!(theme.palette.gutter, Color::Rgb(0xff, 0, 0));
        // Untouched fields still come through.
        assert_eq!(theme.palette.search_bg, Color::Rgb(0xf9, 0xe2, 0xaf));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_hash_is_rejected() {
        assert!(parse_hex("gutter", "45475a").is_err());
    }

    #[test]
    fn a_wrong_length_is_rejected() {
        assert!(parse_hex("gutter", "#45475").is_err());
        assert!(parse_hex("gutter", "#45475a00").is_err());
    }

    #[test]
    fn non_hex_digits_are_rejected() {
        assert!(parse_hex("gutter", "#zzzzzz").is_err());
    }

    #[test]
    fn a_missing_field_fails_to_parse_the_whole_theme() {
        let broken = r##"{"name": "Broken", "palette": {"gutter": "#45475a"}}"##;
        assert!(Theme::from_json(broken).is_err());
    }

    #[test]
    fn loading_a_nonexistent_file_is_an_error_not_a_fallback() {
        assert!(Theme::load(Path::new("/no/such/theme.json")).is_err());
    }

    #[test]
    fn resolve_prefers_the_env_override_over_the_user_config_and_bundled() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("corral-theme-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".config/corral")).unwrap();
        let mut config_json: serde_json::Value = serde_json::from_str(BUNDLED_JSON).unwrap();
        config_json["name"] = "From config".into();
        std::fs::write(
            home.join(".config/corral/theme.json"),
            serde_json::to_string(&config_json).unwrap(),
        )
        .unwrap();

        let env_path = dir.join("env-theme.json");
        let mut env_json: serde_json::Value = serde_json::from_str(BUNDLED_JSON).unwrap();
        env_json["name"] = "From env".into();
        std::fs::write(&env_path, serde_json::to_string(&env_json).unwrap()).unwrap();

        // SAFETY: guarded by ENV_LOCK; no other test reads these vars.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("CORRAL_THEME", &env_path);
        }
        assert_eq!(Theme::resolve().name, "From env");

        // With no env override, the user config wins over bundled.
        unsafe {
            std::env::remove_var("CORRAL_THEME");
        }
        assert_eq!(Theme::resolve().name, "From config");

        // With neither, the bundled theme answers.
        std::fs::remove_file(home.join(".config/corral/theme.json")).unwrap();
        assert_eq!(Theme::resolve().name, "Catppuccin Mocha");

        unsafe {
            std::env::remove_var("HOME");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
