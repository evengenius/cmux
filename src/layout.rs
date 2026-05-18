use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SavedLayout {
    pub version: u32,
    pub saved_at_unix: u64,
    pub active: usize,
    pub tabs: Vec<SavedTab>,
    pub sidebar_open: bool,
    #[serde(default)]
    pub bottom_open: bool,
    /// cwds of projects the user pinned. Project pinning lives outside the
    /// per-tab struct because "project" is derived from tabs' cwds — one
    /// pin flag per unique cwd, not per tab.
    #[serde(default)]
    pub pinned_projects: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SavedTab {
    pub cwd: PathBuf,
    pub session_id: Option<String>,
    pub title: String,
    /// Tab creation time in UNIX seconds (used to resolve session_id later).
    #[serde(default)]
    pub created_at_unix: u64,
    /// Pinned tabs can't be closed via F8/Alt+W. Persisted so a pinned tab
    /// stays pinned across cmux restarts.
    #[serde(default)]
    pub pinned: bool,
}

pub fn layout_path() -> PathBuf {
    cmux_dir().join("layout.json")
}

/// Directory holding named layouts. Files are `<name>.json`. The unnamed
/// auto-saved layout (`layout.json`) sits alongside it.
pub fn layouts_dir() -> PathBuf {
    cmux_dir().join("layouts")
}

fn cmux_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".cmux")
}

pub fn save(layout: &SavedLayout) -> Result<()> {
    let path = layout_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(layout)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn load() -> Option<SavedLayout> {
    let path = layout_path();
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<SavedLayout>(&text).ok()
}

/// Save the layout under a user-chosen name (sanitised). Overwrites if it
/// already exists.
pub fn save_named(name: &str, layout: &SavedLayout) -> Result<()> {
    let path = named_path(name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(layout)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn load_named(name: &str) -> Option<SavedLayout> {
    let path = named_path(name).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<SavedLayout>(&text).ok()
}

pub fn delete_named(name: &str) -> Result<()> {
    let path = named_path(name)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Enumerate named layouts. Returns the bare names (no `.json` extension),
/// sorted alphabetically.
pub fn list_named() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(layouts_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            out.push(stem.to_string());
        }
    }
    out.sort();
    out
}

fn named_path(name: &str) -> Result<PathBuf> {
    let safe = sanitize_name(name);
    if safe.is_empty() {
        anyhow::bail!("empty layout name");
    }
    Ok(layouts_dir().join(format!("{}.json", safe)))
}

/// Reduce a free-form name to something safe to use as a filename across
/// platforms. Drops path separators, control chars, and trims whitespace.
pub fn sanitize_name(name: &str) -> String {
    let bad = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    let mut out: String = name
        .chars()
        .filter(|c| !c.is_control() && !bad.contains(c))
        .collect();
    out = out.trim().to_string();
    // Collapse internal whitespace to underscores for nicer filenames.
    out = out.split_whitespace().collect::<Vec<_>>().join("_");
    if out.len() > 60 {
        out.truncate(60);
    }
    out
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_drops_separators() {
        assert_eq!(sanitize_name("foo/bar"), "foobar");
        assert_eq!(sanitize_name("a:b*c?d"), "abcd");
    }

    #[test]
    fn sanitize_collapses_whitespace() {
        assert_eq!(sanitize_name("  hello   world  "), "hello_world");
    }

    #[test]
    fn sanitize_drops_controls() {
        assert_eq!(sanitize_name("ok\u{7}name"), "okname");
    }

    #[test]
    fn sanitize_truncates() {
        let s = "x".repeat(100);
        assert_eq!(sanitize_name(&s).len(), 60);
    }
}
