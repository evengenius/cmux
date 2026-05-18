//! Cross-platform clipboard write via shelling out to the OS standard tool.
//! No extra crate dependencies — we already shell out for OS notifications,
//! so the pattern is familiar. Errors are returned as `String` so the caller
//! can surface them in a confirm-modal-style message.

use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug)]
pub enum CopyError {
    Spawn(String),
    Write(String),
    Status(i32, String),
    /// Only reachable on Linux/BSD where wl-copy/xclip/xsel are all missing.
    #[allow(dead_code)]
    NoBackend,
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CopyError::Spawn(s) => write!(f, "couldn't spawn clipboard tool: {}", s),
            CopyError::Write(s) => write!(f, "write to clipboard tool failed: {}", s),
            CopyError::Status(c, s) => write!(f, "clipboard tool exit {}: {}", c, s),
            CopyError::NoBackend => write!(
                f,
                "no clipboard backend found (install wl-clipboard, xclip, or xsel)"
            ),
        }
    }
}

/// Copy `text` to the system clipboard. Returns `Ok(backend_name)` so the
/// caller can mention which tool was used.
#[cfg(windows)]
pub fn copy(text: &str) -> Result<&'static str, CopyError> {
    write_to("clip.exe", &[] as &[&str], text).map(|_| "clip.exe")
}

#[cfg(target_os = "macos")]
pub fn copy(text: &str) -> Result<&'static str, CopyError> {
    write_to("pbcopy", &[] as &[&str], text).map(|_| "pbcopy")
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn copy(text: &str) -> Result<&'static str, CopyError> {
    // Wayland first if WAYLAND_DISPLAY is set, else X11.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && which_exists("wl-copy") {
        return write_to("wl-copy", &[] as &[&str], text).map(|_| "wl-copy");
    }
    if which_exists("xclip") {
        return write_to("xclip", &["-selection", "clipboard"], text).map(|_| "xclip");
    }
    if which_exists("xsel") {
        return write_to("xsel", &["--clipboard", "--input"], text).map(|_| "xsel");
    }
    Err(CopyError::NoBackend)
}

fn write_to(exe: &str, args: &[&str], text: &str) -> Result<(), CopyError> {
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CopyError::Spawn(e.to_string()))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| CopyError::Write(e.to_string()))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| CopyError::Spawn(e.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(CopyError::Status(out.status.code().unwrap_or(-1), stderr));
    }
    Ok(())
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn which_exists(exe: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(exe).exists())
}
