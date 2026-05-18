use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde_json::Value;

pub struct SessionMeta {
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub project_dir: String,
    pub updated: SystemTime,
    pub git_branch: Option<String>,
    pub file_path: PathBuf,
    /// Approximate message count — number of jsonl lines we successfully
    /// parsed as objects. Cheap to compute during enumerate(); the user
    /// trades exactness for not having to read every byte.
    pub message_count: u32,
    /// Sum of input + output tokens across all `usage` records in the
    /// session's jsonl. 0 if the session never emitted usage info.
    pub total_tokens: u64,
}

pub fn claude_projects_root() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".claude").join("projects")
}

pub fn enumerate(root: &Path) -> Vec<SessionMeta> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for proj in rd.flatten() {
        if !proj.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let pname = proj.file_name().to_string_lossy().to_string();
        let Ok(files) = std::fs::read_dir(proj.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(meta) = parse_meta(&p, stem, &pname) {
                out.push(meta);
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated));
    out
}

fn parse_meta(path: &Path, id: &str, project_dir: &str) -> Option<SessionMeta> {
    let updated = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let file = File::open(path).ok()?;
    let mut rdr = BufReader::new(file);

    let mut title: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut message_count: u32 = 0;
    let mut total_tokens: u64 = 0;

    // Walk the full file so message_count and total_tokens reflect the whole
    // session. The 64-line cap used to be here to bail early after grabbing
    // cwd/title — fine for that, but it under-counted everything else. We
    // accept the extra read cost (jsonl is a few KB to a couple MB at most).
    let mut line = String::new();
    loop {
        line.clear();
        let n = rdr.read_line(&mut line).ok()?;
        if n == 0 {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };

        // Token usage records — read from assistant turns.
        if let Some(usage) = v
            .get("message")
            .and_then(|m| m.get("usage"))
            .or_else(|| v.get("usage"))
        {
            let inp = usage
                .get("input_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let outp = usage
                .get("output_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let cache_create = usage
                .get("cache_creation_input_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let cache_read = usage
                .get("cache_read_input_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            total_tokens = total_tokens
                .saturating_add(inp)
                .saturating_add(outp)
                .saturating_add(cache_create)
                .saturating_add(cache_read);
        }

        // Count "real" messages — user prompts and assistant turns. Internal
        // tool-use / system entries don't count for the badge.
        let typ = v.get("type").and_then(|t| t.as_str());
        if matches!(typ, Some("user") | Some("assistant")) {
            message_count = message_count.saturating_add(1);
        }

        if typ != Some("user") {
            continue;
        }
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                cwd = Some(PathBuf::from(c));
            }
        }
        if branch.is_none() {
            if let Some(b) = v.get("gitBranch").and_then(|b| b.as_str()) {
                if !b.is_empty() {
                    branch = Some(b.to_string());
                }
            }
        }
        if title.is_none() {
            // Content may be a string or an array of content blocks
            let content = v.get("message").and_then(|m| m.get("content"));
            let raw = match content {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .next()
                    .map(|s| s.to_string()),
                _ => None,
            };
            if let Some(s) = raw {
                let t = sanitize_title(&s);
                if !t.is_empty() {
                    title = Some(t);
                }
            }
        }
    }

    Some(SessionMeta {
        id: id.to_string(),
        cwd: cwd.unwrap_or_else(|| PathBuf::from(".")),
        title: title.unwrap_or_else(|| "(empty session)".to_string()),
        project_dir: project_dir.to_string(),
        updated,
        git_branch: branch,
        file_path: path.to_path_buf(),
        message_count,
        total_tokens,
    })
}

fn sanitize_title(s: &str) -> String {
    let one_line = s.replace(['\n', '\r', '\t'], " ");
    let trimmed: String = one_line.chars().take(80).collect();
    trimmed.trim().to_string()
}

/// Best-effort short basename of a path for display.
pub fn cwd_label(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| p.to_str().unwrap_or(""))
        .to_string()
}

/// Human-readable "Nh ago" / "Nd ago" from a SystemTime.
pub fn relative_time(t: SystemTime) -> String {
    let now = SystemTime::now();
    let Ok(d) = now.duration_since(t) else {
        return "now".to_string();
    };
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
