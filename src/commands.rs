use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandSource {
    BuiltIn,
    User,
    Project,
}

impl CommandSource {
    pub fn badge(&self) -> &'static str {
        match self {
            CommandSource::BuiltIn => "",
            CommandSource::User => "user",
            CommandSource::Project => "project",
        }
    }
}

#[derive(Clone)]
pub struct CommandEntry {
    pub name: String,
    pub desc: String,
    pub source: CommandSource,
}

/// Build the full command list shown in the F4 sidebar.
///
/// Order: project-local first (more specific overrides), then user, then
/// built-ins. Names are deduplicated — the first occurrence wins, so later
/// duplicates (a built-in `/review` when the user has a custom `review.md`)
/// are dropped silently.
pub fn load(active_cwd: &Path, builtins: &[(&str, &str)]) -> Vec<CommandEntry> {
    let mut out: Vec<CommandEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut push = |entry: CommandEntry, out: &mut Vec<_>| {
        if seen.insert(entry.name.clone()) {
            out.push(entry);
        }
    };

    for e in scan_dir(&active_cwd.join(".claude").join("commands"), CommandSource::Project) {
        push(e, &mut out);
    }
    if let Some(home) = home_dir() {
        for e in scan_dir(&home.join(".claude").join("commands"), CommandSource::User) {
            push(e, &mut out);
        }
    }
    for (name, desc) in builtins {
        push(
            CommandEntry {
                name: (*name).to_string(),
                desc: (*desc).to_string(),
                source: CommandSource::BuiltIn,
            },
            &mut out,
        );
    }
    out
}

fn home_dir() -> Option<PathBuf> {
    let s = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

fn scan_dir(dir: &Path, source: CommandSource) -> Vec<CommandEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.is_empty() {
            continue;
        }
        let desc = extract_description(&p).unwrap_or_default();
        out.push(CommandEntry {
            name: format!("/{}", stem),
            desc,
            source,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Pull a one-line description from a `.md` command file.
///
/// Tries (in order):
/// 1. YAML frontmatter `description: ...` field if file starts with `---`.
/// 2. The first non-blank, non-heading content line.
fn extract_description(path: &Path) -> Option<String> {
    let f = File::open(path).ok()?;
    let mut rdr = BufReader::new(f);
    let mut first_line = String::new();
    let n = rdr.read_line(&mut first_line).ok()?;
    if n == 0 {
        return None;
    }
    if first_line.trim() == "---" {
        // Walk frontmatter looking for `description:`.
        let mut line = String::new();
        for _ in 0..40 {
            line.clear();
            let n = rdr.read_line(&mut line).ok()?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed == "---" {
                break;
            }
            if let Some(rest) = trimmed
                .strip_prefix("description:")
                .or_else(|| trimmed.strip_prefix("Description:"))
            {
                let v = rest.trim().trim_matches(['"', '\''].as_ref());
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    } else {
        let trimmed = first_line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            return Some(trimmed.to_string());
        }
    }

    // Fall back to scanning a few more lines for the first non-blank
    // non-heading line.
    let mut line = String::new();
    for _ in 0..40 {
        line.clear();
        let n = rdr.read_line(&mut line).ok()?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        return Some(trimmed.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_md(contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cmux_test_{}.md",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn desc_from_frontmatter() {
        let p = temp_md("---\ndescription: \"Test command\"\nother: foo\n---\n# Heading\nBody.");
        assert_eq!(extract_description(&p), Some("Test command".to_string()));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn desc_from_first_line() {
        let p = temp_md("First non-heading line\n\nMore body.\n");
        assert_eq!(
            extract_description(&p),
            Some("First non-heading line".to_string())
        );
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn desc_skips_headings() {
        let p = temp_md("# Title only\n## Subtitle\n\nThe real first sentence.\n");
        assert_eq!(
            extract_description(&p),
            Some("The real first sentence.".to_string())
        );
        let _ = std::fs::remove_file(p);
    }
}
