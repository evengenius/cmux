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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SavedTab {
    pub cwd: PathBuf,
    pub session_id: Option<String>,
    pub title: String,
    /// Tab creation time in UNIX seconds (used to resolve session_id later).
    #[serde(default)]
    pub created_at_unix: u64,
}

pub fn layout_path() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".cmux").join("layout.json")
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

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
