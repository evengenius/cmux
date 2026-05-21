use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub layout: LayoutConfig,
    #[serde(default)]
    pub browser: BrowserConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub detect: DetectConfig,
    #[serde(default)]
    pub notify: NotifyConfig,
    #[serde(default)]
    pub theme: ThemeConfig,
    #[serde(default)]
    pub keys: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub git: GitConfig,
    /// User-defined text snippets surfaced in the F9 palette as
    /// `★ Insert snippet: <name>`. The selected snippet's text is sent to
    /// the active chat's input (no trailing `\r`, so the user can edit
    /// before submitting).
    #[serde(default)]
    pub snippets: std::collections::BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GitConfig {
    /// When true, new chats spawned inside a git repo land in a fresh
    /// worktree on a new branch instead of the repo root. Falls back to the
    /// plain cwd if cwd isn't in a git repo or worktree creation fails.
    #[serde(default)]
    pub auto_worktree: bool,
    /// Directory (relative to the repo root, or absolute) where cmux
    /// creates worktrees.
    #[serde(default = "default_worktree_root")]
    pub worktree_root: String,
    /// Prefix for the branch name on each worktree.
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,
    /// When true, `git worktree remove --force` runs after the chat
    /// closes (so the working tree doesn't pile up). Pinned chats are
    /// exempt — pin protects the worktree too.
    #[serde(default = "default_true")]
    pub remove_on_close: bool,
}

fn default_worktree_root() -> String {
    ".cmux-worktrees".to_string()
}
fn default_branch_prefix() -> String {
    "cmux/".to_string()
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            auto_worktree: false,
            worktree_root: default_worktree_root(),
            branch_prefix: default_branch_prefix(),
            remove_on_close: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ThemeConfig {
    /// Accent colour name used for: active-project highlight, palette border,
    /// search-counter background. One of: cyan, yellow, green, magenta, red,
    /// blue, white, gray, lightblue, lightgreen, lightmagenta, lightred,
    /// lightyellow, lightcyan. Unknown values fall back to cyan.
    #[serde(default = "default_accent")]
    pub accent: String,
    /// Palette mode: `dark` (default), `light`, or `auto` — `auto`
    /// inspects `COLORFGBG` to guess the host terminal's background.
    /// Light mode swaps every chrome / panel / input background so the
    /// TUI is readable against a white-on-light terminal.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// When true, icons that rely on emoji (`📌`, `🔍`) fall back to
    /// ASCII alternatives (`[*]`, `?`). Useful for terminals without
    /// emoji-aware fonts, SSH sessions on bare ttys, or LANG=C.
    #[serde(default)]
    pub ascii_icons: bool,
}

fn default_accent() -> String {
    "cyan".to_string()
}

fn default_mode() -> String {
    "dark".to_string()
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            accent: default_accent(),
            mode: default_mode(),
            ascii_icons: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct NotifyConfig {
    /// Emit a terminal BEL (`\x07`) when a tab transitions into the
    /// AwaitingPermission state. Most terminals turn this into a flash
    /// and/or a sound depending on user config.
    #[serde(default = "default_true")]
    pub bell: bool,
    /// Fire a Windows toast (via background PowerShell, no extra deps)
    /// when a tab transitions into AwaitingPermission. Windows-only;
    /// silently ignored on other OSes.
    #[serde(default = "default_true")]
    pub toast: bool,
    /// Emit OSC 9 (iTerm2 / Windows Terminal) and OSC 777 (Konsole)
    /// notification escape sequences. Off by default because terminals
    /// that don't recognise them still don't display anything useful;
    /// users on iTerm2 / Konsole / KDE get free Notification Center
    /// integration by flipping this on.
    #[serde(default)]
    pub osc: bool,
    /// HTTP webhook URL to POST a JSON body to on every AwaitingPermission
    /// transition. Empty = disabled. Useful for Slack incoming webhooks,
    /// Discord webhooks, ntfy.sh, custom dispatch servers. Fires
    /// fire-and-forget via `curl`; if curl isn't on PATH this silently
    /// no-ops.
    #[serde(default)]
    pub webhook: String,
}

fn default_true() -> bool {
    true
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            bell: true,
            toast: true,
            osc: false,
            webhook: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct DetectConfig {
    /// Substrings (lowercase) that mark the embedded claude as awaiting user
    /// input on a permission prompt. Any match flips the tab into the
    /// AwaitingPermission state (red blinking `!`).
    #[serde(default = "default_permission_patterns")]
    pub permission_patterns: Vec<String>,
}

fn default_permission_patterns() -> Vec<String> {
    vec![
        "do you want to".to_string(),
        "allow this tool".to_string(),
        "approve this".to_string(),
        "(y/n)".to_string(),
    ]
}

impl Default for DetectConfig {
    fn default() -> Self {
        Self {
            permission_patterns: default_permission_patterns(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LayoutConfig {
    pub sidebar_width: u16,
    #[serde(default = "default_right_sidebar_width")]
    pub right_sidebar_width: u16,
    pub bottom_height: u16,
    /// When true, the saved layout in `~/.cmux/layout.json` is restored
    /// automatically on startup. Default off — opt-in so the first launch
    /// behaviour stays predictable.
    #[serde(default)]
    pub auto_restore: bool,
    /// Scrollback line cap for the PTY parser AND the plain-text mirror
    /// used by Ctrl+F. Bigger = more history at the cost of RAM (≈100B per
    /// line × cap × open tabs). Only applies to tabs spawned after reload.
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: usize,
}

fn default_scrollback_lines() -> usize {
    10_000
}

fn default_right_sidebar_width() -> u16 {
    42
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            sidebar_width: 42,
            right_sidebar_width: 42,
            bottom_height: 12,
            auto_restore: false,
            scrollback_lines: 10_000,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct BrowserConfig {
    #[serde(default)]
    pub show_hidden: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ShellConfig {
    #[serde(default)]
    pub exe: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// When true, the bottom shell `cd`s to the active tab's cwd on tab switch.
    #[serde(default = "default_follow_tab_cwd")]
    pub follow_tab_cwd: bool,
}

fn default_follow_tab_cwd() -> bool {
    true
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            exe: String::new(),
            args: Vec::new(),
            follow_tab_cwd: true,
        }
    }
}

impl ShellConfig {
    pub fn override_pair(&self) -> Option<(String, Vec<String>)> {
        if self.exe.trim().is_empty() {
            None
        } else {
            Some((self.exe.clone(), self.args.clone()))
        }
    }
}

pub fn config_path() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".cmux").join("config.toml")
}

pub fn load() -> Config {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

const DEFAULT_TOML: &str = r#"# cmux configuration
# Edit and save, then F9 → "★ Reload config" to apply.
# Some fields apply immediately, others only to newly-spawned tabs / shells.

[layout]
# Width in columns of the left sidebar (Sessions/Commands)
sidebar_width = 42
# Width in columns of the right sidebar (Files)
right_sidebar_width = 42
# Height in rows of the bottom shell pane
bottom_height = 12
# When true, ~/.cmux/layout.json is restored automatically on startup
# (otherwise use F9 → "★ Restore previous layout" once per session).
auto_restore = false
# PTY scrollback line cap. ~100B per line × cap × open tabs is the
# RAM budget. Applies to tabs spawned after reload.
scrollback_lines = 10000

[browser]
# Show dot-files (.git, .claude, etc.) in the file browser / files sidebar
show_hidden = false

[shell]
# Override the bottom-pane shell. Empty = autodetect from parent process.
# Example:
#   exe = "pwsh.exe"
#   args = ["-NoLogo"]
exe = ""
args = []
# When true, the bottom shell `cd`s to the active tab's cwd on tab switch
# (only when the shell isn't focused, so it can't corrupt your typing).
follow_tab_cwd = true

[detect]
# Substrings (lowercase) that mark a tab as "awaiting permission" so it
# blinks red in the tab bar and (if enabled) fires a desktop notification.
# Add patterns here if claude's UI uses different wording in your locale.
permission_patterns = [
  "do you want to",
  "allow this tool",
  "approve this",
  "(y/n)",
]

[notify]
# Terminal BEL (\x07) on a tab transitioning into AwaitingPermission.
# Most terminals translate this into a flash/sound per user config.
bell = true
# Windows toast notification on the same transition. Uses a background
# PowerShell + WinRT call — no extra dependencies. Silently no-op on
# non-Windows.
toast = true
# OSC 9 / OSC 777 escape sequences — iTerm2 and Konsole surface these as
# native system notifications. Off by default because most terminals
# ignore them and a few render the escape as visible garbage.
osc = false
# HTTP webhook URL. POST'd a JSON body
#   { "title": "...", "body": "...", "tab": "...", "cwd": "..." }
# on every AwaitingPermission transition. Empty = disabled. Uses curl —
# silently no-op if curl is absent.
webhook = ""

[theme]
# Accent colour. Used for: active-project highlight on the project bar,
# the palette / global-sessions modal border, etc. One of: cyan, yellow,
# green, magenta, red, blue, white, gray, lightblue, lightgreen,
# lightmagenta, lightred, lightyellow, lightcyan.
accent = "cyan"
# Palette mode. `dark` is the historic default; `light` swaps every
# panel / input / chrome background so the TUI stays readable on
# terminals with a white-on-light theme; `auto` guesses from the
# COLORFGBG env var (set by xterm, rxvt, urxvt, some emulators).
mode = "dark"
# When true, replace emoji icons (📌, 🔍) with ASCII fallbacks ([*], ?).
# Switch this on for SSH sessions on bare ttys or LANG=C environments
# where emoji either render as tofu or as wide glyphs that smear layout.
ascii_icons = false

[git]
# When true, F2 / F6-OpenHere spawn the new chat into a fresh git worktree
# branched off HEAD. Falls back silently if cwd isn't in a git repo.
auto_worktree = false
# Where worktrees live, relative to the repo root (or absolute).
worktree_root = ".cmux-worktrees"
# Branch name prefix. Slug is derived from the chat's project basename.
branch_prefix = "cmux/"
# Run `git worktree remove --force` when the chat closes. Pinned chats
# are exempt — pin protects the worktree from cleanup.
remove_on_close = true

[keys]
# Remap actions to custom keys. Format: action_name = key_combo.
# Action names are snake_case versions of the internal Action variants
# (e.g. toggle_search, toggle_palette, new_tab, close_tab, quit, etc.).
# Key combos: f1..f12, single chars, ctrl-x, alt-shift-f4, etc. Unknown
# entries are silently ignored. Default bindings stay in place unless
# you explicitly override.
# Example:
#   toggle_search = "f4"        # F4 opens search instead of F4=commands
#   quit = "ctrl-x"             # Ctrl+X quits (in addition to F10/Ctrl+Q)

[snippets]
# User-defined text snippets — appear in the F9 palette as
# "★ Insert snippet: <name>" and write into the active chat's input box
# (no trailing newline, so you can review/edit before sending).
# Example:
#   review_carefully = "Review this carefully and list issues by severity."
#   commit = "Commit changes with a clear conventional commit message."
"#;

/// Returns `true` if a fresh default config was written on this call —
/// i.e., it's a first run. Used to auto-pop F1 the first time the user
/// launches cmux. On write failure (permissions / disk full) prints a
/// warning to stderr so the user knows config persistence won't work,
/// and returns false (no welcome triggered).
pub fn ensure_default_written() -> bool {
    let path = config_path();
    if path.exists() {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, DEFAULT_TOML) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "cmux: warning — couldn't write default config to {}: {}\n      \
                 (running with built-in defaults; settings won't persist)",
                path.display(),
                e
            );
            false
        }
    }
}
