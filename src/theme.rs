//! Centralised TUI palette.
//!
//! `init()` is called once at startup with the user-configured mode
//! (`dark` / `light` / `auto`) and the accent name; `theme()` then returns
//! a copy for every render site. Holding the palette in a single place
//! lets us add presets and light-mode support without threading colours
//! through render-fn signatures across ~7900 lines of main.rs.
//!
//! `reload()` swaps the palette atomically — used by F9 → Reload config.

use std::sync::{Mutex, OnceLock};

use ratatui::style::Color;

static THEME: OnceLock<Mutex<Theme>> = OnceLock::new();

#[derive(Clone, Copy)]
pub struct Theme {
    pub mode: Mode,
    pub accent: Color,
    /// Solid background for the main chrome (project / chat bars).
    pub bg_chrome: Color,
    /// Slightly-elevated background for inactive items in the chrome.
    pub bg_inactive: Color,
    /// Background for modal / sidebar panels.
    pub bg_panel: Color,
    /// Background for input boxes inside modals.
    pub bg_input: Color,
    /// Background for the button bar buttons in the idle state.
    pub bg_button: Color,
    /// Background reserved for diff / destructive surfaces.
    pub bg_danger: Color,
    /// Dimmed foreground (separators, hints).
    pub fg_dim: Color,
    /// Normal foreground.
    pub fg: Color,
    /// Strong foreground (active, headings).
    pub fg_strong: Color,
    /// Error / destructive foreground.
    pub fg_error: Color,
    /// Warning foreground.
    pub fg_warn: Color,
    /// Success foreground.
    pub fg_ok: Color,
    /// Stable border colour for unfocused panels (used by sidebars).
    pub border_inactive: Color,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

impl Theme {
    fn dark(accent: Color) -> Self {
        Self {
            mode: Mode::Dark,
            accent,
            bg_chrome: Color::Rgb(40, 40, 50),
            bg_inactive: Color::Rgb(40, 40, 50),
            bg_panel: Color::Rgb(20, 20, 24),
            bg_input: Color::Rgb(28, 28, 32),
            bg_button: Color::Rgb(80, 80, 80),
            bg_danger: Color::Rgb(30, 20, 20),
            fg_dim: Color::DarkGray,
            fg: Color::Gray,
            fg_strong: Color::White,
            fg_error: Color::Red,
            fg_warn: Color::Yellow,
            fg_ok: Color::Green,
            border_inactive: Color::DarkGray,
        }
    }

    fn light(accent: Color) -> Self {
        Self {
            mode: Mode::Light,
            accent,
            bg_chrome: Color::Rgb(220, 220, 224),
            bg_inactive: Color::Rgb(208, 208, 214),
            bg_panel: Color::Rgb(245, 245, 248),
            bg_input: Color::Rgb(232, 232, 236),
            bg_button: Color::Rgb(200, 200, 204),
            bg_danger: Color::Rgb(252, 224, 224),
            fg_dim: Color::Rgb(120, 120, 124),
            fg: Color::Rgb(40, 40, 44),
            fg_strong: Color::Rgb(16, 16, 20),
            fg_error: Color::Rgb(180, 30, 30),
            fg_warn: Color::Rgb(160, 110, 0),
            fg_ok: Color::Rgb(30, 120, 50),
            border_inactive: Color::Rgb(160, 160, 168),
        }
    }
}

/// Resolve `auto` mode. Looks at `COLORFGBG` (set by xterm / rxvt /
/// some terminal emulators as `fg;bg` where bg is a 16-colour index;
/// 7 and 15 are light backgrounds).
fn detect_mode() -> Mode {
    if let Ok(s) = std::env::var("COLORFGBG") {
        if let Some(bg) = s.split(';').last() {
            if let Ok(bg) = bg.trim().parse::<u32>() {
                if matches!(bg, 7 | 15) {
                    return Mode::Light;
                }
                return Mode::Dark;
            }
        }
    }
    Mode::Dark
}

fn make(mode_str: &str, accent: Color) -> Theme {
    let mode = match mode_str.trim().to_lowercase().as_str() {
        "light" => Mode::Light,
        "dark" => Mode::Dark,
        _ => detect_mode(),
    };
    match mode {
        Mode::Dark => Theme::dark(accent),
        Mode::Light => Theme::light(accent),
    }
}

pub fn init(mode_str: &str, accent: Color) {
    let t = make(mode_str, accent);
    if let Some(slot) = THEME.get() {
        *slot.lock().unwrap() = t;
    } else {
        let _ = THEME.set(Mutex::new(t));
    }
}

pub fn reload(mode_str: &str, accent: Color) {
    init(mode_str, accent);
}

pub fn theme() -> Theme {
    if let Some(slot) = THEME.get() {
        return *slot.lock().unwrap();
    }
    Theme::dark(Color::Cyan)
}
