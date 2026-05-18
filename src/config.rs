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
"#;

pub fn ensure_default_written() {
    let path = config_path();
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, DEFAULT_TOML);
}
