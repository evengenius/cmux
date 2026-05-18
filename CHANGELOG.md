# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). All dates UTC.
Cmux is pre-1.0 so every release is on `master`; this file rolls up the
git log into reader-friendly chunks.

## [Unreleased]

### Added

- **Project / chat two-row top bar.** Row 0 shows unique cwds among open
  tabs (projects); row 1 shows chats inside the active project. Click a
  project to switch; click `+ project` for a file-browser-driven new
  project. Per-project chat numbering (Alt+1..9 is N-th chat in active
  project, not the global index).
- **Pinning.** Chats and whole projects can be pinned. F8 / Alt+W /
  double-click on pinned chats opens a Y/N confirmation; same goes for
  closing a whole project via project-bar double-click. 📌 prefix marks
  pinned entries; state persists in `~/.cmux/layout.json`.
- **Tab rename.** `Shift+F2` (active tab) or right-click on any chat.
- **Drag-to-reorder.** Mouse down on a chat, release on another →
  swap (within the same project).
- **Unread badge `[N]`.** Counts completed claude turns (streaming →
  idle transitions) since last focus, per non-active chat.
- **Scrollback search (Ctrl+F).** Plain-text mirror of each PTY's output
  feeds a case-insensitive search; matches are painted yellow in the
  visible PTY area, red `•` markers appear on the right-edge scrollbar,
  and the vt100 scrollback follows so the selected match lands on
  screen. **Regex mode** (Alt+R) toggle, sticky across closes; regex
  parse errors surface as a red badge in the overlay header.
- **Visible scrollbar.** 1-cell right-edge ▲/█/▼ scrollbar. Click,
  drag, and wheel-scroll the column to navigate.
- **Sessions sidebar (F3) scoped to active tab cwd.** Live filter, deep
  grep with `F5`, `Ctrl+R` force refresh. **Shift+F3** opens a global
  modal with all sessions grouped by directory.
- **Project-local + user-global slash commands in F4.** Loads
  `<cwd>/.claude/commands/*.md` and `~/.claude/commands/*.md`, badges
  them with `[project]` / `[user]`, parses YAML `description:`
  frontmatter.
- **F6 explorer ⇒ new chat / new project.** `Enter` on the "OpenHere"
  virtual entry spawns a chat in the browser's cwd; if cwd matches an
  existing project it joins, otherwise a new project appears.
- **Bracketed paste forwarding.** Multi-line clipboard reaches the
  inner claude as one chunk — no premature submits on every `\n`.
- **Bottom shell follows tab cwd.** Embedded parent-shell pane `cd`s
  to the active tab's cwd on switch (configurable; safe — only when
  the shell isn't focused).
- **OS notifications on AwaitingPermission.** Terminal BEL + Windows
  toast (via `powershell -EncodedCommand` + WinRT, no extra crates).
  Configurable patterns in `[detect] permission_patterns`.
- **Named layouts.** Palette save / switch / delete; storage in
  `~/.cmux/layouts/<name>.json`.
- **`[layout] auto_restore`.** Opt-in: auto-restore the unnamed
  `layout.json` on startup.
- **Per-tab model selection.** Palette "New tab with model: opus /
  sonnet / haiku" spawns claude with `--model <name>`.
- **Broadcast prompt.** Palette modal sends one typed prompt (+ Enter)
  to every chat in the active project. Common workflow: "run cargo
  check across all chats here".
- **URL Ctrl+Click.** Detects URLs in the PTY view and opens them in
  the OS default handler (`start` / `open` / `xdg-open`).
- **CLI args.** `cmux [PATH] [--layout NAME] [--resume ID] [--continue]`,
  plus `-h`/`-V`. Pre-flight check confirms `claude` is on PATH;
  friendly error before raw mode if missing.
- **Custom keybindings.** `[keys]` config section binds any nullary
  Action to any key combo (`f4 = "toggle_search"`, etc.). Reload via
  F9 → Reload config.
- **Theme accent.** `[theme] accent = "cyan"` controls accent colour
  used by modal borders, etc.
- **Sticky filter.** F3 and F4 sidebars now remember the search query
  independently across close/open and F3↔F4 switches.
- **Wheel scroll over sidebars + scrollbar column.** Wheel stepping
  works in F3 / F4 / files sidebar, and over the chat scrollbar.
- **Windows drives root in F6.** `←` at `C:\` jumps to a virtual
  drive-letter list.
- **Cross-platform spawn.** Linux/macOS hit a plain `claude` binary;
  Windows still goes via `cmd.exe /c claude.cmd`.

### Changed

- F11 / F12 / Alt+1..9 now cycle **within the active project**, not the
  global tab list. Cross-project navigation lives on Ctrl+F11 /
  Ctrl+F12 and Ctrl+Shift+1..9.
- F2 (new tab) spawns in the active tab's cwd, not the launch cwd.
- Bottom button bar reordered to F1→F12 then Ctrl-shortcuts.
- Scrollbar math uses the real `text_buffer.total_lines()` length so
  the thumb is proportional to actual scrollable history (not the cap).
- Unread badge counts **completed replies**, not raw newlines — far
  more meaningful number for "how many things did I miss".

### Performance

- `total_lines` mirrored into an `Arc<AtomicUsize>` updated by the
  reader thread, so the scrollbar render hot path needs no mutex.
- `[layout] scrollback_lines` configurable (default 10000, clamp
  `[64, 1_000_000]`).

### Reliability

- Mutex lock callsites recover from poisoning (`unwrap_or_else(|p|
  p.into_inner())`) so one panicked reader thread doesn't cascade.
- Confirm modal gates all destructive close paths (pinned chat,
  project) — no more silent refusals.

## [0.1.0] — initial cmux

- TUI host for the claude CLI: tabs, sessions sidebar with deep-grep,
  file browser, bottom shell pane, activity overlay (`●` streaming,
  `!` awaiting permission), persistent layout. Windows-only.
