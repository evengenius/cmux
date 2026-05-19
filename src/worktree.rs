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

/// Walk the directories that hold cmux-managed worktrees and return any leaf
/// dir that's NOT in the `known` set. `known` is the union of
/// `worktree_owned` paths from all live tabs + the auto-saved layout + every
/// named layout — anything else under one of `worktree_roots` is an orphan.
///
/// `worktree_roots` is the set of directories cmux *could* have created
/// worktrees in: the configured `[git] worktree_root` for the active config
/// plus, defensively, any parent of a `known` path we found in saved
/// layouts (in case the user changed the config and orphans are now in an
/// old root).
pub fn find_orphans(
    worktree_roots: &[PathBuf],
    known: &std::collections::HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in worktree_roots {
        let Ok(rd) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            // A git WORKTREE has `.git` as a regular FILE containing
            // `gitdir: ...`. A plain clone has `.git` as a directory; a
            // submodule has the gitdir file too but lives inside its parent
            // repo. We only delete things that look exactly like worktrees
            // so we can't nuke a foreign clone the user happens to have
            // under our scan root.
            if !is_worktree_marker(&p) {
                continue;
            }
            // Compare via canonical-ish form so symlink/case differences
            // don't mark a known path as orphan.
            if !known.iter().any(|k| paths_equal(k, &p)) {
                out.push(p);
            }
        }
    }
    out
}

/// True only if `dir/.git` is a regular file starting with `gitdir:` —
/// the worktree marker format. Rejects plain repos (`.git` is a dir) and
/// random dirs with a `.git` of any other shape.
fn is_worktree_marker(dir: &Path) -> bool {
    let marker = dir.join(".git");
    let Ok(meta) = std::fs::metadata(&marker) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(s) = std::fs::read_to_string(&marker) else {
        return false;
    };
    s.trim_start().starts_with("gitdir:")
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Force-remove a worktree from its repo. Used by `--prune-worktrees` to
/// discard orphans. Tries `git worktree remove --force .` from the worktree
/// path itself (git resolves the repo from the `.git` file). If git
/// refuses, falls back to `remove_dir_all` and surfaces git's stderr in
/// the error so the user knows what they're losing.
///
/// After a successful remove (either path), also calls
/// `git -C <repo> worktree prune` on the parent repo (resolved from the
/// `.git` marker) so dangling metadata doesn't accumulate.
pub fn force_remove(worktree_dir: &Path) -> Result<(), String> {
    let parent_repo = parent_repo_of_worktree(worktree_dir);
    let out = Command::new("git")
        .args(["worktree", "remove", "--force", "."])
        .current_dir(worktree_dir)
        .output()
        .map_err(|e| format!("spawn git failed: {}", e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        std::fs::remove_dir_all(worktree_dir)
            .map_err(|e| format!("git refused ({}) and rm -rf failed: {}", stderr, e))?;
        // Tell the user what git refused even though we steamrolled it.
        eprintln!("note: git declined ({}); removed the dir anyway", stderr);
    }
    if let Some(repo) = parent_repo {
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(repo)
            .output();
    }
    Ok(())
}

/// Read `<dir>/.git` (worktree marker) and resolve the parent repo's main
/// git dir, then walk up to its working tree. Returns None if anything
/// looks off (caller falls back to "just don't prune").
fn parent_repo_of_worktree(worktree_dir: &Path) -> Option<PathBuf> {
    let marker = worktree_dir.join(".git");
    let s = std::fs::read_to_string(&marker).ok()?;
    let rest = s.trim().strip_prefix("gitdir:")?.trim();
    // `gitdir: /abs/path/to/repo/.git/worktrees/<name>` →
    // repo working tree is the dir whose `.git` dir contains this.
    let gitdir = PathBuf::from(rest);
    // Walk up: .git/worktrees/<name> → .git → repo root.
    let repo_git = gitdir.parent()?.parent()?;
    repo_git.parent().map(|p| p.to_path_buf())
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
