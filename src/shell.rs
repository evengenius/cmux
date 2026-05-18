use std::path::Path;

/// Return `(executable, args)` for the embedded bottom shell.
/// Strategy:
///   1. Look up the parent process via `sysinfo` and match its image name
///      against well-known shells.
///   2. Fall back to `$SHELL`, then `$COMSPEC`, then `cmd.exe`.
pub fn detect_parent_shell() -> (String, Vec<String>) {
    if let Some(found) = parent_shell_via_sysinfo() {
        return found;
    }
    default_shell()
}

fn parent_shell_via_sysinfo() -> Option<(String, Vec<String>)> {
    use sysinfo::{Pid, System};
    let sys = System::new_all();

    let self_pid = Pid::from_u32(std::process::id());
    let proc = sys.process(self_pid)?;
    let parent_pid = proc.parent()?;
    let parent = sys.process(parent_pid)?;
    let name = parent.name().to_string_lossy().to_lowercase();

    Some(match name.as_str() {
        "powershell.exe" | "powershell" => (
            "powershell.exe".to_string(),
            vec!["-NoLogo".to_string()],
        ),
        "pwsh.exe" | "pwsh" => (
            "pwsh.exe".to_string(),
            vec!["-NoLogo".to_string()],
        ),
        "cmd.exe" | "cmd" => ("cmd.exe".to_string(), vec![]),
        "bash.exe" | "bash" => ("bash.exe".to_string(), vec!["-i".to_string()]),
        "wt.exe" | "windowsterminal.exe" => {
            // Wrapping terminals don't host a shell themselves — fall through.
            return None;
        }
        _ => {
            // Unknown parent — also fall through to env-var fallback.
            return None;
        }
    })
}

fn default_shell() -> (String, Vec<String>) {
    if let Ok(s) = std::env::var("SHELL") {
        return (s, Vec::new());
    }
    if let Ok(s) = std::env::var("COMSPEC") {
        return (s, Vec::new());
    }
    ("cmd.exe".to_string(), Vec::new())
}

pub fn short_name(exe: &str) -> String {
    Path::new(exe)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(exe)
        .to_string()
}
