//! Colours and border styling, modelled after exabind's `Theme` + Catppuccin palette.
//!
//! Every widget asks the theme for a `Style` instead of naming colours itself, so the
//! whole UI stays consistent and switching flavour is one call.
//!
//! The flavour is a global setting (it themes the entire UI), persisted in
//! `~/.config/toolshop/config.toml` and cycled with `t`.

use std::{
    path::Path,
    sync::atomic::{AtomicU8, Ordering},
};

use color_eyre::{Result, eyre::WrapErr};
use ratatui::{
    style::{Color, Modifier, Style},
    widgets::BorderType,
};
use serde::{Deserialize, Serialize};

/// Blend `a` towards `b`; used to derive dim/accent shades from the palette.
fn lerp(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
            Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
        }
        _ => b,
    }
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb(
        (hex >> 16) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

/// Catppuccin flavours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flavour {
    Latte,
    Frappe,
    Macchiato,
    #[default]
    Mocha,
}

impl Flavour {
    /// Cycle order used by the `t` key.
    pub const ALL: [Flavour; 4] = [
        Flavour::Latte,
        Flavour::Frappe,
        Flavour::Macchiato,
        Flavour::Mocha,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Flavour::Latte => "Latte",
            Flavour::Frappe => "Frappé",
            Flavour::Macchiato => "Macchiato",
            Flavour::Mocha => "Mocha",
        }
    }

    /// Key used in the config file.
    pub fn key(self) -> &'static str {
        match self {
            Flavour::Latte => "latte",
            Flavour::Frappe => "frappe",
            Flavour::Macchiato => "macchiato",
            Flavour::Mocha => "mocha",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        let key = key.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|flavour| flavour.key() == key || flavour.name().to_ascii_lowercase() == key)
    }

    pub fn next(self) -> Self {
        let index = Flavour::ALL
            .iter()
            .position(|flavour| *flavour == self)
            .unwrap_or(Flavour::ALL.len() - 1);
        Flavour::ALL[(index + 1) % Flavour::ALL.len()]
    }

    fn index(self) -> u8 {
        Flavour::ALL
            .iter()
            .position(|flavour| *flavour == self)
            .unwrap_or(0) as u8
    }

    fn from_index(index: u8) -> Self {
        Flavour::ALL
            .get(index as usize)
            .copied()
            .unwrap_or_default()
    }
}

/// One Catppuccin flavour: every colour the UI uses, plus its accent cycle.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub rosewater: Color,
    pub flamingo: Color,
    pub pink: Color,
    pub mauve: Color,
    pub red: Color,
    pub maroon: Color,
    pub peach: Color,
    pub yellow: Color,
    pub green: Color,
    pub teal: Color,
    pub sky: Color,
    pub sapphire: Color,
    pub blue: Color,
    pub lavender: Color,
    pub text: Color,
    pub subtext1: Color,
    pub subtext0: Color,
    pub overlay2: Color,
    pub overlay1: Color,
    pub overlay0: Color,
    pub surface2: Color,
    pub surface1: Color,
    pub surface0: Color,
    pub base: Color,
    pub mantle: Color,
    pub crust: Color,
    /// Accent colours cycled over headings, like exabind's shortcut categories.
    pub accents: [Color; 10],
}

impl Palette {
    /// Catppuccin Latte.
    pub const LATTE: Self = Self {
        rosewater: rgb(0xdc8a78),
        flamingo: rgb(0xdd7878),
        pink: rgb(0xea76cb),
        mauve: rgb(0x8839ef),
        red: rgb(0xd20f39),
        maroon: rgb(0xe64553),
        peach: rgb(0xfe640b),
        yellow: rgb(0xdf8e1d),
        green: rgb(0x40a02b),
        teal: rgb(0x179299),
        sky: rgb(0x04a5e5),
        sapphire: rgb(0x209fb5),
        blue: rgb(0x1e66f5),
        lavender: rgb(0x7287fd),
        text: rgb(0x4c4f69),
        subtext1: rgb(0x5c5f77),
        subtext0: rgb(0x6c6f85),
        overlay2: rgb(0x7c7f93),
        overlay1: rgb(0x8c8fa1),
        overlay0: rgb(0x9ca0b0),
        surface2: rgb(0xacb0be),
        surface1: rgb(0xbcc0cc),
        surface0: rgb(0xccd0da),
        base: rgb(0xeff1f5),
        mantle: rgb(0xe6e9ef),
        crust: rgb(0xdce0e8),
        accents: [
            rgb(0x8839ef),
            rgb(0x1e66f5),
            rgb(0x179299),
            rgb(0xdf8e1d),
            rgb(0xea76cb),
            rgb(0x40a02b),
            rgb(0xfe640b),
            rgb(0x209fb5),
            rgb(0xdd7878),
            rgb(0x7287fd),
        ],
    };

    /// Catppuccin Frappe.
    pub const FRAPPE: Self = Self {
        rosewater: rgb(0xf2d5cf),
        flamingo: rgb(0xeebebe),
        pink: rgb(0xf4b8e4),
        mauve: rgb(0xca9ee6),
        red: rgb(0xe78284),
        maroon: rgb(0xea999c),
        peach: rgb(0xef9f76),
        yellow: rgb(0xe5c890),
        green: rgb(0xa6d189),
        teal: rgb(0x81c8be),
        sky: rgb(0x99d1db),
        sapphire: rgb(0x85c1dc),
        blue: rgb(0x8caaee),
        lavender: rgb(0xbabbf1),
        text: rgb(0xc6d0f5),
        subtext1: rgb(0xb5bfe2),
        subtext0: rgb(0xa5adce),
        overlay2: rgb(0x949cbb),
        overlay1: rgb(0x838ba7),
        overlay0: rgb(0x737994),
        surface2: rgb(0x626880),
        surface1: rgb(0x51576d),
        surface0: rgb(0x414559),
        base: rgb(0x303446),
        mantle: rgb(0x292c3c),
        crust: rgb(0x232634),
        accents: [
            rgb(0xca9ee6),
            rgb(0x8caaee),
            rgb(0x81c8be),
            rgb(0xe5c890),
            rgb(0xf4b8e4),
            rgb(0xa6d189),
            rgb(0xef9f76),
            rgb(0x85c1dc),
            rgb(0xeebebe),
            rgb(0xbabbf1),
        ],
    };

    /// Catppuccin Macchiato.
    pub const MACCHIATO: Self = Self {
        rosewater: rgb(0xf4dbd6),
        flamingo: rgb(0xf0c6c6),
        pink: rgb(0xf5bde6),
        mauve: rgb(0xc6a0f6),
        red: rgb(0xed8796),
        maroon: rgb(0xee99a0),
        peach: rgb(0xf5a97f),
        yellow: rgb(0xeed49f),
        green: rgb(0xa6da95),
        teal: rgb(0x8bd5ca),
        sky: rgb(0x91d7e3),
        sapphire: rgb(0x7dc4e4),
        blue: rgb(0x8aadf4),
        lavender: rgb(0xb7bdf8),
        text: rgb(0xcad3f5),
        subtext1: rgb(0xb8c0e0),
        subtext0: rgb(0xa5adcb),
        overlay2: rgb(0x939ab7),
        overlay1: rgb(0x8087a2),
        overlay0: rgb(0x6e738d),
        surface2: rgb(0x5b6078),
        surface1: rgb(0x494d64),
        surface0: rgb(0x363a4f),
        base: rgb(0x24273a),
        mantle: rgb(0x1e2030),
        crust: rgb(0x181926),
        accents: [
            rgb(0xc6a0f6),
            rgb(0x8aadf4),
            rgb(0x8bd5ca),
            rgb(0xeed49f),
            rgb(0xf5bde6),
            rgb(0xa6da95),
            rgb(0xf5a97f),
            rgb(0x7dc4e4),
            rgb(0xf0c6c6),
            rgb(0xb7bdf8),
        ],
    };

    /// Catppuccin Mocha.
    pub const MOCHA: Self = Self {
        rosewater: rgb(0xf5e0dc),
        flamingo: rgb(0xf2cdcd),
        pink: rgb(0xf5c2e7),
        mauve: rgb(0xcba6f7),
        red: rgb(0xf38ba8),
        maroon: rgb(0xeba0ac),
        peach: rgb(0xfab387),
        yellow: rgb(0xf9e2af),
        green: rgb(0xa6e3a1),
        teal: rgb(0x94e2d5),
        sky: rgb(0x89dceb),
        sapphire: rgb(0x74c7ec),
        blue: rgb(0x89b4fa),
        lavender: rgb(0xb4befe),
        text: rgb(0xcdd6f4),
        subtext1: rgb(0xbac2de),
        subtext0: rgb(0xa6adc8),
        overlay2: rgb(0x9399b2),
        overlay1: rgb(0x7f849c),
        overlay0: rgb(0x6c7086),
        surface2: rgb(0x585b70),
        surface1: rgb(0x45475a),
        surface0: rgb(0x313244),
        base: rgb(0x1e1e2e),
        mantle: rgb(0x181825),
        crust: rgb(0x11111b),
        accents: [
            rgb(0xcba6f7),
            rgb(0x89b4fa),
            rgb(0x94e2d5),
            rgb(0xf9e2af),
            rgb(0xf5c2e7),
            rgb(0xa6e3a1),
            rgb(0xfab387),
            rgb(0x74c7ec),
            rgb(0xf2cdcd),
            rgb(0xb4befe),
        ],
    };

    /// Palette for a flavour.
    pub const fn of(flavour: Flavour) -> Self {
        match flavour {
            Flavour::Latte => Self::LATTE,
            Flavour::Frappe => Self::FRAPPE,
            Flavour::Macchiato => Self::MACCHIATO,
            Flavour::Mocha => Self::MOCHA,
        }
    }
}

/// The palette for `flavour`.
pub fn palette(flavour: Flavour) -> &'static Palette {
    match flavour {
        Flavour::Latte => &Palette::LATTE,
        Flavour::Frappe => &Palette::FRAPPE,
        Flavour::Macchiato => &Palette::MACCHIATO,
        Flavour::Mocha => &Palette::MOCHA,
    }
}

/// Flavour currently in use.
static CURRENT: AtomicU8 = AtomicU8::new(3); // Mocha

pub fn flavour() -> Flavour {
    Flavour::from_index(CURRENT.load(Ordering::Relaxed))
}

pub fn set_flavour(flavour: Flavour) {
    CURRENT.store(flavour.index(), Ordering::Relaxed);
}

/// Advance to the next flavour and return it.
pub fn cycle() -> Flavour {
    let next = flavour().next();
    set_flavour(next);
    next
}

/// Styles for every UI slot; each method reads the active flavour.
#[derive(Debug, Clone, Copy)]
pub struct Theme;

pub const THEME: Theme = Theme;

impl Theme {
    fn p(&self) -> &'static Palette {
        palette(flavour())
    }

    /// Border style for a pane: accent when focused, muted otherwise.
    pub fn pane_border(&self, focused: bool) -> Style {
        if focused {
            Style::new().fg(self.p().mauve)
        } else {
            Style::new().fg(self.p().surface2)
        }
    }

    /// Border glyphs: a heavier frame marks the active pane.
    pub fn pane_border_type(&self, focused: bool) -> BorderType {
        if focused {
            BorderType::Thick
        } else {
            BorderType::Rounded
        }
    }

    /// Pane title: accent and bold when focused, quiet when not.
    pub fn pane_title(&self, focused: bool) -> Style {
        if focused {
            Style::new()
                .fg(self.p().lavender)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(self.p().overlay1)
        }
    }

    /// Heading rows (distro families, package categories).
    pub fn heading(&self, accent_index: usize) -> Style {
        Style::new()
            .fg(self.accent(accent_index))
            .add_modifier(Modifier::BOLD)
    }

    /// Accent for a heading index, cycled over the flavour's accent colours.
    pub fn accent(&self, index: usize) -> Color {
        let accents = self.p().accents;
        accents[index % accents.len()]
    }

    /// A checked/selected item in a list.
    pub fn selected(&self) -> Style {
        Style::new().fg(self.p().green)
    }

    /// The row the cursor is on.
    pub fn cursor(&self) -> Style {
        Style::new()
            .bg(self.p().surface0)
            .fg(self.p().text)
            .add_modifier(Modifier::BOLD)
    }

    pub fn text(&self) -> Style {
        Style::new().fg(self.p().text)
    }

    pub fn muted(&self) -> Style {
        Style::new().fg(self.p().overlay0)
    }

    pub fn label(&self) -> Style {
        Style::new().fg(self.p().subtext0)
    }

    pub fn info(&self) -> Style {
        Style::new().fg(self.p().sky)
    }

    pub fn success(&self) -> Style {
        Style::new().fg(self.p().green)
    }

    pub fn warning(&self) -> Style {
        Style::new().fg(self.p().yellow)
    }

    pub fn error(&self) -> Style {
        Style::new().fg(self.p().red)
    }

    /// Active tab in the tab bar.
    pub fn tab_active(&self) -> Style {
        Style::new()
            .fg(self.p().mauve)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }

    pub fn tab_inactive(&self) -> Style {
        Style::new().fg(self.p().overlay1)
    }

    /// Border of a window that owns the keyboard (popups, prompts).
    pub fn window_border(&self) -> Style {
        let p = self.p();
        Style::new().fg(lerp(p.crust, p.mauve, 0.85))
    }

    pub fn window_title(&self) -> Style {
        Style::new().fg(self.p().mauve).add_modifier(Modifier::BOLD)
    }

    /// A panel surface slightly darker (or lighter, on Latte) than the background.
    pub fn surface(&self) -> Style {
        Style::new().bg(self.p().mantle)
    }

    /// Filled part of a progress gauge.
    pub fn gauge(&self) -> Style {
        Style::new().fg(self.p().mauve).bg(self.p().surface0)
    }

    /// Text drawn on top of a gauge.
    pub fn gauge_label(&self) -> Style {
        Style::new().fg(self.p().text).add_modifier(Modifier::BOLD)
    }

    /// Containerfile line installing the package under the cursor.
    pub fn container_primary(&self) -> Style {
        Style::new()
            .fg(self.p().mauve)
            .bg(self.p().surface0)
            .add_modifier(Modifier::BOLD)
    }

    /// Containerfile line installing one of its dependencies.
    pub fn container_dependency(&self) -> Style {
        Style::new().fg(self.p().peach)
    }

    // --- log lines ---------------------------------------------------------

    /// A failure banner ("Build failed …"): red on its own band so it cannot be missed.
    pub fn log_banner(&self) -> Style {
        Style::new()
            .fg(self.p().red)
            .bg(self.p().surface0)
            .add_modifier(Modifier::BOLD)
    }

    /// Any other error line from the build output.
    pub fn log_error(&self) -> Style {
        Style::new().fg(self.p().red).add_modifier(Modifier::BOLD)
    }

    pub fn log_warning(&self) -> Style {
        Style::new().fg(self.p().yellow)
    }

    pub fn log_success(&self) -> Style {
        Style::new().fg(self.p().green)
    }

    /// `STEP n/m:` and other progress lines.
    pub fn log_step(&self) -> Style {
        Style::new().fg(self.p().sapphire)
    }

    pub fn status_running(&self) -> Style {
        Style::new()
            .fg(self.p().yellow)
            .add_modifier(Modifier::BOLD)
    }

    pub fn status_idle(&self) -> Style {
        Style::new().fg(self.p().overlay0)
    }
}

/// Serialises tests that need a specific flavour active while they assert.
#[cfg(test)]
static FLAVOUR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds the flavour lock with `flavour` active, restoring it when dropped.
///
/// The flavour is process-wide, so tests that assert on colours (or on the flavour name
/// in the UI) hold this for the duration of the test.
#[cfg(test)]
pub struct FlavourGuard {
    previous: Flavour,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl FlavourGuard {
    pub fn new(flavour: Flavour) -> Self {
        let lock = FLAVOUR_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = super::theme::flavour();
        set_flavour(flavour);
        Self {
            previous,
            _lock: lock,
        }
    }
}

#[cfg(test)]
impl Drop for FlavourGuard {
    fn drop(&mut self) {
        set_flavour(self.previous);
    }
}

/// Run `f` with `flavour` active, restoring the previous one afterwards.
#[cfg(test)]
pub fn with_flavour<T>(flavour: Flavour, f: impl FnOnce() -> T) -> T {
    let _guard = FlavourGuard::new(flavour);
    f()
}

/// `[theme]` section of the config file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    theme: Option<String>,
}

/// File the flavour preference is stored in.
pub fn config_file() -> std::path::PathBuf {
    crate::distro::config_dir().join("config.toml")
}

/// Read the saved flavour (if any) and apply it.
pub fn load() -> Vec<String> {
    load_from(&config_file())
}

pub fn load_from(path: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return warnings,
        Err(err) => {
            warnings.push(format!("cannot read {}: {err}", path.display()));
            return warnings;
        }
    };

    match toml::from_str::<ConfigFile>(&contents) {
        Ok(config) => match config.theme.as_deref().map(Flavour::from_key) {
            Some(Some(flavour)) => set_flavour(flavour),
            Some(None) => warnings.push(format!(
                "{}: unknown theme {:?}",
                path.display(),
                config.theme.unwrap_or_default()
            )),
            None => {}
        },
        Err(err) => warnings.push(format!("cannot parse {}: {err}", path.display())),
    }

    warnings
}

/// Persist the flavour.
pub fn save(flavour: Flavour) -> Result<()> {
    save_to(&config_file(), flavour)
}

pub fn save_to(path: &Path, flavour: Flavour) -> Result<()> {
    let mut config = match std::fs::read_to_string(path) {
        Ok(contents) => toml::from_str::<ConfigFile>(&contents).unwrap_or_default(),
        Err(_) => ConfigFile::default(),
    };
    config.theme = Some(flavour.key().to_string());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("cannot create {}", parent.display()))?;
    }
    let contents = toml::to_string_pretty(&config).wrap_err("cannot serialise config")?;
    std::fs::write(path, contents).wrap_err_with(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavours_have_distinct_backgrounds() {
        let latte = palette(Flavour::Latte);
        let mocha = palette(Flavour::Mocha);
        assert_eq!(latte.base, rgb(0xeff1f5), "Latte is light");
        assert_eq!(mocha.base, rgb(0x1e1e2e), "Mocha is dark");
        assert_ne!(latte.base, mocha.base);
        assert_eq!(mocha.mauve, rgb(0xcba6f7));
        assert_eq!(palette(Flavour::Macchiato).mauve, rgb(0xc6a0f6));
        assert_eq!(palette(Flavour::Frappe).mauve, rgb(0xca9ee6));
    }

    #[test]
    fn every_flavour_defines_a_full_accent_cycle() {
        for flavour in Flavour::ALL {
            let palette = palette(flavour);
            assert_eq!(palette.accents.len(), 10);
            assert!(
                palette
                    .accents
                    .iter()
                    .all(|colour| matches!(colour, Color::Rgb(..))),
                "{flavour:?} has a non-RGB accent"
            );
            assert_ne!(palette.accents[0], palette.accents[1], "{flavour:?}");
        }
    }

    #[test]
    fn flavours_cycle_and_round_trip_through_keys() {
        assert_eq!(Flavour::Latte.next(), Flavour::Frappe);
        assert_eq!(Flavour::Frappe.next(), Flavour::Macchiato);
        assert_eq!(Flavour::Macchiato.next(), Flavour::Mocha);
        assert_eq!(Flavour::Mocha.next(), Flavour::Latte, "wraps around");

        for flavour in Flavour::ALL {
            assert_eq!(Flavour::from_key(flavour.key()), Some(flavour));
            assert_eq!(
                Flavour::from_key(&flavour.name().to_ascii_lowercase()),
                Some(flavour)
            );
        }
        assert_eq!(Flavour::from_key("frappé"), Some(Flavour::Frappe));
        assert_eq!(Flavour::from_key("dracula"), None);
    }

    #[test]
    fn the_theme_follows_the_active_flavour() {
        let previous = flavour();
        set_flavour(Flavour::Latte);
        assert_eq!(THEME.text().fg, Some(palette(Flavour::Latte).text));
        assert_eq!(
            THEME.pane_border(true).fg,
            Some(palette(Flavour::Latte).mauve)
        );

        set_flavour(Flavour::Mocha);
        assert_eq!(THEME.text().fg, Some(palette(Flavour::Mocha).text));
        set_flavour(previous);
    }

    #[test]
    fn focused_panes_use_a_heavier_accented_frame() {
        assert_eq!(THEME.pane_border_type(true), BorderType::Thick);
        assert_eq!(THEME.pane_border_type(false), BorderType::Rounded);
    }

    #[test]
    fn accents_cycle_per_heading() {
        let accents = palette(flavour()).accents;
        assert_eq!(THEME.accent(0), accents[0]);
        assert_eq!(
            THEME.accent(accents.len()),
            accents[0],
            "accents wrap around"
        );
        assert_ne!(THEME.accent(0), THEME.accent(1));
    }

    #[test]
    fn lerp_blends_towards_the_target() {
        let black = Color::Rgb(0, 0, 0);
        let white = Color::Rgb(255, 255, 255);
        assert_eq!(lerp(black, white, 0.0), black);
        assert_eq!(lerp(black, white, 1.0), white);
        assert_eq!(lerp(black, white, 0.5), Color::Rgb(127, 127, 127));
    }

    #[test]
    fn window_borders_are_dimmed_accents() {
        let border = THEME.window_border().fg.expect("colour");
        assert_ne!(
            border,
            palette(flavour()).mauve,
            "blended, not the raw accent"
        );
        assert_ne!(border, palette(flavour()).crust);
    }

    #[test]
    fn the_flavour_persists_in_the_config() {
        let dir = std::env::temp_dir().join(format!("toolshop-theme-{}", std::process::id()));
        let path = dir.join("config.toml");

        save_to(&path, Flavour::Macchiato).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("theme = \"macchiato\""), "{contents}");

        with_flavour(Flavour::Mocha, || {
            assert!(load_from(&path).is_empty());
            assert_eq!(flavour(), Flavour::Macchiato, "loaded from the config");
        });

        // An unknown flavour is reported instead of applied.
        std::fs::write(&path, "theme = \"dracula\"\n").unwrap();
        with_flavour(Flavour::Mocha, || {
            let warnings = load_from(&path);
            assert!(
                warnings.iter().any(|w| w.contains("unknown theme")),
                "{warnings:?}"
            );
            assert_eq!(flavour(), Flavour::Mocha);
        });

        std::fs::write(&path, "theme = 3\n").unwrap();
        let warnings = load_from(&path);
        assert!(
            warnings.iter().any(|w| w.contains("cannot parse")),
            "{warnings:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
