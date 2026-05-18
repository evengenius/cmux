# cmux

*[Русская версия](README.md) · English*

**TL;DR** — A Midnight-Commander-style TUI that lets you run several `claude` sessions in one terminal window with tabs, sidebars, a bottom shell pane, and an indicator showing which session is waiting for you.

---

## Overview

A Midnight-Commander-style TUI host for the [Claude Code](https://claude.com/claude-code) CLI.
Run several `claude` sessions side-by-side in one terminal window — with tabs, sessions sidebar,
cross-session search, file browser, a bottom shell pane, and an activity indicator that tells you
which session is waiting for your permission.

## What it gives you over plain `claude`

- **Tabs** — open multiple `claude` processes in one window, switch with `F11/F12` or click.
- **Sessions sidebar** (`F3`) — list every session from `~/.claude/projects/`, live-filter by title/cwd/branch, `Enter` to resume in a new tab.
- **Deep grep** (`F5`) — when filtering sessions, grep through the actual `.jsonl` content in the background and see snippet previews of where the match was.
- **Files sidebar** (`Ctrl+B`) — VSCode-style file tree, chrooted to the active tab's cwd; `Space` inserts the relative path into the chat input.
- **File explorer modal** (`F6`) — navigate the whole filesystem; `Space` inserts an absolute path into the chat input.
- **Command palette** (`F9`) — fuzzy-pick any action *or* any session.
- **Bottom shell pane** (`Ctrl+\``) — embedded PTY running your parent shell (PowerShell / pwsh / bash / cmd, auto-detected). Run `git status`, `cargo build` etc. without switching windows.
- **Activity overlay** — each tab shows `●` (streaming) or `!` (awaiting permission) so you can run several Claude sessions in parallel and see who needs you.
- **Persistent layout** — when you quit, tabs and their session IDs are saved to `~/.cmux/layout.json`; on next launch, `F9 → Restore previous layout` brings them all back with `--resume`.
- **Click-friendly chrome** — every F-key has a button in the bottom bar; tabs, `+`, borders are all clickable; drag borders to resize.

## Requirements

- Windows 10+ (other OSes may work but only Windows is tested)
- [Claude Code](https://claude.com/claude-code) installed and on `PATH` as `claude.cmd`
- Rust toolchain (>= 1.75) to build from source

## Install

```powershell
git clone https://github.com/evengenius/cmux
cd cmux
cargo build --release
# Resulting binary: target/release/cmux.exe
```

Move/symlink `target/release/cmux.exe` somewhere on your `PATH`, then run `cmux`.

## Keyboard quick-reference

| Key | Action |
| --- | --- |
| `F1` | Help overlay (full keymap) |
| `F2` | New tab |
| `F3` | Sessions sidebar |
| `F4` | Claude commands sidebar |
| `F5` | Toggle deep-grep (in Sessions) |
| `F6` | File explorer (modal, whole filesystem) |
| `F7` | Toggle mouse mode (off = native terminal text select) |
| `F8` | Close active tab (last tab kept) |
| `F9` | Command palette |
| `F10` / `Ctrl+Q` | Quit |
| `F11` / `F12` | Previous / next tab |
| `Ctrl+B` | Files sidebar (chrooted to tab cwd) |
| `` Ctrl+` `` | Bottom shell pane |
| `Esc` (in sidebar / bottom) | Unfocus — panel stays visible, keys go back to claude |
| `Alt+1..9` | Switch to tab N |
| `Alt+T` / `Alt+W` | New / close tab |
| `Sidebar Enter` | Files: cd / open here · Sessions: resume · Commands: submit |
| `Sidebar Space` | Files: insert *relative* path · Commands: insert (no submit) |
| `F6 Space` | Insert *absolute* path |
| `Drag border` | Resize sidebar / bottom pane |
| `Shift + drag` | Native terminal text select (for copy) |

## Configuration

Configuration lives at `~/.cmux/config.toml` (created with defaults on first launch).

```toml
[layout]
sidebar_width = 42         # left sidebar (Sessions/Commands)
right_sidebar_width = 42   # right sidebar (Files)
bottom_height = 12         # bottom shell pane

[browser]
show_hidden = false        # show .git, .claude etc. in file browser

[shell]
# Override the bottom-pane shell. Empty = autodetect from parent process.
exe = ""
args = []
# Example:
# exe = "pwsh.exe"
# args = ["-NoLogo"]
```

After editing the config, press `F9 → ★ Reload config` to apply without restart.
Some settings (scrollback size, the bottom shell) only take effect for newly-spawned panes.

## Persistent layout

Each time you open/close a tab, the current state is written to `~/.cmux/layout.json` (tab cwds + resolved session IDs).
On startup the file is loaded but **not** restored automatically — open the command palette (`F9`) and pick
**★ Restore previous layout** to bring it back.

## Activity indicators

In the tab bar, after the tab title:

- **`●` (yellow)** — recent output, the embedded `claude` is streaming.
- **`!` (red, blinking)** — text on screen matches a permission prompt (`do you want to`, `approve`, `allow this tool`, `(y/n)`) — Claude is waiting for you.
- **nothing** — idle.

Run five tabs in parallel, and switch to whichever one lights up red.

## How it works (load-bearing pieces)

- Each tab spawns `cmd.exe /c claude.cmd` (with optional `--resume <id>`) inside a real PTY (`portable-pty`), so Claude's TUI renders exactly as it would in a regular terminal.
- The PTY output flows through a `vt100` parser into a `tui-term` widget rendered by `ratatui`.
- A scrollback buffer (10k lines) is owned by us, so wheel/PgUp/PgDn scroll back through history without going to the underlying app.
- Deep-grep runs in a background thread that streams hits over an `mpsc` channel; cancellation is automatic when the query changes.
- Parent shell for the bottom pane is detected via `sysinfo` (PID of our parent process → image name → known shell).

## Limitations / known sharp edges

- Windows-only tested. Other OSes are best-effort.
- The bottom shell starts in the cwd that `cmux` was launched from — not the active tab's cwd.
- `session_id` for *new* (non-resumed) tabs is heuristically resolved at save-time by matching tab cwd to the freshest `.jsonl` in `~/.claude/projects/`. With two tabs in the same cwd it might attribute wrongly.
- Custom keybindings are not yet config-driven — F-keys are hardcoded.
- If a `.jsonl` line contains a `lowercase()`-resizing character (Turkish İ, German ß), grep snippet may show the lowercased form rather than the original case.

## License

MIT — see [LICENSE](LICENSE).
