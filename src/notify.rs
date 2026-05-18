use std::io::Write;

/// Emit a terminal BEL byte. Most terminals translate this into either a
/// visual flash or an audible bell depending on user config.
pub fn bell() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

/// Emit OSC 9 (iTerm2 / Windows Terminal) and OSC 777 (Konsole / KDE)
/// notification sequences. These are completely ignored by terminals that
/// don't recognise them, so it's safe to send both unconditionally.
/// `title` and `body` must not contain BEL (`\x07`) or ESC (`\x1b`); we
/// sanitise just in case.
pub fn osc_notify(title: &str, body: &str) {
    let title = sanitise(title);
    let body = sanitise(body);
    let mut out = std::io::stdout();
    // OSC 9 — iTerm2 form: `ESC ] 9 ; <text> BEL`
    let _ = write!(out, "\x1b]9;{}: {}\x07", title, body);
    // OSC 777 — Konsole / "Konsole notify" form:
    //   `ESC ] 777 ; notify ; <title> ; <message> BEL`
    let _ = write!(out, "\x1b]777;notify;{};{}\x07", title, body);
    let _ = out.flush();
}

/// Strip anything that could break an OSC sequence: ASCII control bytes
/// (`<` 0x20 incl. `\n`, `\r`, BEL, ESC, NUL), DEL (0x7f), the C1 ST byte
/// (0x9c), and `;` which is the OSC field separator. A user-supplied tab
/// title containing a newline used to terminate the OSC early and corrupt
/// the next render; this filter prevents that.
/// POST a JSON `{title, body, tab, cwd}` payload to `url` via background
/// curl. Fire-and-forget — failures are silent so a misconfigured webhook
/// never disrupts the TUI.
///
/// Security: URL is **validated** to start with `http://` or `https://` so a
/// config value containing curl flags or weird schemes can't smuggle in
/// arbitrary behaviour (`--upload-file …` would otherwise let an attacker
/// exfiltrate files). We also pass `--` before the URL so curl treats it
/// strictly as a positional argument.
pub fn webhook(url: &str, title: &str, body: &str, tab: &str, cwd: &str) {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    let payload = serde_json::json!({
        "title": title,
        "body": body,
        "tab": tab,
        "cwd": cwd,
    })
    .to_string();
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-sS",
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "--data-binary",
        "@-",
        "--max-time",
        "5",
        "--",
    ])
    .arg(url)
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(mut child) = cmd.spawn() else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
    // Reap in a detached thread so POSIX doesn't accumulate zombies.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn sanitise(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            !c.is_control() && c != ';' && c != '\u{007f}' && c != '\u{009c}'
        })
        .collect()
}

/// Show a Windows toast notification. No-op on non-Windows platforms.
/// Fire-and-forget — errors are swallowed because failing to notify must
/// never disrupt the TUI.
pub fn toast(title: &str, body: &str) {
    #[cfg(windows)]
    {
        let xml_title = xml_escape(title);
        let xml_body = xml_escape(body);
        let script = format!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null;\
             [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType=WindowsRuntime] | Out-Null;\
             $x = New-Object Windows.Data.Xml.Dom.XmlDocument;\
             $x.LoadXml('<toast><visual><binding template=\"ToastText02\"><text id=\"1\">{}</text><text id=\"2\">{}</text></binding></visual></toast>');\
             $t = New-Object Windows.UI.Notifications.ToastNotification $x;\
             [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('cmux').Show($t)",
            xml_title, xml_body,
        );
        let encoded = encode_for_powershell(&script);
        // CREATE_NO_WINDOW so PowerShell doesn't flash a console window.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-WindowStyle",
                "Hidden",
                "-EncodedCommand",
                &encoded,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = (title, body);
    }
}

#[cfg(windows)]
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// PowerShell `-EncodedCommand` expects base64 of the UTF-16LE bytes.
#[cfg(windows)]
fn encode_for_powershell(script: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(script.len() * 2);
    for u in script.encode_utf16() {
        bytes.push((u & 0xff) as u8);
        bytes.push((u >> 8) as u8);
    }
    base64_encode(&bytes)
}

#[cfg(windows)]
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    match input.len() - i {
        1 => {
            let n = (input[i] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn base64_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_one_byte() {
        assert_eq!(base64_encode(b"a"), "YQ==");
    }

    #[test]
    fn base64_two_bytes() {
        assert_eq!(base64_encode(b"ab"), "YWI=");
    }

    #[test]
    fn base64_three_bytes() {
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn base64_longer() {
        assert_eq!(base64_encode(b"Hello, World!"), "SGVsbG8sIFdvcmxkIQ==");
    }

    #[test]
    fn xml_escape_all() {
        assert_eq!(xml_escape("a&b<c>d'e\"f"), "a&amp;b&lt;c&gt;d&apos;e&quot;f");
    }
}
