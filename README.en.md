# cmux

*[Русская версия](README.md) · English*

**TL;DR** — A Midnight-Commander-style TUI that runs several `claude` sessions in one terminal window. Two-row top chrome: row 0 lists projects (unique cwds), row 1 lists chats inside the active project. Cross-session search, scrollback search of the active chat, file browser, bottom shell pane, OS notifications when a session is waiting on you.

---

## Overview

A TUI host for the [Claude Code](https://claude.com/claude-code) CLI. Run many projects and dozens of chats in parallel — each tab is its own `claude` inside a real PTY, rendered the same as a regular terminal. Open chats are grouped by their cwd ("project"); you switch projects and switch chats inside a project independently.

## What it gives you over plain `claude`

### Projects and chats grouping

- **Two-row top chrome**: row 0 — projects (unique cwds among open tabs), row 1 — chats in the active project.
- `F2` opens a new chat **in the current project** (active tab's cwd, not the cwd cmux was launched from).
- `F6 → OpenHere` spawns a chat in the selected directory. If that cwd already matches a project, the chat joins it; otherwise a new project appears in row 0 with its first chat.
- Closing the last chat of a project removes the project from row 0.

### Sessions

- **F3 sidebar** — sessions from `~/.claude/projects/`, scoped to the active tab's cwd hierarchy. Live text filter, deep grep across `.jsonl` content via `F5`. `Ctrl+R` re-reads from disk.
- **Shift+F3 global modal** — every session, grouped by directory with section headers. Same text filter; `↑/↓` skip headers. `Ctrl+R` refresh.

### Search

- **Command palette F9** — fuzzy-pick any action or session. Also where you save/switch/delete named layouts and pin/unpin the active tab.
- **Chat-history search Ctrl+F** — overlay at the bottom of the PTY. Matches are highlighted yellow in-place plus a snippet below the input with an `N/M` counter. `↑/↓` (or Enter for "next") step through hits; the vt100 scrollback follows so the selected line stays visible.

### Files

- **Files sidebar Ctrl+B** — VSCode-style tree, chrooted to the active tab's cwd; `Space` inserts the relative path into the chat input.
- **F6 modal explorer** — navigate the whole filesystem. On Windows `←` at a drive root (`C:\`) jumps to a virtual drives list. `Space` inserts an absolute path; `Enter` on the "OpenHere" entry spawns a new chat in that cwd.

### Chats and tabs

- **Rename Shift+F2** — modal pre-filled with the title. Empty input resets to cwd basename.
- **Pin/Unpin** — pinned chats refuse F8/Alt+W close, marked with 📌. Toggle via palette.
- **Drag-to-reorder** — mouse: down on chat A, release on chat B → swap (within the same project).
- **`[N]` badge** — on inactive chats, the count of completed replies (streaming → idle transitions) you've missed. Cleared on focus.

### Claude slash commands

- **F4** — sidebar listing slash commands. Loads project-local (`<cwd>/.claude/commands/*.md`) and user-global (`~/.claude/commands/*.md`). Each entry shows a `[project]` / `[user]` badge plus the built-ins. Description comes from a YAML `description:` frontmatter field or the first non-heading line of the .md.

### Environment

- **Bottom shell pane `` Ctrl+` ``** — embedded PTY running the parent shell (PowerShell / pwsh / bash / zsh / fish / cmd, auto-detected via sysinfo). Auto-`cd`s to the active tab's cwd on switch (toggleable).
- **Bracketed paste** — multi-line clipboard pastes reach claude as one atomic chunk, no premature Enter / submit on every `\n`.

### Signalling

- **Activity overlay**: `●` (streaming) or `!` (awaiting permission) after the chat title.
- **OS notifications** on the transition into AwaitingPermission: terminal BEL plus a Windows toast (via `powershell -EncodedCommand` + WinRT — no extra deps). On macOS/Linux the toast is a no-op, BEL still works.
- AwaitingPermission patterns are editable in config — add your own if claude uses different wording in your locale.

### Parallel workflow

- **Broadcast prompt** (palette → `★ Broadcast prompt to all chats in active project…`) — one typed text reaches every chat in the active project with `\r`. Use cases: "run cargo check across all of them", "commit ready work".
- **Per-model new tab** (palette → `★ New tab with model: opus / sonnet / haiku`) — spawns a fresh chat with `claude --model <name>`.

### Quality of life

- **Ctrl+Click on a URL** in the PTY view opens it in the OS default handler (`start` / `open` / `xdg-open`). The regex trims trailing sentence punctuation.
- **Visible scrollbar** on the right edge of the chat with `▲`/`█`/`▼`. Click / drag / wheel-scroll. Red dots on the track mark match positions while Ctrl+F is active.

### Layout

- **Auto-save** — current state is written to `~/.cmux/layout.json` on every change (chat open/close, pin toggle).
- **Auto-restore on startup** — opt-in (`[layout] auto_restore = true`).
- **Named layouts** — `~/.cmux/layouts/<name>.json`. Via the palette: `★ Save current layout as…`, `⇄ Switch to layout: …`, `✕ Delete saved layout: …`.

### Click-friendly

- Every F-key has a button in the bottom bar; chats, projects, `+`, borders are clickable; borders drag-to-resize.

## Requirements

- Rust toolchain (>= 1.75) to build from source.
- [Claude Code](https://claude.com/claude-code) on `PATH`:
  - Windows — `claude.cmd`.
  - Linux/macOS — `claude`.
- Windows 10+ — primary platform (tested). Linux/macOS support is in the codebase but not actively tested.

## Install

```powershell
git clone https://github.com/evengenius/cmux
cd cmux
cargo build --release
# Resulting binary: target/release/cmux.exe (Windows) or target/release/cmux (Unix)
```

Move/symlink the binary somewhere on your `PATH`, then run `cmux`.

## CLI

```
cmux [PATH] [--layout NAME] [--resume ID] [--continue]
cmux -h | --help | -V | --version
```

| Arg | Effect |
| --- | --- |
| `PATH` | Starting cwd for the first chat (default: current directory). |
| `--layout NAME` | Apply `~/.cmux/layouts/<NAME>.json` on startup. |
| `--resume ID` | First tab = `claude --resume <id>`. |
| `--continue` | First tab = `claude --continue` (resume the latest session in this cwd). |

Before entering raw mode, cmux verifies that `claude` (or `claude.cmd` on Windows) is on `PATH`. If missing, you get a clear error instead of a cryptic PTY failure.

## Keymap

### Global

| Key | Action |
| --- | --- |
| `F1` | Help overlay (full keymap) |
| `F2` | New chat in the active project |
| `F3` | Sessions sidebar (scoped to active tab's cwd) |
| `Shift+F3` | Global sessions modal (grouped by directory) |
| `F4` | Slash-commands sidebar (built-in + user + project) |
| `F5` | Toggle deep-grep (while F3 sidebar is focused) |
| `F6` | File explorer modal (whole filesystem) |
| `F7` | Toggle mouse mode (off = native terminal selection) |
| `F8` | Close active chat (pinned chats refuse) |
| `F9` | Command palette |
| `F10` / `Ctrl+Q` | Quit |
| `Shift+F2` | Rename active chat |

### Chat navigation (within the active project)

| Key | Action |
| --- | --- |
| `F11` / `F12` | Previous / next chat |
| `Alt+←` / `Alt+→` | Same |
| `Alt+1..9` | Chat N within the active project |
| `Ctrl+PgUp` / `Ctrl+PgDn` | Previous / next chat |
| `Alt+T` / `Alt+W` | New / close chat |

### Project navigation

| Key | Action |
| --- | --- |
| `Ctrl+F11` / `Ctrl+F12` | Previous / next project |
| `Ctrl+Shift+1..9` | Jump to project N |

### Search

| Key | Action |
| --- | --- |
| `Ctrl+F` | Search active chat's scrollback |
| `↑` / `↓` or `Enter` | Step through matches |
| `Alt+R` | Toggle regex mode (sticky across opens) |
| `Esc` | Close, scrollback → 0 |

### Environment

| Key | Action |
| --- | --- |
| `Ctrl+B` | Files sidebar (chroot to tab cwd) |
| `` Ctrl+` `` | Bottom shell pane (parent shell) |
| `PgUp` / `PgDn` | PTY scrollback (when claude is focused) |
| `Esc` (in sidebar/bottom) | Unfocus — panel stays visible, keys go back to claude |
| `Drag border` | Resize sidebar / bottom pane |
| `Shift + drag` | Native terminal text selection (for copy) |

### Sidebars

| Key | Action |
| --- | --- |
| `F3` `Ctrl+R` | Re-read session list from disk |
| `Sidebar Enter` | Files: cd / open here · Sessions: resume · Commands: submit |
| `Sidebar Space` | Files: insert *relative* path · Commands: insert (no submit) |
| `F6 Space` | Insert *absolute* path |

### Mouse

- **Row 0 (projects)**: click → switch project; double-click → close the whole project (asks for confirmation); click `+ project` → file browser anchored at the active cwd for a new chat.
- **Row 1 (chats)**: click → switch chat; double-click → close chat (pinned → confirmation); right-click → rename; drag (down→up on another chat) → swap within the project.
- **Chat PTY**: `Ctrl+Click` on a URL opens it in the OS default browser; wheel scrolls scrollback; `Shift+Drag` — native terminal text selection.
- **Scrollbar (right edge of chat)**: click/drag to set position; wheel scrolls.
- **Sidebars**: wheel steps through entries; drag the border to resize.

## Configuration

Config lives at `~/.cmux/config.toml`, created with defaults on first launch. After editing, press `F9 → ★ Reload config` to apply without restart (some fields only apply to newly-spawned panes/tabs).

```toml
[layout]
sidebar_width = 42         # left sidebar (Sessions/Commands)
right_sidebar_width = 42   # right sidebar (Files)
bottom_height = 12         # bottom shell pane
auto_restore = false       # auto-restore ~/.cmux/layout.json on startup
scrollback_lines = 10000   # PTY scrollback cap; ~100B × cap × open tabs

[browser]
show_hidden = false        # show .git, .claude, etc.

[shell]
exe = ""                   # override the bottom-pane shell
args = []                  #   empty = autodetect from parent process
follow_tab_cwd = true      # cd the bottom shell when switching tabs
# Example:
# exe = "pwsh.exe"
# args = ["-NoLogo"]

[detect]
# Lowercase substrings that flag a tab as "awaiting your input".
# Add patterns here if claude uses different wording in your locale.
permission_patterns = [
  "do you want to",
  "allow this tool",
  "approve this",
  "(y/n)",
]

[notify]
bell = true    # terminal BEL on AwaitingPermission transition
toast = true   # Windows toast (no-op on other OSes)

[theme]
# Accent colour used for palette / save-as modal borders, etc.
# cyan | yellow | green | magenta | red | blue | white | gray | light*
accent = "cyan"

[keys]
# Remap nullary actions to custom keys. Format:
#   action_name = "key_combo"
# Actions are snake_case versions of internal Action variants.
# Keys: f1..f12, single chars, ctrl-x, alt-shift-f4, etc.
# Defaults stay in place — these entries are overrides / additions.
# Example:
#   toggle_search = "f4"     # F4 opens search instead of F4=commands
#   quit          = "ctrl-x" # Ctrl+X quits (in addition to F10/Ctrl+Q)
```

## Layout persistence

- **Auto-save** to `~/.cmux/layout.json` on every state change.
- **Auto-restore** on startup: only when `[layout] auto_restore = true`. Otherwise pick `F9 → ★ Restore previous layout` manually.
- **Named layouts** in `~/.cmux/layouts/<name>.json`:
  - `★ Save current layout as…` (palette) — prompts for a name, sanitises (strips `/ \ : * ? " < > |`, control bytes, collapses whitespace to `_`).
  - `⇄ Switch to layout: <name>` per layout.
  - `✕ Delete saved layout: <name>` per layout.

Per-tab persisted fields: `cwd`, `session_id` (for resume), `title`, `created_at_unix`, `pinned`.

## Indicators

In the chat bar:

- **`●` (yellow)** — claude is streaming.
- **`!` (red, blinking)** — screen text matches one of `permission_patterns` — claude is waiting on you.
- **`[N]`** — N completed replies (streaming → idle transitions) you haven't seen. Cleared on focus.
- **`📌`** — chat is pinned.

In the project bar: active project highlighted in cyan.

## Chat-history search

`Ctrl+F` opens an overlay at the bottom of the PTY. Each keystroke recomputes matches (case-insensitive). Hits are highlighted yellow in-place plus a context snippet under the input showing the current match with an `N/M` counter. `↑/↓` (or Enter for "next") step through; the vt100 scrollback follows so the selected match stays visible.

Search runs over a plain-text mirror of the PTY (ANSI is stripped, `\r` resets the line, `\n` commits it). Buffer cap is 10k lines, matching vt100's scrollback.

Known limitation: matches that span a line wrap aren't found — each visible row is scanned independently.

## How it works

- Each tab spawns a real PTY via `portable-pty`:
  - Windows: `cmd.exe /c claude.cmd [--resume <id>]`.
  - Unix: `claude [--resume <id>]`.
- Output flows into a `vt100` parser (rendered via `tui-term` + `ratatui`) and in parallel into a separate text-only mirror used by Ctrl+F.
- The 10k-line scrollback is owned by us, so wheel/PgUp/PgDn scroll history without leaking into the inner app.
- Deep-grep runs in a background thread over an `mpsc` channel; auto-cancels when the query changes.
- Bottom-pane shell is identified via `sysinfo` (parent PID → image name → known shell).
- AwaitingPermission detection scans `parser.screen().contents()` (lowercased) for substring matches from `[detect] permission_patterns`.
- Windows toast: a background `powershell.exe -EncodedCommand <UTF-16LE base64>` call into WinRT's `ToastNotificationManager`. No extra crates.

## Limitations

- Primary platform is Windows. Linux/macOS code compiles and should run, but isn't actively tested here.
- With `follow_tab_cwd = true` the bottom shell receives a `cd` line on tab switch — if you're typing into the shell at that moment, your input gets clobbered. To prevent that, follow only fires when the bottom pane isn't focused.
- `session_id` for *new* (non-resumed) tabs is resolved heuristically at save time by matching tab cwd against the freshest `.jsonl` in `~/.claude/projects/`. Two concurrent tabs in the same cwd may attribute incorrectly.
- Keybindings aren't config-driven yet (F-keys are hardcoded).
- In-PTY match highlighting doesn't cross line wraps.

## License

MIT — see [LICENSE](LICENSE).
