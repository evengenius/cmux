# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). All dates UTC.
Cmux is pre-1.0 so every release is on `master`; this file rolls up the
git log into reader-friendly chunks.

## [Unreleased]

### UX / UI polish (review pass)

- **Width-aware `truncate`.** Project / chat / session labels now budget
  by terminal *display columns* instead of `chars().count()`. CJK,
  emoji, and other wide glyphs no longer push siblings off-screen.
- **Unified focus / modal border colour.** Sidebars (Sessions / Files /
  Commands) and modals (Browse, Help, Palette, Save-as, Rename,
  Broadcast, Diff, Usage, Global sessions) all route through
  `theme::focus_border(focused)`. Focused = configured accent;
  unfocused = `border_inactive`. Siblings differentiate via title text
  only.
- **Light-mode + auto-detect theme.** New `[theme] mode = "dark" |
  "light" | "auto"`. `auto` inspects `COLORFGBG` (xterm / rxvt). Every
  ratatui call site reads from a centralised `Theme` struct, so all
  panel / input / chrome / button / hint / body colours follow.
- **ASCII-icon fallback.** New `[theme] ascii_icons = true` swaps `📌`
  for `[*]` and `🔍` for `?` — for SSH-into-old-server, LANG=C, fonts
  without emoji.
- **AwaitingPermission badge no longer blinks.** Switched to a static
  white-on-red bold pill so terminals that strip `BLINK` (gnome-
  terminal, kitty, alacritty, vscode-terminal) and a11y setups still
  surface the most important signal in the TUI.
- **Unified hint-bar in every modal.** New `hint_line()` helper plus a
  fresh hint row in the F9 palette (`Enter run · ↑/↓ navigate · type
  to filter · Ctrl+U clear · Esc close`).
- **Grouped command palette.** Every action carries a category prefix
  — `[Tab]`, `[Claude]`, `[Project]`, `[View]`, `[Layout]`, `[App]` —
  visible AND in the search haystack. Type "view" to filter by
  category.
- **Confirm before broadcasting to ≥2 chats.** Enter in the broadcast
  modal now pops a Y/N confirmation when the project has more than one
  chat; single-chat case is unchanged.
- **Usage modal stops dismissing on any keypress.** Only Esc / Enter /
  q close it; typing into the modal no longer leaks to the underlying
  PTY.
- **Save-as error strip.** Validation failures show as a high-contrast
  white-on-red bold strip instead of plain red text on the panel.
- **Scrollbar hidden on short PTYs.** Below 4 rows the ▲ / ▼ caps
  consumed the whole bar; PTY now reclaims the column.
- **README FAQ + action_names reference.** New troubleshooting section
  covering `claude` not on PATH, box-drawing on legacy fonts, light
  terminal contrast, OS notification setup per platform, shell
  follow_tab_cwd, worktree fallback, tmux/mouse conflict. The `[keys]`
  example now enumerates every bindable action name.

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
