//! Optional git-worktree-per-chat support. When enabled in config, spawning a
//! new chat inside a git repo creates a sibling worktree on a fresh branch
//! and routes the chat into that worktree instead of the repo root. Closing
//! the chat tears the worktree down (configurable).
//!
//! Failure modes are always graceful: not-in-a-repo, git missing, branch
//! collision, etc. → fall back to the original cwd. The TUI never blocks on
//! a worktree error.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Discover the git repo root containing `cwd`, or `None` if not in a repo
/// (or git isn't installed). Uses `git rev-parse --show-toplevel`.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

/// Pick a branch name that doesn't collide with an existing one. Tries
/// `<prefix><slug>` first, then appends `-2`, `-3`, … on collision.
pub fn pick_branch(repo: &Path, prefix: &str, slug: &str) -> String {
    let base = format!("{}{}", prefix, sanitise_slug(slug));
    if !branch_exists(repo, &base) {
        return base;
    }
    for n in 2..1000 {
        let candidate = format!("{}-{}", base, n);
        if !branch_exists(repo, &candidate) {
            return candidate;
        }
    }
    // Pathological — just append a timestamp.
    format!("{}-{}", base, now_unix())
}

fn branch_exists(repo: &Path, name: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", name)])
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `git worktree add --quiet -b <branch> <path>` from the repo root. Returns
/// the absolute path to the new worktree on success.
pub fn create(
    repo: &Path,
    worktree_dir: &Path,
    branch: &str,
) -> Result<PathBuf, String> {
    // The directory must NOT exist when `git worktree add` runs.
    if worktree_dir.exists() {
        return Err(format!(
            "worktree target already exists: {}",
            worktree_dir.display()
        ));
    }
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create parent dir failed: {}", e))?;
    }
    let out = Command::new("git")
        .args(["worktree", "add", "--quiet", "-b", branch])
        .arg(worktree_dir)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("spawn git failed: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(worktree_dir.to_path_buf())
}

/// `git worktree remove --force <path>`. Best-effort — log errors but don't
/// panic. Always also calls `git worktree prune` to clean dangling state.
pub fn remove(repo: &Path, worktree_dir: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(worktree_dir)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("spawn git failed: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // Tidy up: prune any references the worktree had.
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo)
        .output();
    Ok(())
}

/// Reduce an arbitrary string to a git-branch-safe slug. Replaces unsafe
/// chars with `-`, collapses runs, lowercases, caps at 40.
fn sanitise_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if ok {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 40 {
        out.truncate(40);
    }
    if out.is_empty() {
        "chat".to_string()
    } else {
        out
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_strips_unsafe() {
        assert_eq!(sanitise_slug("Hello World!"), "hello-world");
        assert_eq!(sanitise_slug("foo/bar:baz"), "foo-bar-baz");
        assert_eq!(sanitise_slug("__init__"), "__init__");
        assert_eq!(sanitise_slug(""), "chat");
        assert_eq!(sanitise_slug("---"), "chat");
    }

    #[test]
    fn slug_caps_at_40() {
        let s = "a".repeat(100);
        assert_eq!(sanitise_slug(&s).len(), 40);
    }
}
