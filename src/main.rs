mod browser;
mod clipboard;
mod commands;
mod config;
mod grep;
mod layout;
mod notify;
mod scrollback_text;
mod sessions;
mod shell;
mod worktree;

use commands::{CommandEntry, CommandSource};
use scrollback_text::ScrollbackText;

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Terminal,
};
use tui_term::widget::PseudoTerminal;
use vt100::{MouseProtocolMode, Parser};

use browser::{Entry as BrowserEntry, FileBrowser};
use grep::{GrepHit, GrepJob};
use sessions::SessionMeta;

/// Total non-PTY rows at the top + bottom of the screen:
///   row 0 — project bar (one strip per unique cwd among open tabs)
///   row 1 — chat bar (tabs filtered to the active project)
///   row last — button bar
const CHROME_ROWS: u16 = 3;
const SCROLLBACK_LINES: usize = 10_000;
/// Short model names shown in the palette for "New tab with model: …".
/// Claude Code CLI's `--model` accepts these; if the user runs an unsupported
/// model on their plan claude itself will complain.
const NEW_TAB_MODELS: &[&str] = &["opus", "sonnet", "haiku"];
/// Max gap between two left-button-down events on the same chat that still
/// counts as a double-click (close-the-tab gesture).
const DOUBLE_CLICK_MS: u128 = 500;
const SCROLL_STEP: usize = 3;
// Defaults live in config::LayoutConfig::default; overridable via ~/.cmux/config.toml

// ===========================================================================
// Action — anything that can be triggered by either keyboard or mouse.
// ===========================================================================
#[derive(Copy, Clone)]
enum Action {
    NewTab,
    CloseTab,
    PrevTab,
    NextTab,
    SwitchTab(usize),
    ToggleSidebar,
    ToggleFilesSidebar,
    ToggleDeepGrep,
    ToggleMouse,
    TogglePalette,
    ToggleBrowser,
    ToggleBottom,
    ToggleCommands,
    ToggleHelp,
    ToggleSearch,
    RenameActiveTab,
    RestoreLayout,
    OpenSaveLayoutAs,
    ToggleGlobalSessions,
    ToggleActivePin,
    PrevProject,
    NextProject,
    SwitchProject(usize),
    /// Open the file browser pinned to the active project's cwd; the user
    /// picks a directory and Enter on "OpenHere" spawns a chat there.
    OpenBrowserForNewProject,
    ToggleActiveProjectPin,
    /// Spawn a new tab in the active project's cwd with `claude --continue`.
    NewTabContinue,
    /// Spawn a new tab passing `--model <name>` to claude. usize indexes
    /// into `NEW_TAB_MODELS`.
    NewTabWithModel(usize),
    /// Open the broadcast modal — Enter sends the typed prompt to every
    /// chat in the active project.
    OpenBroadcast,
    /// Read the active chat's jsonl and pop a modal with token totals.
    ShowActiveUsage,
    /// Open the `git diff` viewer modal for the active tab's cwd.
    ShowGitDiff,
    /// Send `/clear<Enter>` to the active chat — same as typing the slash
    /// command manually, but a single Ctrl+L stroke.
    ClearActiveChat,
    /// Copy the active chat's full scrollback (plain-text mirror) to the
    /// system clipboard. Reports success / failure via the usage modal.
    CopyChatScrollback,
    /// Copy the last assistant message in the active chat's jsonl to the
    /// system clipboard.
    CopyLastResponse,
    /// Pop the most recently closed chat off the undo stack and respawn it.
    ReopenLastClosed,
    /// Insert snippet number `n` (index into the sorted snippets list) into
    /// the active chat's input. No trailing `\r` — the user submits.
    /// Insert snippet by `snippet_*` index into `App.snippet_keys` — a
    /// cached, sorted copy of the config snippet names. Resolving by name
    /// (not raw position) keeps the binding stable across config reloads.
    InsertSnippet(usize),
    /// Export a Markdown summary of the active chat's session to
    /// `<cwd>/sessions/<ts>-<slug>.md`. Inspired by iannuttall/claude-sessions.
    ExportSessionNote,
    ReloadConfig,
    Quit,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum SidebarMode {
    Sessions,
    Commands,
}

const CLAUDE_COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show available slash commands"),
    ("/clear", "Clear the conversation history"),
    ("/compact", "Compact the conversation context"),
    ("/cost", "Show token cost of the current session"),
    ("/status", "Show account / connection status"),
    ("/config", "Open the configuration menu"),
    ("/init", "Create a CLAUDE.md project file"),
    ("/login", "Sign in to Claude"),
    ("/logout", "Sign out"),
    ("/model", "Switch the active model"),
    ("/permissions", "Manage tool permissions"),
    ("/resume", "Pick a previous session to resume"),
    ("/ide", "Connect to an IDE (VS Code, JetBrains)"),
    ("/mcp", "Manage MCP servers"),
    ("/agents", "List or invoke custom subagents"),
    ("/memory", "Manage memory files (CLAUDE.md and friends)"),
    ("/add-dir", "Add a working directory to the session"),
    ("/upgrade", "Upgrade your Claude subscription"),
    ("/release-notes", "Show release notes"),
    ("/feedback", "Submit feedback"),
    ("/bug", "Report a bug"),
    ("/doctor", "Diagnose installation"),
    ("/vim", "Toggle vim editing mode"),
    ("/review", "Review the current pull request"),
    ("/security-review", "Run a security review on pending changes"),
    ("/terminal-setup", "Configure the terminal"),
    ("/migrate-installer", "Migrate to the new installer"),
    ("/install-github-app", "Install the Claude GitHub app"),
    ("/pr-comments", "Show pull-request comments"),
    ("/exit", "Exit the CLI"),
    ("/quit", "Exit the CLI"),
];

#[derive(Copy, Clone, PartialEq, Eq)]
enum ResizeDrag {
    None,
    Sidebar,
    RightSidebar,
    Bottom,
    /// Dragging the vertical scrollbar thumb of the active chat. Position is
    /// computed from the mouse row at every event.
    Scrollbar,
}

struct ButtonHit {
    rect: Rect,
    action: Action,
}

// ===========================================================================
// TabState — what's happening inside the embedded claude right now.
// ===========================================================================
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum TabState {
    Idle,
    Streaming,
    AwaitingPermission,
}

const STREAMING_QUIET_MS: u128 = 800;

// ===========================================================================
// Command palette items
// ===========================================================================
enum PaletteKind {
    Action(Action),
    Session(usize),        // index into App.sessions
    LayoutSwitch(usize),   // index into App.layout_names
    LayoutDelete(usize),   // index into App.layout_names
}

struct PaletteItem {
    label: String,
    hay: String, // lowercased haystack for matching
    kind: PaletteKind,
}

enum PaletteResult {
    None,
    Run(Action),
    OpenSession(usize),
    SwitchLayout(usize),
    DeleteLayout(usize),
}

// ===========================================================================
// Recently-closed chat (Ctrl+Shift+T reopens) — snapshot of just enough to
// respawn the same session.
// ===========================================================================
#[derive(Clone)]
struct ClosedTab {
    cwd: PathBuf,
    session_id: Option<String>,
    title: String,
    pinned: bool,
    /// Worktree info, copied so reopen can rebind the chat. If the worktree
    /// was removed on close (config + not pinned), this still points at a
    /// now-non-existent dir — reopen detects that and falls back to the
    /// repo root.
    worktree_owned: Option<(PathBuf, PathBuf)>,
}

const RECENTLY_CLOSED_CAP: usize = 10;

// ===========================================================================
// Broadcast modal — type once, submit to every chat in the active project.
// ===========================================================================
#[derive(Default)]
struct BroadcastState {
    open: bool,
    /// Text the user is typing. Submit appends `\r` per recipient so claude
    /// treats it as one submitted prompt.
    input: String,
}

impl BroadcastState {
    fn clear(&mut self) {
        self.open = false;
        self.input.clear();
    }
}

// ===========================================================================
// Confirm modal — small Y/N prompt used before destructive actions
// (closing a pinned chat or closing a whole project).
// ===========================================================================
#[derive(Clone)]
enum PendingConfirm {
    /// Unpin then close the chat at this global index.
    UnpinAndCloseChat(usize),
    /// Close every chat whose cwd equals this path.
    CloseProject(PathBuf),
}

#[derive(Default)]
struct ConfirmState {
    open: bool,
    message: String,
    pending: Option<PendingConfirm>,
}

impl ConfirmState {
    fn clear(&mut self) {
        self.open = false;
        self.message.clear();
        self.pending = None;
    }
}

// ===========================================================================
// Global sessions modal — full directory listing grouped by cwd.
// ===========================================================================
enum GlobalEntry {
    /// Section header — printed for the cwd that the following sessions
    /// share. Not selectable.
    Header(String),
    /// Pointer back to `App.sessions[idx]`. Selectable.
    Session(usize),
}

#[derive(Default)]
struct GlobalSessionsState {
    open: bool,
    filter: String,
    entries: Vec<GlobalEntry>,
    idx: usize,
    scroll: usize,
}

impl GlobalSessionsState {
    fn clear(&mut self) {
        self.open = false;
        self.filter.clear();
        self.entries.clear();
        self.idx = 0;
        self.scroll = 0;
    }

    fn is_selectable(&self, idx: usize) -> bool {
        matches!(self.entries.get(idx), Some(GlobalEntry::Session(_)))
    }

    fn step(&mut self, dir: isize) {
        if self.entries.is_empty() {
            self.idx = 0;
            return;
        }
        let n = self.entries.len();
        let mut cur = self.idx as isize;
        // Step at least once, then keep stepping past headers in the same
        // direction. Wrap to keep navigation forgiving.
        for _ in 0..n {
            cur += dir;
            if cur < 0 {
                cur = n as isize - 1;
            }
            if cur >= n as isize {
                cur = 0;
            }
            if self.is_selectable(cur as usize) {
                self.idx = cur as usize;
                return;
            }
        }
    }
}

// ===========================================================================
// Scrollback search state (Ctrl+F)
// ===========================================================================
#[derive(Default)]
struct SearchState {
    open: bool,
    query: String,
    /// True when `query` is interpreted as a regex. Toggled with Alt+R.
    regex_mode: bool,
    /// Last compile error for the regex query — shown in the overlay so the
    /// user can fix the pattern instead of guessing why nothing matches.
    regex_error: Option<String>,
    matches: Vec<scrollback_text::Match>,
    /// Index into `matches` of the currently focused hit.
    idx: usize,
    /// Tab the search applies to. Closing the search clears this; switching
    /// tabs while open also resets matches.
    tab_idx: Option<usize>,
}

impl SearchState {
    fn clear(&mut self) {
        self.open = false;
        self.query.clear();
        self.regex_error = None;
        self.matches.clear();
        self.idx = 0;
        self.tab_idx = None;
        // regex_mode is sticky — preserve user preference across opens.
    }
}

// ===========================================================================
// ChatTab
// ===========================================================================
struct ChatTab {
    title: String,
    cwd: PathBuf,
    session_id: Option<String>,
    created_at_unix: u64,
    parser: Arc<Mutex<Parser>>,
    dirty: Arc<AtomicBool>,
    last_activity: Arc<Mutex<Instant>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Plain-text mirror of the PTY output, fed alongside the vt100 parser.
    /// Used by the Ctrl+F scrollback search.
    text_buffer: Arc<Mutex<ScrollbackText>>,
    /// Mirror of `text_buffer.total_lines()` that the reader thread updates
    /// after every feed. Read on the hot scrollbar render path so we don't
    /// have to take the text_buffer mutex on every frame.
    total_lines: Arc<AtomicUsize>,
    /// Scrollback row cap this tab was spawned with — used to clamp the
    /// scrollbar drag math and the search jump offset.
    scrollback_max: usize,
    /// Latest cwd seen in an OSC 7 sequence from the PTY. The main loop
    /// drains it on every tick and updates `cwd` accordingly so the
    /// project grouping follows shells that `cd`.
    pending_cwd: Arc<Mutex<Option<PathBuf>>>,
    /// Completed claude turns the user hasn't seen yet — incremented on every
    /// Streaming → Idle transition that happens while the tab isn't active,
    /// reset on focus. Rendered as a `[N]` badge in the chat bar. This is a
    /// better signal than a raw byte/line count: a streaming tab can produce
    /// thousands of bytes within a single reply, but only one "completion".
    unread_replies: usize,
    /// User-pinned: F8 / Alt+W refuse to close, drag-to-reorder still works.
    /// Persisted in `SavedTab`.
    pinned: bool,
    /// (repo_root, worktree_path) when this chat was spawned into a fresh
    /// `git worktree`. `close_active` uses these to call `git worktree
    /// remove` if config asks for it. `None` for chats spawned in a plain
    /// cwd (no repo, or auto_worktree off).
    worktree_owned: Option<(PathBuf, PathBuf)>,
    /// True while the tab has already fired its AwaitingPermission notification
    /// for the current "stuck" period. Cleared once state leaves Awaiting so
    /// the next transition fires again.
    notified_awaiting: bool,
}

impl ChatTab {
    fn spawn(cwd: PathBuf, rows: u16, cols: u16, scrollback: usize) -> Result<Self> {
        let title = cwd
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("claude")
            .to_string();
        Self::spawn_inner(cwd, &[], title, None, rows, cols, scrollback)
    }

    /// Spawn a fresh (non-resumed) tab with an explicit title. Used by
    /// layout restore so previously-renamed tabs keep their names instead of
    /// snapping back to the cwd basename.
    fn spawn_with_title(
        cwd: PathBuf,
        title: String,
        rows: u16,
        cols: u16,
        scrollback: usize,
    ) -> Result<Self> {
        Self::spawn_inner(cwd, &[], title, None, rows, cols, scrollback)
    }

    fn spawn_resume(
        cwd: PathBuf,
        session_id: &str,
        title: String,
        rows: u16,
        cols: u16,
        scrollback: usize,
    ) -> Result<Self> {
        Self::spawn_inner(
            cwd,
            &["--resume", session_id],
            title,
            Some(session_id.to_string()),
            rows,
            cols,
            scrollback,
        )
    }

    fn spawn_inner(
        cwd: PathBuf,
        extra_args: &[&str],
        title: String,
        session_id: Option<String>,
        rows: u16,
        cols: u16,
        scrollback: usize,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // Windows ships claude as `claude.cmd` and refuses to spawn .cmd
        // files directly — we go through cmd.exe. Other platforms have a
        // plain `claude` binary on PATH.
        let mut cmd = if cfg!(windows) {
            let mut c = CommandBuilder::new("cmd.exe");
            let mut argv: Vec<&str> = vec!["/c", "claude.cmd"];
            argv.extend_from_slice(extra_args);
            c.args(argv);
            c
        } else {
            let mut c = CommandBuilder::new("claude");
            for a in extra_args {
                c.arg(a);
            }
            c
        };
        cmd.cwd(&cwd);
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        // Clamp scrollback so weird config values can't OOM or zero out the
        // buffer. `vt100::Parser` requires `scrollback >= 1`.
        let scrollback_cap = scrollback.clamp(64, 1_000_000);
        let parser = Arc::new(Mutex::new(Parser::new(rows, cols, scrollback_cap)));
        let dirty = Arc::new(AtomicBool::new(true));
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        let text_buffer = Arc::new(Mutex::new(ScrollbackText::with_capacity(scrollback_cap)));
        let total_lines = Arc::new(AtomicUsize::new(1));
        let pending_cwd: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let mut reader = pair.master.try_clone_reader()?;
        let parser_for_reader = parser.clone();
        let dirty_for_reader = dirty.clone();
        let activity_for_reader = last_activity.clone();
        let text_for_reader = text_buffer.clone();
        let total_for_reader = total_lines.clone();
        let pending_cwd_for_reader = pending_cwd.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut p) = parser_for_reader.lock() {
                            p.process(&buf[..n]);
                        }
                        if let Ok(mut t) = text_for_reader.lock() {
                            t.feed(&buf[..n]);
                            // Publish total so the main thread can size the
                            // scrollbar without locking text_buffer.
                            total_for_reader.store(t.total_lines(), Ordering::Relaxed);
                        }
                        // OSC 7 cwd hint — shells emit `\x1b]7;file://...\x07`
                        // after every cd. Publish the latest into pending_cwd;
                        // the main loop applies it and re-buckets the project.
                        if let Some(p) = extract_osc7_path(&buf[..n]) {
                            if let Ok(mut slot) = pending_cwd_for_reader.lock() {
                                *slot = Some(p);
                            }
                        }
                        dirty_for_reader.store(true, Ordering::Release);
                        if let Ok(mut t) = activity_for_reader.lock() {
                            *t = Instant::now();
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let writer = pair.master.take_writer()?;

        Ok(Self {
            title,
            cwd,
            session_id,
            created_at_unix: layout::now_unix(),
            parser,
            dirty,
            last_activity,
            master: pair.master,
            writer,
            child,
            text_buffer,
            total_lines,
            scrollback_max: scrollback_cap,
            pending_cwd,
            worktree_owned: None,
            unread_replies: 0,
            pinned: false,
            notified_awaiting: false,
        })
    }

    /// Inspect screen + recent output to classify what the tab is doing.
    /// Lock order (parser → last_activity) matches the reader thread.
    fn compute_state(&self, now: Instant, permission_patterns: &[String]) -> TabState {
        let text = {
            let p = self.parser.lock().unwrap_or_else(|p| p.into_inner());
            p.screen().contents().to_lowercase()
        };
        // permission prompts beat freshness — even if there's new output,
        // the important state is "waiting for the user".
        if permission_patterns.iter().any(|pat| text.contains(pat)) {
            return TabState::AwaitingPermission;
        }
        let last = *self.last_activity.lock().unwrap_or_else(|p| p.into_inner());
        if now.duration_since(last).as_millis() < STREAMING_QUIET_MS {
            return TabState::Streaming;
        }
        TabState::Idle
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser.lock().unwrap_or_else(|p| p.into_inner()).set_size(rows, cols);
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    fn scroll_up(&self) {
        let mut p = self.parser.lock().unwrap_or_else(|p| p.into_inner());
        let cur = p.screen().scrollback();
        let next = (cur + SCROLL_STEP).min(self.scrollback_max);
        p.set_scrollback(next);
        self.dirty.store(true, Ordering::Release);
    }

    fn scroll_down(&self) {
        let mut p = self.parser.lock().unwrap_or_else(|p| p.into_inner());
        let cur = p.screen().scrollback();
        let next = cur.saturating_sub(SCROLL_STEP);
        p.set_scrollback(next);
        self.dirty.store(true, Ordering::Release);
    }

    fn scroll_reset(&self) {
        let mut p = self.parser.lock().unwrap_or_else(|p| p.into_inner());
        if p.screen().scrollback() != 0 {
            p.set_scrollback(0);
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn is_dead(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
    }

    fn mouse_enabled(&self) -> bool {
        let p = self.parser.lock().unwrap_or_else(|p| p.into_inner());
        !matches!(p.screen().mouse_protocol_mode(), MouseProtocolMode::None)
    }
}

// ===========================================================================
// BottomTerminal — embedded parent-shell PTY shown at the bottom of the screen.
// ===========================================================================
struct BottomTerminal {
    parser: Arc<Mutex<Parser>>,
    dirty: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    shell_label: String,
    /// Last cwd we sent a `cd` for — used to dedupe auto-follow on tab switch.
    last_cwd_sent: Option<PathBuf>,
}

impl BottomTerminal {
    fn spawn(
        rows: u16,
        cols: u16,
        shell_override: Option<(String, Vec<String>)>,
        cwd: PathBuf,
    ) -> Result<Self> {
        let (exe, args) = shell_override.unwrap_or_else(shell::detect_parent_shell);

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&exe);
        for a in &args {
            cmd.arg(a);
        }
        cmd.cwd(&cwd);
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(Parser::new(rows, cols, SCROLLBACK_LINES)));
        let dirty = Arc::new(AtomicBool::new(true));

        let mut reader = pair.master.try_clone_reader()?;
        let parser_for_reader = parser.clone();
        let dirty_for_reader = dirty.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut p) = parser_for_reader.lock() {
                            p.process(&buf[..n]);
                        }
                        dirty_for_reader.store(true, Ordering::Release);
                    }
                    Err(_) => break,
                }
            }
        });

        let writer = pair.master.take_writer()?;

        Ok(Self {
            parser,
            dirty,
            master: pair.master,
            writer,
            child,
            shell_label: shell::short_name(&exe),
            last_cwd_sent: Some(cwd),
        })
    }

    /// Inject a `cd <path>` line into the shell if the path differs from the
    /// last cd we sent. No-op for unrecognised shells.
    fn cd_to(&mut self, path: &Path) -> Result<()> {
        if self.last_cwd_sent.as_deref() == Some(path) {
            return Ok(());
        }
        if let Some(bytes) = shell::cd_command(&self.shell_label, path) {
            self.write_input(&bytes)?;
            self.last_cwd_sent = Some(path.to_path_buf());
        }
        Ok(())
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser.lock().unwrap_or_else(|p| p.into_inner()).set_size(rows, cols);
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

// ===========================================================================
// App
// ===========================================================================
struct App {
    tabs: Vec<ChatTab>,
    active: usize,
    cwd: PathBuf,
    /// Mouse-hit map for the chat bar: each entry is `(rect, global tab idx)`.
    /// Rebuilt every draw; only contains chats of the currently active project.
    chat_rects: Vec<(Rect, usize)>,
    new_tab_rect: Option<Rect>,
    /// Mouse-hit map for the project bar: each entry is `(rect, project idx
    /// into `projects()`)`. Rebuilt every draw.
    project_rects: Vec<(Rect, usize)>,
    /// Click rect for the `+` button on the project bar — opens the file
    /// browser so the user can pick a directory for a new project.
    new_project_rect: Option<Rect>,
    /// cwds of projects the user has pinned. Pinned projects show 📌 on the
    /// project bar and require confirmation to close.
    pinned_projects: std::collections::HashSet<PathBuf>,
    /// LIFO stack of recently closed chats. `close_active` pushes onto it;
    /// `Action::ReopenLastClosed` pops and respawns. Capped at 10 entries.
    recently_closed: std::collections::VecDeque<ClosedTab>,
    /// Last left-button-down on a project entry — same role as
    /// `last_chat_click` but for the project bar, used to detect
    /// "double-click close" on a project.
    last_project_click: Option<(Instant, PathBuf)>,
    // sessions sidebar
    sessions: Vec<SessionMeta>,
    sidebar_open: bool,
    sidebar_focused: bool,
    sidebar_mode: SidebarMode,
    sidebar_idx: usize,
    sidebar_scroll: usize,
    filter: String,
    /// Per-mode filter memos — preserved across F3↔F4 switches and close/open
    /// cycles, so re-entering a sidebar lands you back on the same query.
    sessions_filter_memo: String,
    commands_filter_memo: String,
    filtered: Vec<usize>,
    deep_grep: bool,
    grep_job: Option<GrepJob>,
    grep_hits: Vec<GrepHit>,
    // chrome / input modes
    mouse_on: bool,
    mouse_capture_dirty: bool,
    button_hits: Vec<ButtonHit>,
    last_states: Vec<TabState>,
    // command palette
    palette_open: bool,
    palette_query: String,
    palette_items: Vec<PaletteItem>,
    palette_filtered: Vec<usize>,
    palette_idx: usize,
    // file browser (modal F6)
    browser_open: bool,
    browser: Option<FileBrowser>,
    // right files sidebar (Ctrl+B)
    right_sidebar_open: bool,
    right_sidebar_focused: bool,
    right_sidebar_area: Rect,
    // bottom terminal pane
    bottom: Option<BottomTerminal>,
    bottom_open: bool,
    bottom_focused: bool,
    bottom_area: Rect,
    // remembered geometry for hit-testing
    sidebar_area: Rect,
    body_bottom_y: u16,
    // mouse drag state
    resize_drag: ResizeDrag,
    /// Index of the tab whose mouse-down started a (potential) drag-to-reorder.
    /// `None` while no drag is in progress.
    tab_drag_from: Option<usize>,
    /// Last left-button-down on a chat tab — used for double-click detection.
    /// A second click on the same chat within DOUBLE_CLICK_MS closes it.
    last_chat_click: Option<(Instant, usize)>,
    // commands sidebar
    commands_list: Vec<CommandEntry>,
    commands_filtered: Vec<usize>,
    // help overlay
    help_open: bool,
    // scrollback search (Ctrl+F)
    search: SearchState,
    // global sessions modal (Shift+F3)
    global_sessions: GlobalSessionsState,
    // tab-rename modal (Shift+F2)
    rename_open: bool,
    rename_input: String,
    // save-layout-as modal (via palette)
    save_as_open: bool,
    save_as_input: String,
    save_as_error: Option<String>,
    // confirm modal for destructive actions
    confirm: ConfirmState,
    // broadcast modal — send one prompt to every chat in active project
    broadcast: BroadcastState,
    // usage modal — shows token totals for the active chat
    usage_open: bool,
    usage_lines: Vec<String>,
    // git diff modal — read-only viewer for `git diff` of active tab's cwd
    diff_open: bool,
    diff_lines: Vec<String>,
    diff_scroll: usize,
    diff_title: String,
    // named layouts in ~/.cmux/layouts/, refreshed on palette open
    layout_names: Vec<String>,
    // persisted layout (if any was found at startup)
    saved_layout: Option<layout::SavedLayout>,
    // user config
    config: config::Config,
    // user-defined keymap from `[keys]` section, rebuilt on reload.
    key_bindings: KeyBindings,
    /// Snapshot of snippet names taken when the palette is built. Used to
    /// resolve `InsertSnippet(i)` so a config reload between palette-open
    /// and item-select doesn't shift indices under us.
    snippet_keys: Vec<String>,
}

impl App {
    fn new(cwd: PathBuf, rows: u16, cols: u16) -> Result<Self> {
        let config = config::load();
        let scrollback = config.layout.scrollback_lines;
        let tab = ChatTab::spawn(cwd.clone(), rows, cols, scrollback)?;
        Ok(Self {
            tabs: vec![tab],
            active: 0,
            cwd,
            chat_rects: Vec::new(),
            new_tab_rect: None,
            project_rects: Vec::new(),
            new_project_rect: None,
            pinned_projects: std::collections::HashSet::new(),
            recently_closed: std::collections::VecDeque::with_capacity(10),
            last_project_click: None,
            sessions: Vec::new(),
            sidebar_open: false,
            sidebar_focused: false,
            sidebar_mode: SidebarMode::Sessions,
            sidebar_idx: 0,
            sidebar_scroll: 0,
            filter: String::new(),
            sessions_filter_memo: String::new(),
            commands_filter_memo: String::new(),
            filtered: Vec::new(),
            deep_grep: false,
            grep_job: None,
            grep_hits: Vec::new(),
            mouse_on: true,
            mouse_capture_dirty: false,
            button_hits: Vec::new(),
            last_states: Vec::new(),
            palette_open: false,
            palette_query: String::new(),
            palette_items: Vec::new(),
            palette_filtered: Vec::new(),
            palette_idx: 0,
            browser_open: false,
            browser: None,
            right_sidebar_open: false,
            right_sidebar_focused: false,
            right_sidebar_area: Rect::default(),
            bottom: None,
            bottom_open: false,
            bottom_focused: false,
            bottom_area: Rect::default(),
            sidebar_area: Rect::default(),
            body_bottom_y: 0,
            resize_drag: ResizeDrag::None,
            tab_drag_from: None,
            last_chat_click: None,
            commands_list: Vec::new(),
            commands_filtered: Vec::new(),
            help_open: false,
            search: SearchState::default(),
            global_sessions: GlobalSessionsState::default(),
            rename_open: false,
            rename_input: String::new(),
            save_as_open: false,
            save_as_input: String::new(),
            save_as_error: None,
            confirm: ConfirmState::default(),
            broadcast: BroadcastState::default(),
            usage_open: false,
            usage_lines: Vec::new(),
            diff_open: false,
            diff_lines: Vec::new(),
            diff_scroll: 0,
            diff_title: String::new(),
            layout_names: Vec::new(),
            saved_layout: layout::load(),
            key_bindings: KeyBindings::from_config(&config.keys),
            snippet_keys: Vec::new(),
            config,
        })
    }

    fn active_tab(&mut self) -> &mut ChatTab {
        &mut self.tabs[self.active]
    }

    // -- Project grouping --------------------------------------------------
    //
    // A "project" is a unique cwd that one or more tabs share. The project
    // list is recomputed on demand from the current tab list, preserving
    // first-occurrence order so the bar doesn't jump around as tabs are
    // opened/closed.

    /// Distinct cwds in the order they first appear in `self.tabs`.
    fn projects(&self) -> Vec<PathBuf> {
        let mut seen: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::with_capacity(self.tabs.len());
        let mut out = Vec::with_capacity(self.tabs.len());
        for t in &self.tabs {
            if seen.insert(t.cwd.clone()) {
                out.push(t.cwd.clone());
            }
        }
        out
    }

    /// cwd of the active tab — i.e., the active project. Falls back to the
    /// launch cwd if there are somehow no tabs.
    fn active_project_cwd(&self) -> PathBuf {
        self.tabs
            .get(self.active)
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone())
    }

    /// Indices into `self.tabs` for the chats that belong to the active
    /// project (same cwd as active tab). Order matches `self.tabs`.
    fn chats_in_active_project(&self) -> Vec<usize> {
        let proj = self.active_project_cwd();
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(i, t)| if t.cwd == proj { Some(i) } else { None })
            .collect()
    }

    /// Index of the active project in `projects()` order.
    fn active_project_idx(&self) -> usize {
        let proj = self.active_project_cwd();
        self.projects()
            .iter()
            .position(|p| *p == proj)
            .unwrap_or(0)
    }

    fn open_tab(&mut self, rows: u16, cols: u16) -> Result<()> {
        // Spawn the new tab in the active tab's cwd — this is what the
        // user usually means by F2 ("another chat in this project"). Falls
        // back to the launch cwd if there are no tabs (shouldn't happen).
        let base_cwd = self
            .tabs
            .get(self.active)
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let scrollback = self.config.layout.scrollback_lines;
        let (cwd, worktree_owned) = self.resolve_spawn_cwd(&base_cwd);
        let mut tab = ChatTab::spawn(cwd, rows, cols, scrollback)?;
        tab.worktree_owned = worktree_owned;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    /// If `[git] auto_worktree = true` and `base` is inside a git repo,
    /// create a fresh sibling worktree on a new branch and return that path
    /// plus the (repo_root, worktree_path) bookkeeping so close-time cleanup
    /// can `git worktree remove` it. Falls back to (base, None) silently on
    /// any failure — worktrees are a convenience, not a hard requirement.
    fn resolve_spawn_cwd(
        &self,
        base: &Path,
    ) -> (PathBuf, Option<(PathBuf, PathBuf)>) {
        if !self.config.git.auto_worktree {
            return (base.to_path_buf(), None);
        }
        let Some(repo) = worktree::repo_root(base) else {
            return (base.to_path_buf(), None);
        };
        let slug = base
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("chat");
        let branch = worktree::pick_branch(&repo, &self.config.git.branch_prefix, slug);
        // Worktree dir: <root>/<slug>-<unix-nanos>. Nanos so two F2 presses
        // in the same second can't collide on disk.
        let leaf = format!(
            "{}-{}",
            slug,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let root = if Path::new(&self.config.git.worktree_root).is_absolute() {
            PathBuf::from(&self.config.git.worktree_root)
        } else {
            repo.join(&self.config.git.worktree_root)
        };
        let target = root.join(&leaf);
        match worktree::create(&repo, &target, &branch) {
            Ok(p) => (p.clone(), Some((repo, p))),
            Err(_) => (base.to_path_buf(), None), // log via diagnostics later
        }
    }

    fn close_active(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }
        // Pinned tabs refuse to close — unpin via the palette first.
        if self.tabs.get(self.active).map(|t| t.pinned).unwrap_or(false) {
            return false;
        }
        // Snapshot for the undo stack BEFORE we drop the tab.
        let snapshot = self.tabs.get(self.active).map(|t| ClosedTab {
            cwd: t.cwd.clone(),
            session_id: t.session_id.clone(),
            title: t.title.clone(),
            pinned: t.pinned,
            worktree_owned: t.worktree_owned.clone(),
        });
        if let Some(snap) = snapshot {
            if self.recently_closed.len() == RECENTLY_CLOSED_CAP {
                self.recently_closed.pop_front();
            }
            self.recently_closed.push_back(snap);
        }
        let mut t = self.tabs.remove(self.active);
        // Tear down the worktree if cmux created it AND config asks for it.
        self.maybe_remove_worktree(&t);
        t.kill();
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        true
    }

    /// Run `git worktree remove --force` for a closed tab if (a) cmux owned
    /// the worktree, (b) config asks for removal, and (c) the tab wasn't
    /// pinned. Spawned in a detached thread so the TUI never blocks on git
    /// (a big worktree with untracked files can take seconds to remove,
    /// and `close_project` may close N tabs at once).
    fn maybe_remove_worktree(&self, tab: &ChatTab) {
        if !self.config.git.remove_on_close || tab.pinned {
            return;
        }
        let Some((repo, wt)) = tab.worktree_owned.clone() else {
            return;
        };
        std::thread::spawn(move || {
            let _ = worktree::remove(&repo, &wt);
        });
    }

    /// Pop the most-recently-closed chat and spawn it back. Falls back to
    /// `spawn_with_title` when no session_id is recorded. Inserts next to
    /// the last sibling chat (same cwd) so the project grouping isn't
    /// broken — a freshly reopened cmux chat slots back into its project
    /// instead of dangling at the end.
    fn reopen_last_closed(&mut self, rows: u16, cols: u16) -> Result<bool> {
        let Some(mut c) = self.recently_closed.pop_back() else {
            return Ok(false);
        };
        let scrollback = self.config.layout.scrollback_lines;
        // If the chat lived in a cmux-owned worktree that has since been
        // removed, fall back to the repo root so claude doesn't spawn in a
        // ghost directory.
        if let Some((repo, wt)) = &c.worktree_owned {
            if !wt.exists() {
                c.cwd = repo.clone();
                c.worktree_owned = None;
            }
        }
        // Title was already truncated when the snapshot was taken; don't
        // re-truncate (avoid ellipsis-on-ellipsis cosmetic bug).
        let title = c.title.clone();
        let mut tab = match &c.session_id {
            Some(id) => ChatTab::spawn_resume(
                c.cwd.clone(),
                id,
                title,
                rows,
                cols,
                scrollback,
            )?,
            None => ChatTab::spawn_with_title(
                c.cwd.clone(),
                title,
                rows,
                cols,
                scrollback,
            )?,
        };
        tab.pinned = c.pinned;
        tab.worktree_owned = c.worktree_owned.clone();
        // Find the index of the LAST sibling sharing this cwd and insert
        // right after it. If no sibling exists, append (new project).
        let insert_at = self
            .tabs
            .iter()
            .enumerate()
            .rev()
            .find(|(_, t)| t.cwd == c.cwd)
            .map(|(i, _)| i + 1)
            .unwrap_or(self.tabs.len());
        self.tabs.insert(insert_at, tab);
        self.active = insert_at;
        Ok(true)
    }

    fn next_tab(&mut self) {
        // Cycle within the active project's chats only.
        let chats = self.chats_in_active_project();
        if chats.len() <= 1 {
            return;
        }
        let pos = chats.iter().position(|&i| i == self.active).unwrap_or(0);
        let next = chats[(pos + 1) % chats.len()];
        self.active = next;
    }

    fn prev_tab(&mut self) {
        let chats = self.chats_in_active_project();
        if chats.len() <= 1 {
            return;
        }
        let pos = chats.iter().position(|&i| i == self.active).unwrap_or(0);
        let prev = chats[(pos + chats.len() - 1) % chats.len()];
        self.active = prev;
    }

    /// Switch to the Nth chat within the active project (Alt+1..9).
    fn switch(&mut self, idx_in_project: usize) {
        let chats = self.chats_in_active_project();
        if let Some(&global) = chats.get(idx_in_project) {
            self.active = global;
        }
    }

    /// Switch to the first chat of the next project (Ctrl+F12).
    fn next_project(&mut self) {
        let projs = self.projects();
        if projs.len() <= 1 {
            return;
        }
        let pos = self.active_project_idx();
        let target_cwd = &projs[(pos + 1) % projs.len()];
        if let Some(idx) = self.tabs.iter().position(|t| t.cwd == *target_cwd) {
            self.active = idx;
        }
    }

    /// Switch to the first chat of the previous project (Ctrl+F11).
    fn prev_project(&mut self) {
        let projs = self.projects();
        if projs.len() <= 1 {
            return;
        }
        let pos = self.active_project_idx();
        let target_cwd = &projs[(pos + projs.len() - 1) % projs.len()];
        if let Some(idx) = self.tabs.iter().position(|t| t.cwd == *target_cwd) {
            self.active = idx;
        }
    }

    /// Switch to the Nth project (Ctrl+Shift+1..9). N is 0-based here.
    fn switch_project(&mut self, project_idx: usize) {
        let projs = self.projects();
        if let Some(target_cwd) = projs.get(project_idx) {
            if let Some(idx) = self.tabs.iter().position(|t| t.cwd == *target_cwd) {
                self.active = idx;
            }
        }
    }

    fn resize_all(&mut self, rows: u16, cols: u16) -> Result<()> {
        for t in &mut self.tabs {
            t.resize(rows, cols)?;
        }
        Ok(())
    }

    fn cleanup_dead(&mut self) {
        let mut i = 0;
        while i < self.tabs.len() {
            if self.tabs[i].is_dead() {
                self.tabs.remove(i);
                if self.active >= self.tabs.len() && self.active > 0 {
                    self.active -= 1;
                }
            } else {
                i += 1;
            }
        }
    }

    fn kill_all(&mut self) {
        for t in &mut self.tabs {
            t.kill();
        }
        if let Some(b) = self.bottom.as_mut() {
            b.kill();
        }
    }

    fn toggle_sidebar(&mut self) {
        // Same mode → close (regardless of focus). Use mouse to re-focus.
        if self.sidebar_open && self.sidebar_mode == SidebarMode::Sessions {
            self.sidebar_open = false;
            self.sidebar_focused = false;
            return;
        }
        self.swap_filter_for_mode(SidebarMode::Sessions);
        // Always re-enumerate on open so new/closed sessions show up. The
        // scan is cheap (<50ms for hundreds of files) and beats the surprise
        // of "I started another tab two hours ago and it isn't here".
        self.refresh_sessions();
        self.sidebar_open = true;
        self.sidebar_focused = true;
        self.sidebar_idx = 0;
        self.sidebar_scroll = 0;
        self.apply_filter();
    }

    /// Stash the current `filter` into the previous mode's memo and restore
    /// the new mode's memo. Lets the user re-enter F3 / F4 with the query
    /// they had last time.
    fn swap_filter_for_mode(&mut self, new_mode: SidebarMode) {
        match self.sidebar_mode {
            SidebarMode::Sessions => {
                self.sessions_filter_memo = std::mem::take(&mut self.filter);
            }
            SidebarMode::Commands => {
                self.commands_filter_memo = std::mem::take(&mut self.filter);
            }
        }
        self.filter = match new_mode {
            SidebarMode::Sessions => std::mem::take(&mut self.sessions_filter_memo),
            SidebarMode::Commands => std::mem::take(&mut self.commands_filter_memo),
        };
        self.sidebar_mode = new_mode;
    }

    /// Re-scan `~/.claude/projects/` and replace `self.sessions`. Used by F3
    /// open, palette open, and Ctrl+R inside the sessions sidebar.
    fn refresh_sessions(&mut self) {
        let root = sessions::claude_projects_root();
        self.sessions = sessions::enumerate(&root);
    }

    fn toggle_commands_sidebar(&mut self) {
        if self.sidebar_open && self.sidebar_mode == SidebarMode::Commands {
            self.sidebar_open = false;
            self.sidebar_focused = false;
            return;
        }
        self.swap_filter_for_mode(SidebarMode::Commands);
        self.sidebar_open = true;
        self.sidebar_focused = true;
        self.sidebar_idx = 0;
        self.sidebar_scroll = 0;
        self.reload_commands();
        self.apply_commands_filter();
    }

    /// Rebuild `commands_list` from project-local + user-global `.claude/commands/*.md`
    /// plus the hardcoded built-ins. Called on every F4 open so project-local
    /// edits take effect without restart.
    fn reload_commands(&mut self) {
        let cwd = self
            .tabs
            .get(self.active)
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        self.commands_list = commands::load(&cwd, CLAUDE_COMMANDS);
    }

    fn apply_commands_filter(&mut self) {
        let q = self.filter.to_lowercase();
        self.commands_filtered = if q.is_empty() {
            (0..self.commands_list.len()).collect()
        } else {
            self.commands_list
                .iter()
                .enumerate()
                .filter_map(|(i, e)| {
                    if e.name.to_lowercase().contains(&q) || e.desc.to_lowercase().contains(&q) {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect()
        };
        if self.sidebar_idx >= self.commands_filtered.len() {
            self.sidebar_idx = self.commands_filtered.len().saturating_sub(1);
        }
        self.sidebar_scroll = 0;
    }

    fn toggle_help(&mut self) {
        self.help_open = !self.help_open;
    }

    fn toggle_files_sidebar(&mut self) {
        if self.right_sidebar_open {
            self.right_sidebar_open = false;
            self.right_sidebar_focused = false;
            return;
        }
        self.right_sidebar_open = true;
        self.right_sidebar_focused = true;
        // unfocus left so keys go to right
        self.sidebar_focused = false;
        self.ensure_browser_for_active_tab();
    }

    fn ensure_browser_for_active_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let active_cwd = self.tabs[self.active].cwd.clone();
        let show_hidden = self.config.browser.show_hidden;
        let needs_new = match self.browser.as_ref() {
            None => true,
            Some(b) => b.root.as_deref() != Some(active_cwd.as_path()),
        };
        if needs_new {
            self.browser = Some(FileBrowser::new_chrooted(
                active_cwd.clone(),
                show_hidden,
                active_cwd,
            ));
        }
    }

    fn reload_config(&mut self) {
        self.config = config::load();
        self.key_bindings = KeyBindings::from_config(&self.config.keys);
        if let Some(b) = self.browser.as_mut() {
            b.set_show_hidden(self.config.browser.show_hidden);
        }
    }

    /// Accent colour resolved from config; cached lookups are cheap, no need
    /// to memoise.
    fn accent_color(&self) -> Color {
        parse_accent(&self.config.theme.accent)
    }

    /// Call after any change that may have shifted the active tab.
    fn on_active_changed(&mut self) {
        if self.right_sidebar_open {
            self.ensure_browser_for_active_tab();
        }
        // User has eyes on this tab — clear its unread-replies badge.
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.unread_replies = 0;
        }
        self.follow_bottom_to_active();
        // Search is scoped to a single tab — close it on switch so the user
        // doesn't see results that apply to a tab they're no longer on.
        if self.search.open {
            self.search.clear();
        }
        // The F3 list is scoped to the active tab's cwd hierarchy, so the
        // visible set has to be rebuilt on switch.
        if self.sidebar_open && self.sidebar_mode == SidebarMode::Sessions {
            self.apply_filter();
        }
    }

    // -- tab pinning -------------------------------------------------------

    fn toggle_active_pin(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.pinned = !tab.pinned;
            self.save_layout();
        }
    }

    fn is_project_pinned(&self, cwd: &Path) -> bool {
        self.pinned_projects.contains(cwd)
    }

    fn toggle_active_project_pin(&mut self) {
        let cwd = self.active_project_cwd();
        if self.pinned_projects.contains(&cwd) {
            self.pinned_projects.remove(&cwd);
        } else {
            self.pinned_projects.insert(cwd);
        }
        self.save_layout();
    }

    /// Close every chat whose cwd equals `cwd`. No-op if doing so would empty
    /// the tab list — at least one chat must remain alive. Each closed chat
    /// is pushed onto `recently_closed` (newest of the project ends up at the
    /// top of the stack, so Ctrl+Shift+T undoes them in close order).
    fn close_project(&mut self, cwd: &Path) -> bool {
        let to_close: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(i, t)| if t.cwd == cwd { Some(i) } else { None })
            .collect();
        if to_close.is_empty() || to_close.len() >= self.tabs.len() {
            return false;
        }
        // Snapshot each tab onto the undo stack BEFORE removing.
        for &idx in &to_close {
            if let Some(t) = self.tabs.get(idx) {
                let snap = ClosedTab {
                    cwd: t.cwd.clone(),
                    session_id: t.session_id.clone(),
                    title: t.title.clone(),
                    pinned: t.pinned,
                    worktree_owned: t.worktree_owned.clone(),
                };
                if self.recently_closed.len() == RECENTLY_CLOSED_CAP {
                    self.recently_closed.pop_front();
                }
                self.recently_closed.push_back(snap);
            }
        }
        // Kill from the end so earlier indices stay valid.
        for idx in to_close.into_iter().rev() {
            let mut t = self.tabs.remove(idx);
            self.maybe_remove_worktree(&t);
            t.kill();
            if self.active > idx {
                self.active -= 1;
            } else if self.active == idx && self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            }
        }
        self.pinned_projects.remove(cwd);
        self.save_layout();
        true
    }

    // -- tab rename (Shift+F2) ---------------------------------------------

    fn open_rename(&mut self) {
        if let Some(tab) = self.tabs.get(self.active) {
            self.rename_input = tab.title.clone();
            self.rename_open = true;
        }
    }

    fn close_rename(&mut self) {
        self.rename_open = false;
        self.rename_input.clear();
    }

    /// Apply the current `rename_input` to the active tab. Empty input is
    /// treated as "reset to cwd basename" so users can undo a rename.
    fn apply_rename(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let new_title = if self.rename_input.trim().is_empty() {
                tab.cwd
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("claude")
                    .to_string()
            } else {
                self.rename_input.trim().to_string()
            };
            tab.title = truncate(&new_title, 60);
        }
        self.close_rename();
        self.save_layout();
    }

    // -- global sessions modal (Shift+F3) ----------------------------------

    fn toggle_global_sessions(&mut self) {
        if self.global_sessions.open {
            self.global_sessions.clear();
            return;
        }
        self.refresh_sessions();
        self.global_sessions.filter.clear();
        self.global_sessions.idx = 0;
        self.global_sessions.scroll = 0;
        self.rebuild_global_entries();
        self.global_sessions.open = true;
        // Land on the first selectable row.
        if !self.global_sessions.is_selectable(self.global_sessions.idx) {
            self.global_sessions.step(1);
        }
    }

    fn rebuild_global_entries(&mut self) {
        let q = self.global_sessions.filter.to_lowercase();
        // Indices of sessions that pass the text filter.
        let mut idxs: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                if q.is_empty() {
                    return Some(i);
                }
                let hay = format!(
                    "{}\n{}\n{}\n{}",
                    s.title.to_lowercase(),
                    s.cwd.to_string_lossy().to_lowercase(),
                    s.git_branch.as_deref().unwrap_or("").to_lowercase(),
                    s.project_dir.to_lowercase()
                );
                if hay.contains(&q) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        // Stable sort by (cwd as string, updated desc). The original
        // `self.sessions` is already updated-desc, so a stable sort keyed by
        // cwd alone yields cwd groups with newest-first inside each group.
        idxs.sort_by(|&a, &b| {
            let pa = self.sessions[a].cwd.to_string_lossy();
            let pb = self.sessions[b].cwd.to_string_lossy();
            pa.cmp(&pb)
        });

        let mut entries: Vec<GlobalEntry> = Vec::with_capacity(idxs.len() + 8);
        let mut last_cwd: Option<String> = None;
        for i in idxs {
            let cwd_s = self.sessions[i].cwd.to_string_lossy().to_string();
            if last_cwd.as_deref() != Some(cwd_s.as_str()) {
                entries.push(GlobalEntry::Header(cwd_s.clone()));
                last_cwd = Some(cwd_s);
            }
            entries.push(GlobalEntry::Session(i));
        }
        self.global_sessions.entries = entries;
        if self.global_sessions.idx >= self.global_sessions.entries.len() {
            self.global_sessions.idx = 0;
        }
        self.global_sessions.scroll = 0;
        if !self.global_sessions.entries.is_empty()
            && !self.global_sessions.is_selectable(self.global_sessions.idx)
        {
            self.global_sessions.step(1);
        }
    }

    fn global_sessions_selected_session(&self) -> Option<usize> {
        match self.global_sessions.entries.get(self.global_sessions.idx) {
            Some(GlobalEntry::Session(i)) => Some(*i),
            _ => None,
        }
    }

    // -- confirm modal -----------------------------------------------------

    fn ask_confirm(&mut self, message: String, pending: PendingConfirm) {
        self.confirm.message = message;
        self.confirm.pending = Some(pending);
        self.confirm.open = true;
    }

    /// Write a Markdown summary of the active chat's session to
    /// `<cwd>/sessions/YYYY-MM-DD-HHMM-<slug>.md` — or, when the chat lives
    /// inside a cmux-owned worktree (which gets deleted on close), to
    /// `~/.cmux/sessions/` so the note survives. Surface success/failure
    /// via the usage modal.
    fn export_session_note(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let cwd = tab.cwd.clone();
        let title = tab.title.clone();
        // If this chat's cwd is a cmux-managed worktree, exporting INTO it
        // means the file vanishes on close. Redirect to ~/.cmux/sessions/.
        let dir = if tab.worktree_owned.is_some() {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_default();
            PathBuf::from(home).join(".cmux").join("sessions")
        } else {
            cwd.join("sessions")
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.usage_lines = vec![format!(" create dir failed: {} ", e)];
            self.usage_open = true;
            return;
        }
        let now = chrono_like_stamp();
        let slug = title
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .chars()
            .take(40)
            .collect::<String>();
        let fname = if slug.is_empty() {
            format!("{}.md", now)
        } else {
            format!("{}-{}.md", now, slug)
        };
        let path = dir.join(&fname);

        let session = self.resolve_active_session();
        let body = build_session_markdown(&title, &cwd, session.as_ref());
        match std::fs::write(&path, body) {
            Ok(()) => {
                self.usage_lines = vec![
                    " Session note written: ".into(),
                    format!(" {} ", path.display()),
                ];
            }
            Err(e) => {
                self.usage_lines = vec![format!(" write failed: {} ", e)];
            }
        }
        self.usage_open = true;
    }

    /// Dump the active tab's plain-text scrollback to the system clipboard.
    /// Result is shown via the usage modal (reused as a generic "info"
    /// surface here). The text_buffer mutex is dropped before formatting
    /// so the reader thread doesn't stall during a large copy.
    fn copy_scrollback(&mut self) {
        // Phase 1: snapshot lines under the lock, release ASAP.
        let snapshot: Vec<String> = self
            .tabs
            .get(self.active)
            .and_then(|t| {
                t.text_buffer.lock().ok().map(|b| {
                    let n = b.total_lines();
                    let mut v = Vec::with_capacity(n);
                    for i in 0..n {
                        if let Some(line) = b.line(i) {
                            v.push(line.to_string());
                        }
                    }
                    v
                })
            })
            .unwrap_or_default();
        // Phase 2: build the final string outside the lock.
        let mut text = String::with_capacity(snapshot.iter().map(|l| l.len() + 1).sum());
        for line in &snapshot {
            text.push_str(line);
            text.push('\n');
        }
        let msg = self.copy_to_clipboard_msg(&text, "scrollback");
        self.usage_lines = msg;
        self.usage_open = true;
    }

    /// Copy the last assistant message from the active chat's jsonl into the
    /// clipboard. Useful for "send me what claude just said".
    fn copy_last_response(&mut self) {
        let session = self.resolve_active_session();
        let Some(s) = session else {
            self.usage_lines = vec![" no resolved session for the active chat ".into()];
            self.usage_open = true;
            return;
        };
        let text = match last_assistant_text(&s.file_path) {
            Ok(t) if !t.is_empty() => t,
            Ok(_) => {
                self.usage_lines = vec![" no assistant turn found in jsonl yet ".into()];
                self.usage_open = true;
                return;
            }
            Err(e) => {
                self.usage_lines = vec![format!(" reading jsonl failed: {} ", e)];
                self.usage_open = true;
                return;
            }
        };
        let msg = self.copy_to_clipboard_msg(&text, "last response");
        self.usage_lines = msg;
        self.usage_open = true;
    }

    /// Shared "copy and report" helper for the two clipboard actions above.
    fn copy_to_clipboard_msg(&self, text: &str, label: &str) -> Vec<String> {
        if text.is_empty() {
            return vec![format!(" nothing to copy: {} is empty ", label)];
        }
        let chars = text.chars().count();
        match clipboard::copy(text) {
            Ok(backend) => vec![
                format!(" Copied {} chars to clipboard ({}) ", chars, backend),
                format!(" source: {} ", label),
            ],
            Err(e) => vec![format!(" Copy failed: {} ", e)],
        }
    }

    /// Run `git diff` synchronously in the active tab's cwd, populate the
    /// modal's lines, and open it. `git status` is appended at the top so
    /// the viewer also surfaces untracked / staged-but-not-diff files.
    fn show_git_diff(&mut self) {
        let cwd = self
            .tabs
            .get(self.active)
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        self.diff_title = format!(
            " git diff @ {} ",
            cwd.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_else(|| cwd.to_str().unwrap_or(""))
        );
        let mut lines: Vec<String> = Vec::new();
        // First: `git status --short` so untracked / staged files are visible
        // even if `git diff` (unstaged-only by default) is empty.
        match std::process::Command::new("git")
            .args(["status", "--short", "--branch"])
            .current_dir(&cwd)
            .output()
        {
            Ok(out) if out.status.success() => {
                lines.push("── git status (short) ──".into());
                for l in String::from_utf8_lossy(&out.stdout).lines() {
                    lines.push(l.to_string());
                }
                lines.push(String::new());
            }
            Ok(out) => {
                lines.push(format!(
                    "── git status failed ({}) ──",
                    out.status.code().unwrap_or(-1)
                ));
                for l in String::from_utf8_lossy(&out.stderr).lines() {
                    lines.push(l.to_string());
                }
            }
            Err(e) => {
                lines.push(format!("git not on PATH: {}", e));
            }
        }
        // Then: full `git diff HEAD` so both staged and unstaged hunks appear.
        match std::process::Command::new("git")
            .args(["--no-pager", "diff", "--no-color", "HEAD"])
            .current_dir(&cwd)
            .output()
        {
            Ok(out) => {
                lines.push("── git diff HEAD ──".into());
                if out.stdout.is_empty() {
                    lines.push(" (no diff vs HEAD) ".into());
                } else {
                    for l in String::from_utf8_lossy(&out.stdout).lines() {
                        lines.push(l.to_string());
                    }
                }
                if !out.stderr.is_empty() {
                    for l in String::from_utf8_lossy(&out.stderr).lines() {
                        lines.push(format!("err: {}", l));
                    }
                }
            }
            Err(e) => {
                lines.push(format!("git diff failed: {}", e));
            }
        }
        self.diff_lines = lines;
        self.diff_scroll = 0;
        self.diff_open = true;
    }

    /// Look at the active tab and find its claude jsonl on disk. Returns a
    /// SessionMeta with token / message totals. Resolution heuristic mirrors
    /// `save_layout`: prefer an explicit session_id, else pick the newest
    /// jsonl in ~/.claude/projects with matching cwd and updated >= tab.
    fn resolve_active_session(&self) -> Option<SessionMeta> {
        let tab = self.tabs.get(self.active)?;
        let root = sessions::claude_projects_root();
        let all = sessions::enumerate(&root);
        if let Some(id) = &tab.session_id {
            return all.into_iter().find(|s| &s.id == id);
        }
        all.into_iter().find(|s| {
            s.cwd == tab.cwd
                && s.updated
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    >= tab.created_at_unix
        })
    }

    /// Build a multi-line summary of the active chat's token usage and pop
    /// the read-only Usage modal.
    fn show_active_usage(&mut self) {
        let tab_title = self
            .tabs
            .get(self.active)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        let mut lines = Vec::with_capacity(6);
        lines.push(format!(" Active chat: {} ", tab_title));
        match self.resolve_active_session() {
            Some(s) => {
                lines.push(format!(" session id: {} ", truncate(&s.id, 60)));
                lines.push(format!(" messages: {} ", s.message_count));
                lines.push(format!(
                    " total tokens (input + output + cache_*): {} ",
                    format_tokens(s.total_tokens)
                ));
                lines.push(format!(" jsonl: {} ", s.file_path.display()));
            }
            None => {
                lines.push(" No claude jsonl found yet — chat is brand-new ".into());
                lines.push(" or its cwd doesn't match any ~/.claude/projects entry. ".into());
            }
        }
        lines.push(" Press any key to close.".into());
        self.usage_lines = lines;
        self.usage_open = true;
    }

    /// Send `self.broadcast.input` (with a trailing `\r` so claude submits it)
    /// to every chat sharing the active project's cwd. Errors per chat are
    /// swallowed — one stuck PTY shouldn't abort the whole broadcast.
    fn apply_broadcast(&mut self) -> usize {
        let text = self.broadcast.input.trim().to_string();
        if text.is_empty() {
            return 0;
        }
        let cwd = self.active_project_cwd();
        let mut payload = text.into_bytes();
        payload.push(b'\r');
        let mut sent = 0;
        for t in self.tabs.iter_mut() {
            if t.cwd != cwd {
                continue;
            }
            if t.write_input(&payload).is_ok() {
                sent += 1;
            }
        }
        sent
    }

    /// Execute the pending action committed via Y/Enter in the confirm modal.
    fn apply_confirm(&mut self) {
        let Some(action) = self.confirm.pending.take() else {
            self.confirm.clear();
            return;
        };
        match action {
            PendingConfirm::UnpinAndCloseChat(idx) => {
                // Snapshot with pinned: true BEFORE we unpin, so Ctrl+Shift+T
                // restores the pinned state the user explicitly chose.
                if idx < self.tabs.len() && self.tabs.len() > 1 {
                    if let Some(t) = self.tabs.get(idx) {
                        let snap = ClosedTab {
                            cwd: t.cwd.clone(),
                            session_id: t.session_id.clone(),
                            title: t.title.clone(),
                            pinned: true,
                            worktree_owned: t.worktree_owned.clone(),
                        };
                        if self.recently_closed.len() == RECENTLY_CLOSED_CAP {
                            self.recently_closed.pop_front();
                        }
                        self.recently_closed.push_back(snap);
                    }
                    // Drop the tab directly — bypass close_active so it
                    // doesn't push a second (stale, pinned: false) snapshot.
                    let mut t = self.tabs.remove(idx);
                    // pin protects worktree; we're explicitly removing
                    // the pin to close, so honour remove_on_close.
                    let was_pinned = t.pinned;
                    t.pinned = false; // so maybe_remove_worktree doesn't skip
                    self.maybe_remove_worktree(&t);
                    t.pinned = was_pinned;
                    t.kill();
                    if self.active >= self.tabs.len() {
                        self.active = self.tabs.len().saturating_sub(1);
                    }
                    self.on_active_changed();
                    self.save_layout();
                }
            }
            PendingConfirm::CloseProject(cwd) => {
                self.close_project(&cwd);
                self.on_active_changed();
            }
        }
        self.confirm.clear();
    }

    // -- save layout as (palette) ------------------------------------------

    fn open_save_as(&mut self) {
        self.save_as_input.clear();
        self.save_as_error = None;
        self.save_as_open = true;
    }

    fn close_save_as(&mut self) {
        self.save_as_open = false;
        self.save_as_input.clear();
        self.save_as_error = None;
    }

    /// Commit the save-as input. Sanitisation in `layout::sanitize_name`
    /// strips path-unsafe chars; if the result is empty we set an error
    /// instead of writing nonsense.
    fn apply_save_as(&mut self) {
        let raw = self.save_as_input.trim().to_string();
        let sanitised = layout::sanitize_name(&raw);
        if sanitised.is_empty() {
            self.save_as_error = Some("name is empty after sanitising".to_string());
            return;
        }
        match self.save_layout_as(&sanitised) {
            Ok(()) => self.close_save_as(),
            Err(e) => self.save_as_error = Some(e.to_string()),
        }
    }

    // -- scrollback search (Ctrl+F) ----------------------------------------

    fn toggle_search(&mut self) {
        if self.search.open {
            // Closing — reset scrollback to bottom so the user returns to
            // live output. Without this they're stuck wherever the last
            // match jumped them.
            if let Some(tab) = self.tabs.get(self.active) {
                if let Ok(mut p) = tab.parser.lock() {
                    p.set_scrollback(0);
                }
                tab.dirty.store(true, Ordering::Release);
            }
            self.search.clear();
        } else {
            self.search.open = true;
            self.search.tab_idx = Some(self.active);
        }
    }

    fn search_rerun(&mut self) {
        let Some(tab_idx) = self.search.tab_idx else {
            return;
        };
        // Compile the query first — a regex parse error stays visible in the
        // overlay header so the user can fix the pattern.
        let compiled = scrollback_text::Query::compile(
            &self.search.query,
            self.search.regex_mode,
        );
        let query = match compiled {
            Ok(q) => {
                self.search.regex_error = None;
                q
            }
            Err(e) => {
                self.search.regex_error = Some(e.to_string());
                self.search.matches.clear();
                self.search.idx = 0;
                return;
            }
        };
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let matches = tab
            .text_buffer
            .lock()
            .map(|buf| buf.find_all(&query))
            .unwrap_or_default();
        self.search.matches = matches;
        self.search.idx = 0;
        // After re-search, jump to the FIRST match (which by buffer order is
        // the oldest hit) so the user can step forward chronologically.
        self.apply_search_jump();
    }

    fn search_next(&mut self) {
        if self.search.matches.is_empty() {
            return;
        }
        self.search.idx = (self.search.idx + 1) % self.search.matches.len();
        self.apply_search_jump();
    }

    fn search_prev(&mut self) {
        if self.search.matches.is_empty() {
            return;
        }
        let n = self.search.matches.len();
        self.search.idx = (self.search.idx + n - 1) % n;
        self.apply_search_jump();
    }

    /// Move the active tab's PTY scrollback so the current match line is
    /// roughly in view. Mapping is approximate: text-buffer line offsets
    /// don't account for vt100 line wrapping, but the user lands within a
    /// few rows.
    fn apply_search_jump(&self) {
        let Some(tab_idx) = self.search.tab_idx else {
            return;
        };
        let Some(m) = self.search.matches.get(self.search.idx) else {
            return;
        };
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let offset = tab
            .text_buffer
            .lock()
            .map(|buf| buf.lines_above_bottom(m.line_idx))
            .unwrap_or(0);
        let capped = offset.min(tab.scrollback_max);
        if let Ok(mut p) = tab.parser.lock() {
            p.set_scrollback(capped);
        }
        tab.dirty.store(true, Ordering::Release);
    }

    /// If the bottom shell is open and follow-mode is on, `cd` it to the
    /// active tab's cwd. Skipped when the user is typing in the bottom shell
    /// (focused) so we don't corrupt their input.
    fn follow_bottom_to_active(&mut self) {
        if !self.config.shell.follow_tab_cwd {
            return;
        }
        if !self.bottom_open || self.bottom_focused {
            return;
        }
        let Some(target) = self.tabs.get(self.active).map(|t| t.cwd.clone()) else {
            return;
        };
        if let Some(bt) = self.bottom.as_mut() {
            let _ = bt.cd_to(&target);
        }
    }

    /// Active tab's cwd — the scope root for the F3 sidebar. Sessions whose
    /// own cwd doesn't sit under this path are hidden so the sidebar reflects
    /// what's relevant to the project you're currently in. Use Shift+F3
    /// to see *all* sessions in a separate global view.
    fn scope_root(&self) -> std::path::PathBuf {
        self.tabs
            .get(self.active)
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone())
    }

    fn apply_filter(&mut self) {
        // Cancel any running grep — query or mode may have changed.
        if let Some(job) = self.grep_job.take() {
            job.cancel();
        }
        self.grep_hits.clear();

        let scope = self.scope_root();
        let in_scope = |s: &SessionMeta| s.cwd.starts_with(&scope);

        if self.deep_grep && !self.filter.is_empty() {
            // Deep-grep path: filtered list is populated incrementally by
            // run-loop draining the channel. Start empty.
            self.filtered = Vec::new();
            let targets: Vec<_> = self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| in_scope(s))
                .map(|(i, s)| (i, s.file_path.clone()))
                .collect();
            self.grep_job = Some(grep::spawn(targets, self.filter.clone()));
        } else {
            // Metadata-only filter (instant).
            let q = self.filter.to_lowercase();
            self.filtered = self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| in_scope(s))
                .filter_map(|(i, s)| {
                    if q.is_empty() {
                        return Some(i);
                    }
                    let hay = format!(
                        "{}\n{}\n{}\n{}",
                        s.title.to_lowercase(),
                        sessions::cwd_label(&s.cwd).to_lowercase(),
                        s.git_branch.as_deref().unwrap_or("").to_lowercase(),
                        s.project_dir.to_lowercase()
                    );
                    if hay.contains(&q) {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect();
        }
        if self.sidebar_idx >= self.filtered.len() {
            self.sidebar_idx = self.filtered.len().saturating_sub(1);
        }
        self.sidebar_scroll = 0;
    }

    fn toggle_deep_grep(&mut self) {
        self.deep_grep = !self.deep_grep;
        self.apply_filter();
    }

    fn toggle_palette(&mut self) {
        if self.palette_open {
            self.close_palette();
        } else {
            self.open_palette();
        }
    }

    fn open_palette(&mut self) {
        // Always re-enumerate so the session list in the palette matches
        // what's actually on disk right now.
        self.refresh_sessions();
        self.layout_names = layout::list_named();
        self.rebuild_palette_items();
        self.palette_query.clear();
        self.palette_idx = 0;
        self.apply_palette_filter();
        self.palette_open = true;
    }

    fn close_palette(&mut self) {
        self.palette_open = false;
        self.palette_query.clear();
    }

    fn rebuild_palette_items(&mut self) {
        let mut items: Vec<PaletteItem> = Vec::new();
        let push_action = |items: &mut Vec<PaletteItem>, label: &str, action: Action| {
            items.push(PaletteItem {
                label: label.to_string(),
                hay: label.to_lowercase(),
                kind: PaletteKind::Action(action),
            });
        };
        push_action(&mut items, "★  New tab", Action::NewTab);
        push_action(
            &mut items,
            "★  New tab continuing last session (claude --continue)",
            Action::NewTabContinue,
        );
        for (i, model) in NEW_TAB_MODELS.iter().enumerate() {
            let label = format!("★  New tab with model: {}", model);
            items.push(PaletteItem {
                label: label.clone(),
                hay: label.to_lowercase(),
                kind: PaletteKind::Action(Action::NewTabWithModel(i)),
            });
        }
        push_action(
            &mut items,
            "★  Broadcast prompt to all chats in active project…",
            Action::OpenBroadcast,
        );
        push_action(
            &mut items,
            "★  Show usage / token totals for active chat",
            Action::ShowActiveUsage,
        );
        push_action(
            &mut items,
            "★  Show git diff for active chat's cwd",
            Action::ShowGitDiff,
        );
        push_action(
            &mut items,
            "★  Clear active chat (sends /clear · Ctrl+L)",
            Action::ClearActiveChat,
        );
        push_action(
            &mut items,
            "★  Copy active chat's scrollback to clipboard",
            Action::CopyChatScrollback,
        );
        push_action(
            &mut items,
            "★  Copy last claude response to clipboard",
            Action::CopyLastResponse,
        );
        push_action(
            &mut items,
            "★  Reopen most recently closed chat (Ctrl+Shift+T)",
            Action::ReopenLastClosed,
        );
        push_action(
            &mut items,
            "★  Export session note (Markdown → <cwd>/sessions/)",
            Action::ExportSessionNote,
        );
        // Refresh the snippet-name snapshot every palette rebuild so
        // `InsertSnippet(i)` resolves through this stable vec instead of
        // the live `config.snippets` map that may reshuffle on reload.
        self.snippet_keys = self.config.snippets.keys().cloned().collect();
        for (i, name) in self.snippet_keys.iter().enumerate() {
            let text = self.config.snippets.get(name).cloned().unwrap_or_default();
            let preview = text.chars().take(40).collect::<String>();
            let label = format!("★  Insert snippet: {} — {}", name, preview);
            items.push(PaletteItem {
                label: label.clone(),
                hay: format!("snippet {} {}", name.to_lowercase(), text.to_lowercase()),
                kind: PaletteKind::Action(Action::InsertSnippet(i)),
            });
        }
        push_action(&mut items, "★  Close active tab", Action::CloseTab);
        push_action(&mut items, "★  Previous chat (in project)", Action::PrevTab);
        push_action(&mut items, "★  Next chat (in project)", Action::NextTab);
        push_action(&mut items, "★  Previous project (Ctrl+F11)", Action::PrevProject);
        push_action(&mut items, "★  Next project (Ctrl+F12)", Action::NextProject);
        push_action(
            &mut items,
            "★  New project — pick a directory in the file browser",
            Action::OpenBrowserForNewProject,
        );
        let project_pin_label = if self.is_project_pinned(&self.active_project_cwd()) {
            "★  Unpin active project"
        } else {
            "★  Pin active project"
        };
        push_action(&mut items, project_pin_label, Action::ToggleActiveProjectPin);
        push_action(&mut items, "★  Toggle sessions sidebar", Action::ToggleSidebar);
        push_action(
            &mut items,
            "★  Global sessions (Shift+F3, grouped by dir)",
            Action::ToggleGlobalSessions,
        );
        push_action(&mut items, "★  Toggle commands sidebar", Action::ToggleCommands);
        push_action(&mut items, "★  Toggle files sidebar", Action::ToggleFilesSidebar);
        push_action(&mut items, "★  Show help overlay", Action::ToggleHelp);
        push_action(&mut items, "★  Toggle deep-grep", Action::ToggleDeepGrep);
        push_action(&mut items, "★  Open file browser (modal)", Action::ToggleBrowser);
        push_action(&mut items, "★  Toggle bottom terminal", Action::ToggleBottom);
        push_action(&mut items, "★  Search scrollback (Ctrl+F)", Action::ToggleSearch);
        push_action(&mut items, "★  Rename active tab (Shift+F2)", Action::RenameActiveTab);
        let pin_label = if self
            .tabs
            .get(self.active)
            .map(|t| t.pinned)
            .unwrap_or(false)
        {
            "★  Unpin active tab"
        } else {
            "★  Pin active tab (drag to reorder)"
        };
        push_action(&mut items, pin_label, Action::ToggleActivePin);
        push_action(&mut items, "★  Toggle mouse mode", Action::ToggleMouse);
        let cfg_path = config::config_path();
        let cfg_label = format!("★  Reload config ({})", cfg_path.display());
        items.push(PaletteItem {
            label: cfg_label.clone(),
            hay: cfg_label.to_lowercase(),
            kind: PaletteKind::Action(Action::ReloadConfig),
        });
        if let Some(saved) = &self.saved_layout {
            let label = format!(
                "★  Restore previous layout ({} tabs from {})",
                saved.tabs.len(),
                relative_unix(saved.saved_at_unix)
            );
            items.push(PaletteItem {
                label: label.clone(),
                hay: label.to_lowercase(),
                kind: PaletteKind::Action(Action::RestoreLayout),
            });
        }
        // Named layouts — save-as plus per-layout switch / delete.
        push_action(
            &mut items,
            "★  Save current layout as…",
            Action::OpenSaveLayoutAs,
        );
        for (i, name) in self.layout_names.iter().enumerate() {
            let label = format!("⇄  Switch to layout: {}", name);
            items.push(PaletteItem {
                label: label.clone(),
                hay: format!("switch layout {}", name.to_lowercase()),
                kind: PaletteKind::LayoutSwitch(i),
            });
        }
        for (i, name) in self.layout_names.iter().enumerate() {
            let label = format!("✕  Delete saved layout: {}", name);
            items.push(PaletteItem {
                label: label.clone(),
                hay: format!("delete layout {}", name.to_lowercase()),
                kind: PaletteKind::LayoutDelete(i),
            });
        }
        push_action(&mut items, "★  Quit", Action::Quit);

        for (i, s) in self.sessions.iter().enumerate() {
            let cwd = sessions::cwd_label(&s.cwd);
            let branch = s.git_branch.as_deref().unwrap_or("");
            let label = format!("⤴  {}  ·  {}  ·  {}", s.title, cwd, branch);
            let hay = format!(
                "{}\n{}\n{}\n{}",
                s.title.to_lowercase(),
                cwd.to_lowercase(),
                branch.to_lowercase(),
                s.project_dir.to_lowercase()
            );
            items.push(PaletteItem {
                label,
                hay,
                kind: PaletteKind::Session(i),
            });
        }

        self.palette_items = items;
    }

    fn apply_palette_filter(&mut self) {
        let q = self.palette_query.to_lowercase();
        self.palette_filtered = if q.is_empty() {
            (0..self.palette_items.len()).collect()
        } else {
            self.palette_items
                .iter()
                .enumerate()
                .filter_map(|(i, it)| if it.hay.contains(&q) { Some(i) } else { None })
                .collect()
        };
        if self.palette_idx >= self.palette_filtered.len() {
            self.palette_idx = self.palette_filtered.len().saturating_sub(1);
        }
    }

    fn palette_take_selection(&mut self) -> PaletteResult {
        let Some(&item_idx) = self.palette_filtered.get(self.palette_idx) else {
            return PaletteResult::None;
        };
        match self.palette_items[item_idx].kind {
            PaletteKind::Action(a) => PaletteResult::Run(a),
            PaletteKind::Session(s) => PaletteResult::OpenSession(s),
            PaletteKind::LayoutSwitch(i) => PaletteResult::SwitchLayout(i),
            PaletteKind::LayoutDelete(i) => PaletteResult::DeleteLayout(i),
        }
    }

    /// Pull whatever the grep thread has produced. Returns true if anything
    /// changed (caller should redraw).
    fn poll_grep(&mut self) -> bool {
        let Some(job) = &self.grep_job else {
            return false;
        };
        let (pushed, completed) = grep::drain(job, &mut self.grep_hits);
        if pushed {
            self.filtered = self.grep_hits.iter().map(|h| h.session_idx).collect();
            if self.sidebar_idx >= self.filtered.len() {
                self.sidebar_idx = self.filtered.len().saturating_sub(1);
            }
        }
        if completed {
            self.grep_job = None;
        }
        pushed || completed
    }

    fn open_selected_session(&mut self, rows: u16, cols: u16) -> Result<()> {
        let Some(&real_idx) = self.filtered.get(self.sidebar_idx) else {
            return Ok(());
        };
        let scrollback = self.config.layout.scrollback_lines;
        let s = &self.sessions[real_idx];
        let title = truncate(&s.title, 24);
        let tab = ChatTab::spawn_resume(s.cwd.clone(), &s.id, title, rows, cols, scrollback)?;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.sidebar_open = false;
        self.sidebar_focused = false;
        self.filter.clear();
        if let Some(job) = self.grep_job.take() {
            job.cancel();
        }
        self.grep_hits.clear();
        self.on_active_changed();
        self.save_layout();
        Ok(())
    }

    fn toggle_browser(&mut self) {
        if self.browser_open {
            self.browser_open = false;
        } else {
            if self.browser.is_none() {
                self.browser = Some(FileBrowser::new(
                    self.cwd.clone(),
                    self.config.browser.show_hidden,
                ));
            }
            self.browser_open = true;
        }
    }

    /// Open the file browser anchored at the active project's cwd so the user
    /// can pick a directory for a new chat. Recreates the browser instance to
    /// reset any prior navigation state.
    fn open_browser_for_new_project(&mut self) {
        let start = self.active_project_cwd();
        self.browser = Some(FileBrowser::new(start, self.config.browser.show_hidden));
        self.browser_open = true;
    }

    fn spawn_tab_here(&mut self, cwd: PathBuf, rows: u16, cols: u16) -> Result<()> {
        let scrollback = self.config.layout.scrollback_lines;
        let (resolved, worktree_owned) = self.resolve_spawn_cwd(&cwd);
        let mut tab = ChatTab::spawn(resolved, rows, cols, scrollback)?;
        tab.worktree_owned = worktree_owned;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    /// Spawn a fresh tab in the active project's cwd asking claude to use
    /// the model at `NEW_TAB_MODELS[idx]` via `--model <name>`. Title shows
    /// the model so it's obvious in the chat bar.
    fn open_tab_with_model_idx(&mut self, idx: usize, rows: u16, cols: u16) -> Result<()> {
        let Some(&model) = NEW_TAB_MODELS.get(idx) else {
            return Ok(());
        };
        let cwd = self
            .tabs
            .get(self.active)
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let scrollback = self.config.layout.scrollback_lines;
        let basename = cwd
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("claude")
            .to_string();
        let title = format!("{} · {}", basename, model);
        let tab = ChatTab::spawn_inner(
            cwd,
            &["--model", model],
            title,
            None,
            rows,
            cols,
            scrollback,
        )?;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    /// Spawn a fresh tab in the active project's cwd with `claude --continue`.
    /// Claude Code resolves --continue to "resume the latest session in this
    /// directory", so this is the common "new chat continuing where I left
    /// off" workflow.
    fn open_tab_continue(&mut self, rows: u16, cols: u16) -> Result<()> {
        let cwd = self
            .tabs
            .get(self.active)
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let scrollback = self.config.layout.scrollback_lines;
        let title = cwd
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("claude")
            .to_string();
        let tab = ChatTab::spawn_inner(
            cwd,
            &["--continue"],
            title,
            None,
            rows,
            cols,
            scrollback,
        )?;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    fn save_layout(&self) {
        let snapshot = self.build_saved_layout();
        let _ = layout::save(&snapshot);
    }

    fn restore_layout(&mut self, rows: u16, cols: u16) -> Result<()> {
        let Some(saved) = self.saved_layout.take() else {
            return Ok(());
        };
        self.apply_layout(saved, rows, cols)
    }

    /// Drain current tabs and replace them with the tabs in `saved`. Shared
    /// by `restore_layout` (unnamed/auto layout) and `switch_to_layout`
    /// (named layouts from `~/.cmux/layouts/`).
    fn apply_layout(
        &mut self,
        saved: layout::SavedLayout,
        rows: u16,
        cols: u16,
    ) -> Result<()> {
        if saved.tabs.is_empty() {
            return Ok(());
        }
        for mut t in self.tabs.drain(..) {
            t.kill();
        }
        let scrollback = self.config.layout.scrollback_lines;
        for st in &saved.tabs {
            let mut tab = match &st.session_id {
                Some(id) => ChatTab::spawn_resume(
                    st.cwd.clone(),
                    id,
                    truncate(&st.title, 24),
                    rows,
                    cols,
                    scrollback,
                )?,
                None => ChatTab::spawn_with_title(
                    st.cwd.clone(),
                    truncate(&st.title, 24),
                    rows,
                    cols,
                    scrollback,
                )?,
            };
            tab.pinned = st.pinned;
            tab.worktree_owned = st.worktree_owned.clone();
            self.tabs.push(tab);
        }
        self.active = saved.active.min(self.tabs.len().saturating_sub(1));
        if self.bottom.is_none() && saved.bottom_open {
            let h = self.config.layout.bottom_height.saturating_sub(1).max(1);
            let shell_override = self.config.shell.override_pair();
            let bottom_cwd = self
                .tabs
                .get(self.active)
                .map(|t| t.cwd.clone())
                .unwrap_or_else(|| self.cwd.clone());
            self.bottom = Some(BottomTerminal::spawn(h, 80, shell_override, bottom_cwd)?);
            self.bottom_open = true;
        }
        self.pinned_projects = saved.pinned_projects.into_iter().collect();
        self.save_layout(); // overwrite the unnamed/auto layout with current state
        Ok(())
    }

    /// Snapshot the current layout (same data the auto-save uses) and write
    /// it under `name` in `~/.cmux/layouts/<name>.json`.
    fn save_layout_as(&self, name: &str) -> Result<()> {
        let snapshot = self.build_saved_layout();
        layout::save_named(name, &snapshot)
    }

    /// Build the SavedLayout that represents the current state — extracted
    /// so save_layout (unnamed) and save_layout_as (named) share one truth.
    fn build_saved_layout(&self) -> layout::SavedLayout {
        let projects_root = sessions::claude_projects_root();
        let sessions_all = sessions::enumerate(&projects_root);
        let saved_tabs: Vec<layout::SavedTab> = self
            .tabs
            .iter()
            .map(|t| {
                let resolved_id = match &t.session_id {
                    Some(id) => Some(id.clone()),
                    None => sessions_all
                        .iter()
                        .find(|s| {
                            s.cwd == t.cwd
                                && s.updated
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0)
                                    >= t.created_at_unix
                        })
                        .map(|s| s.id.clone()),
                };
                layout::SavedTab {
                    cwd: t.cwd.clone(),
                    session_id: resolved_id,
                    title: t.title.clone(),
                    created_at_unix: t.created_at_unix,
                    pinned: t.pinned,
                    worktree_owned: t.worktree_owned.clone(),
                }
            })
            .collect();

        layout::SavedLayout {
            version: 1,
            saved_at_unix: layout::now_unix(),
            active: self.active,
            tabs: saved_tabs,
            sidebar_open: self.sidebar_open,
            bottom_open: self.bottom_open,
            pinned_projects: self.pinned_projects.iter().cloned().collect(),
        }
    }

    /// Load `name` from disk and apply it. Drains current tabs.
    fn switch_to_layout(&mut self, name: &str, rows: u16, cols: u16) -> Result<()> {
        let Some(saved) = layout::load_named(name) else {
            return Ok(()); // silently no-op if missing
        };
        self.apply_layout(saved, rows, cols)
    }

    /// CLI `--resume <id>`: drop the default tab and replace it with a
    /// resumed one. Spawn cwd stays the user-provided / launch cwd; claude
    /// uses the session jsonl for the real state anyway.
    fn cli_replace_default_with_resume(
        &mut self,
        id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<()> {
        let cwd = self
            .tabs
            .first()
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let scrollback = self.config.layout.scrollback_lines;
        let title = truncate(id, 16);
        let tab = ChatTab::spawn_resume(cwd, id, title, rows, cols, scrollback)?;
        for mut old in self.tabs.drain(..) {
            old.kill();
        }
        self.tabs.push(tab);
        self.active = 0;
        Ok(())
    }

    /// CLI `--continue`: replace the default tab with `claude --continue`,
    /// which Claude Code resolves to "resume the most recent session in this
    /// cwd". Easier than looking up the session id.
    fn cli_replace_default_with_continue(
        &mut self,
        rows: u16,
        cols: u16,
    ) -> Result<()> {
        let cwd = self
            .tabs
            .first()
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let scrollback = self.config.layout.scrollback_lines;
        let title = cwd
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("claude")
            .to_string();
        let tab = ChatTab::spawn_inner(
            cwd,
            &["--continue"],
            title,
            None,
            rows,
            cols,
            scrollback,
        )?;
        for mut old in self.tabs.drain(..) {
            old.kill();
        }
        self.tabs.push(tab);
        self.active = 0;
        Ok(())
    }

    fn toggle_bottom(&mut self) -> Result<()> {
        if self.bottom_open {
            self.bottom_open = false;
            self.bottom_focused = false;
            return Ok(());
        }
        if self.bottom.is_none() {
            // Bottom pane has one Borders::TOP row of chrome.
            let h = self.config.layout.bottom_height.saturating_sub(1).max(1);
            let shell_override = self.config.shell.override_pair();
            let bottom_cwd = self
                .tabs
                .get(self.active)
                .map(|t| t.cwd.clone())
                .unwrap_or_else(|| self.cwd.clone());
            self.bottom = Some(BottomTerminal::spawn(h, 80, shell_override, bottom_cwd)?);
        }
        self.bottom_open = true;
        self.bottom_focused = true;
        Ok(())
    }
}

fn relative_unix(then: u64) -> String {
    let now = layout::now_unix();
    if then == 0 || then > now {
        return "just now".to_string();
    }
    let d = now - then;
    if d < 60 {
        format!("{}s ago", d)
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

/// Compact token-count formatter: 1234567 → "1.2M", 12345 → "12.3k", 999 → "999".
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let collected: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{}…", collected)
    } else {
        collected
    }
}

// ===========================================================================
// CLI args
// ===========================================================================

/// Parsed command-line arguments. Lives separately from the TUI state so the
/// pre-flight phase can read them before raw mode is enabled.
#[derive(Default)]
struct CliArgs {
    /// Positional: starting cwd for the default first tab. Defaults to
    /// `std::env::current_dir()`.
    cwd: Option<PathBuf>,
    /// `--layout NAME` — apply that named layout on startup.
    layout: Option<String>,
    /// `--resume ID` — first tab resumes that session id.
    resume: Option<String>,
    /// `--continue` — first tab spawns claude with `--continue`.
    continue_last: bool,
    /// `--doctor` — print diagnostics and exit (no TUI).
    doctor: bool,
    /// `--prune-worktrees` — scan known layouts, find cmux-managed
    /// worktrees no live/saved tab references, remove them, exit.
    prune_worktrees: bool,
}

fn parse_args() -> std::result::Result<CliArgs, String> {
    let mut out = CliArgs::default();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("cmux {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--layout" => {
                out.layout = Some(iter.next().ok_or("--layout requires a name")?.to_string());
            }
            "--resume" => {
                out.resume = Some(iter.next().ok_or("--resume requires a session id")?.to_string());
            }
            "--continue" => {
                out.continue_last = true;
            }
            "--doctor" => {
                out.doctor = true;
            }
            "--prune-worktrees" => {
                out.prune_worktrees = true;
            }
            s if s.starts_with("--") => return Err(format!("unknown flag: {}", s)),
            s => {
                if out.cwd.is_some() {
                    return Err(format!("unexpected positional argument: {}", s));
                }
                out.cwd = Some(PathBuf::from(s));
            }
        }
    }
    if out.resume.is_some() && out.continue_last {
        return Err("--resume and --continue are mutually exclusive".to_string());
    }
    Ok(out)
}

fn print_usage() {
    println!(
        "cmux {ver} — TUI host for the claude CLI

USAGE:
    cmux [PATH] [OPTIONS]

ARGS:
    PATH                Starting cwd for the first tab (default: current dir)

OPTIONS:
    --layout NAME       Apply a named layout from ~/.cmux/layouts/<NAME>.json
    --resume ID         Resume the given claude session id in the first tab
    --continue          Spawn the first tab with `claude --continue`
    --doctor            Print diagnostics (paths, versions, config) and exit
    --prune-worktrees   Remove cmux-managed git worktrees no live/saved tab
                        references (orphans from crashes). Reports each
                        action and exits.
    -h, --help          Print this help and exit
    -V, --version       Print version and exit",
        ver = env!("CARGO_PKG_VERSION"),
    );
}

/// Return a `YYYY-MM-DD-HHMM` stamp in UTC. We don't depend on chrono — the
/// stdlib gives us enough.
fn chrono_like_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since 1970-01-01 (Thursday). civil_from_days adapted from
    // Howard Hinnant's date algorithms.
    let z = now as i64 / 86_400 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let secs_in_day = now % 86_400;
    let h = secs_in_day / 3600;
    let mins = (secs_in_day % 3600) / 60;
    format!("{:04}-{:02}-{:02}-{:02}{:02}", y, m, d, h, mins)
}

/// Build the Markdown body for the session-note export.
fn build_session_markdown(
    title: &str,
    cwd: &Path,
    session: Option<&SessionMeta>,
) -> String {
    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(out, "# {}", title);
    let _ = writeln!(out);
    let _ = writeln!(out, "- generated:  {} UTC", chrono_like_stamp());
    let _ = writeln!(out, "- cwd:        {}", cwd.display());
    if let Some(s) = session {
        let _ = writeln!(out, "- session id: {}", s.id);
        if let Some(b) = &s.git_branch {
            let _ = writeln!(out, "- git branch: {}", b);
        }
        let _ = writeln!(out, "- messages:   {}", s.message_count);
        let _ = writeln!(out, "- tokens:     {}", format_tokens(s.total_tokens));
        let _ = writeln!(out, "- jsonl:      {}", s.file_path.display());
        let _ = writeln!(out);
        // First and last user prompts + last assistant turn.
        if let Ok((first_user, last_user, last_assistant)) =
            extract_session_milestones(&s.file_path)
        {
            if !first_user.is_empty() {
                let _ = writeln!(out, "## First user prompt\n\n```\n{}\n```\n", first_user.trim());
            }
            if !last_user.is_empty() && last_user != first_user {
                let _ = writeln!(out, "## Last user prompt\n\n```\n{}\n```\n", last_user.trim());
            }
            if !last_assistant.is_empty() {
                let _ = writeln!(out, "## Last assistant response\n\n{}\n", last_assistant);
            }
        }
    } else {
        let _ = writeln!(out, "- session id: (not resolved)");
        let _ = writeln!(out);
        let _ = writeln!(out, "_(no jsonl found — the chat is brand-new or its cwd doesn't match any ~/.claude/projects entry.)_");
    }
    out
}

/// Return `(first_user, last_user, last_assistant)` text from a claude jsonl.
fn extract_session_milestones(
    path: &Path,
) -> std::io::Result<(String, String, String)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let rdr = std::io::BufReader::new(file);
    let mut first_user = String::new();
    let mut last_user = String::new();
    let mut last_assistant = String::new();
    for line in rdr.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let typ = v.get("type").and_then(|t| t.as_str());
        if typ != Some("user") && typ != Some("assistant") {
            continue;
        }
        let content = v.get("message").and_then(|m| m.get("content"));
        let text = match content {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) != Some("text") {
                        return None;
                    }
                    b.get("text").and_then(|t| t.as_str()).map(String::from)
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        if typ == Some("user") {
            if first_user.is_empty() {
                first_user = text.clone();
            }
            last_user = text;
        } else {
            last_assistant = text;
        }
    }
    Ok((first_user, last_user, last_assistant))
}

/// Find the text content of the most recent `assistant` entry in a claude
/// jsonl. Skips tool_use / system entries. Returns "" if none found.
fn last_assistant_text(path: &Path) -> std::io::Result<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let rdr = std::io::BufReader::new(file);
    let mut last: String = String::new();
    for line in rdr.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let content = v.get("message").and_then(|m| m.get("content"));
        let text = match content {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) != Some("text") {
                        return None;
                    }
                    b.get("text").and_then(|t| t.as_str()).map(String::from)
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        if !text.trim().is_empty() {
            last = text;
        }
    }
    Ok(last)
}

/// Scan a chunk of PTY bytes for an OSC 7 cwd hint
/// (`\x1b]7;file://host/path<BEL or ESC \>`) and return the parsed path.
/// Shells emit this after every `cd` when the terminal advertises support;
/// using it lets cmux track the chat's real cwd even when the user `cd`s
/// inside claude / the bottom shell.
fn extract_osc7_path(bytes: &[u8]) -> Option<PathBuf> {
    let needle = b"\x1b]7;file://";
    let mut search_from = 0;
    while let Some(rel) = bytes[search_from..]
        .windows(needle.len())
        .position(|w| w == needle)
    {
        let after = &bytes[search_from + rel + needle.len()..];
        // Skip the authority (hostname) — runs until the first `/`.
        let Some(slash) = after.iter().position(|&b| b == b'/') else {
            search_from += rel + needle.len();
            continue;
        };
        // Path runs until BEL (0x07) or ESC (0x1b — start of ESC \ terminator).
        let path_bytes = &after[slash..];
        let end = path_bytes
            .iter()
            .position(|&b| b == 0x07 || b == 0x1b)
            .unwrap_or(path_bytes.len());
        if end == 0 {
            search_from += rel + needle.len();
            continue;
        }
        let raw = &path_bytes[..end];
        // OSC 7 paths are URL-encoded. We're not running a full decoder —
        // just turn `%20` → ' ', enough for typical Unix/Windows paths.
        let mut decoded = String::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == b'%' && i + 2 < raw.len() {
                if let (Some(h), Some(l)) =
                    (hex_nibble(raw[i + 1]), hex_nibble(raw[i + 2]))
                {
                    decoded.push((h * 16 + l) as char);
                    i += 3;
                    continue;
                }
            }
            decoded.push(raw[i] as char);
            i += 1;
        }
        // Windows paths come as `file:///C:/foo` → `/C:/foo`. Strip the
        // leading slash so PathBuf treats it as a regular drive path.
        if cfg!(windows)
            && decoded.len() >= 3
            && decoded.starts_with('/')
            && decoded.chars().nth(2) == Some(':')
        {
            decoded.remove(0);
        }
        return Some(PathBuf::from(decoded));
    }
    None
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse a config-supplied accent colour name into a ratatui Color. Unknown
/// values fall back to cyan so a typo doesn't crash the TUI.
fn parse_accent(name: &str) -> Color {
    match name.trim().to_lowercase().as_str() {
        "cyan" => Color::Cyan,
        "yellow" => Color::Yellow,
        "green" => Color::Green,
        "magenta" => Color::Magenta,
        "red" => Color::Red,
        "blue" => Color::Blue,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "lightblue" => Color::LightBlue,
        "lightgreen" => Color::LightGreen,
        "lightmagenta" => Color::LightMagenta,
        "lightred" => Color::LightRed,
        "lightyellow" => Color::LightYellow,
        "lightcyan" => Color::LightCyan,
        _ => Color::Cyan,
    }
}

/// Parse a key-combo string like `"f4"`, `"ctrl-x"`, `"alt-shift-f12"` into
/// a (KeyCode, KeyModifiers) pair. Returns None for unknown forms — caller
/// drops the entry silently.
fn parse_key_combo(s: &str) -> Option<(KeyCode, KeyModifiers)> {
    let mut mods = KeyModifiers::empty();
    let parts: Vec<&str> = s.split('-').collect();
    if parts.is_empty() {
        return None;
    }
    let (mod_parts, key_part) = parts.split_at(parts.len() - 1);
    for m in mod_parts {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }
    let key = key_part[0].to_lowercase();
    let code = if let Some(rest) = key.strip_prefix('f') {
        let n: u8 = rest.parse().ok()?;
        if !(1..=12).contains(&n) {
            return None;
        }
        KeyCode::F(n)
    } else if key.chars().count() == 1 {
        KeyCode::Char(key.chars().next()?)
    } else {
        match key.as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "backspace" => KeyCode::Backspace,
            "space" => KeyCode::Char(' '),
            "tab" => KeyCode::Tab,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "delete" | "del" => KeyCode::Delete,
            _ => return None,
        }
    };
    Some((code, mods))
}

/// Resolve a snake_case action name into an Action variant. Only nullary
/// variants are settable from config — variants with payloads (SwitchTab(n),
/// SwitchProject(n), NewTabWithModel(i)) stay keyed to their existing
/// hardcoded triggers.
fn action_from_str(s: &str) -> Option<Action> {
    Some(match s.trim().to_lowercase().as_str() {
        "new_tab" => Action::NewTab,
        "close_tab" => Action::CloseTab,
        "prev_tab" => Action::PrevTab,
        "next_tab" => Action::NextTab,
        "toggle_sidebar" | "sessions_sidebar" => Action::ToggleSidebar,
        "toggle_files_sidebar" | "files_sidebar" => Action::ToggleFilesSidebar,
        "toggle_deep_grep" | "deep_grep" => Action::ToggleDeepGrep,
        "toggle_mouse" => Action::ToggleMouse,
        "toggle_palette" | "palette" => Action::TogglePalette,
        "toggle_browser" | "file_browser" => Action::ToggleBrowser,
        "toggle_bottom" | "bottom_shell" => Action::ToggleBottom,
        "toggle_commands" | "commands_sidebar" => Action::ToggleCommands,
        "toggle_help" | "help" => Action::ToggleHelp,
        "toggle_search" | "search" => Action::ToggleSearch,
        "rename_active_tab" | "rename" => Action::RenameActiveTab,
        "restore_layout" => Action::RestoreLayout,
        "open_save_layout_as" | "save_layout_as" => Action::OpenSaveLayoutAs,
        "toggle_global_sessions" | "global_sessions" => Action::ToggleGlobalSessions,
        "toggle_active_pin" | "pin_chat" => Action::ToggleActivePin,
        "prev_project" => Action::PrevProject,
        "next_project" => Action::NextProject,
        "open_browser_for_new_project" | "new_project" => Action::OpenBrowserForNewProject,
        "toggle_active_project_pin" | "pin_project" => Action::ToggleActiveProjectPin,
        "new_tab_continue" | "continue" => Action::NewTabContinue,
        "open_broadcast" | "broadcast" => Action::OpenBroadcast,
        "reload_config" | "reload" => Action::ReloadConfig,
        "quit" => Action::Quit,
        "clear_active_chat" | "clear_chat" | "clear" => Action::ClearActiveChat,
        "copy_chat_scrollback" | "copy_scrollback" => Action::CopyChatScrollback,
        "copy_last_response" | "copy_response" => Action::CopyLastResponse,
        "show_active_usage" | "usage" => Action::ShowActiveUsage,
        "show_git_diff" | "git_diff" | "diff" => Action::ShowGitDiff,
        "reopen_last_closed" | "reopen" => Action::ReopenLastClosed,
        "export_session_note" | "export_note" | "journal" => Action::ExportSessionNote,
        _ => return None,
    })
}

/// User-defined key→action map, rebuilt from config on every reload.
#[derive(Default)]
struct KeyBindings {
    /// Parallel arrays keep the API tiny — linear scan over ≤ a few dozen
    /// entries is cheaper than a HashMap and avoids needing Hash on KeyCode
    /// across crossterm versions.
    bindings: Vec<((KeyCode, KeyModifiers), Action)>,
}

impl KeyBindings {
    fn from_config(map: &std::collections::HashMap<String, String>) -> Self {
        let mut bindings = Vec::new();
        for (action_name, key_combo) in map {
            let (Some(act), Some(key)) =
                (action_from_str(action_name), parse_key_combo(key_combo))
            else {
                continue;
            };
            bindings.push((key, act));
        }
        Self { bindings }
    }

    fn lookup(&self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        self.bindings
            .iter()
            .find(|((c, m), _)| *c == code && *m == mods)
            .map(|(_, a)| *a)
    }
}

/// URL regex used by Ctrl+Click detection. Lazy-compiled because building it
/// every click would be silly. Tuned so trailing sentence punctuation (`.,;:!?`)
/// stays outside the match while internal punctuation is fine.
fn url_regex() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(?:https?|file)://[-a-zA-Z0-9+&@#/%?=~_|!:,.;()*]*[-a-zA-Z0-9+&@#/%=~_|()]",
        )
        .expect("url regex compiles")
    })
}

/// If a URL covers `(row, col)` on the current vt100 screen, return it.
fn url_at(screen: &vt100::Screen, row: u16, col: u16) -> Option<String> {
    let (rows, cols) = screen.size();
    if row >= rows {
        return None;
    }
    let mut row_cells: Vec<String> = Vec::with_capacity(cols as usize);
    for c in 0..cols {
        let s = screen
            .cell(row, c)
            .map(|c| c.contents().to_string())
            .unwrap_or_default();
        row_cells.push(if s.is_empty() { " ".into() } else { s });
    }
    let row_text: String = row_cells.concat();
    let mut byte_to_col: Vec<u16> = Vec::with_capacity(row_text.len() + 1);
    for (cc, s) in row_cells.iter().enumerate() {
        for _ in 0..s.len() {
            byte_to_col.push(cc as u16);
        }
    }
    byte_to_col.push(cols);
    for m in url_regex().find_iter(&row_text) {
        let col_start = byte_to_col.get(m.start()).copied().unwrap_or(0);
        let col_end = byte_to_col.get(m.end()).copied().unwrap_or(cols);
        if col >= col_start && col < col_end {
            return Some(m.as_str().to_string());
        }
    }
    None
}

/// Open `url` in the OS default handler. Fire-and-forget — errors are
/// swallowed because failing to open a URL should never tear down the TUI.
fn open_url(url: &str) {
    let _ = open_url_inner(url);
}

#[cfg(windows)]
fn open_url_inner(url: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // `cmd /c start "" "<url>"` — the empty quoted string is the window title;
    // start treats the first quoted argument as the title, so we need a
    // placeholder so the actual URL is parsed as the file to open.
    std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_url_inner(url: &str) -> Result<()> {
    std::process::Command::new("open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn open_url_inner(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Collect all worktree paths cmux knows are still in use: the auto-saved
/// layout's tabs plus every named layout's tabs. The active session has no
/// state on disk at startup, so the union of layouts is the best we can do
/// without IPC into a running cmux.
fn collect_known_worktrees() -> std::collections::HashSet<PathBuf> {
    let mut known = std::collections::HashSet::new();
    if let Some(l) = layout::load() {
        for t in l.tabs {
            if let Some((_, wt)) = t.worktree_owned {
                known.insert(wt);
            }
        }
    }
    for name in layout::list_named() {
        if let Some(l) = layout::load_named(&name) {
            for t in l.tabs {
                if let Some((_, wt)) = t.worktree_owned {
                    known.insert(wt);
                }
            }
        }
    }
    known
}

/// `--prune-worktrees` entry point: enumerate cmux-managed worktrees that
/// no saved tab references, remove them, print a report, exit.
fn run_prune_worktrees() {
    let cfg = config::load();
    let known = collect_known_worktrees();

    // Build the candidate roots: the configured worktree_root resolved
    // against every repo we know of (via saved tabs' worktree_owned tuples),
    // plus parents of every known worktree path (in case the user changed
    // the config and orphans are now in an old root).
    let mut roots: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let configured = &cfg.git.worktree_root;
    let configured_is_abs = Path::new(configured).is_absolute();
    if configured_is_abs {
        roots.insert(PathBuf::from(configured));
    }
    // Scan saved layouts again to learn repo roots and old worktree parents.
    let layouts: Vec<layout::SavedLayout> = layout::load()
        .into_iter()
        .chain(
            layout::list_named()
                .into_iter()
                .filter_map(|n| layout::load_named(&n)),
        )
        .collect();
    for l in &layouts {
        for t in &l.tabs {
            if let Some((repo, wt)) = &t.worktree_owned {
                if !configured_is_abs {
                    roots.insert(repo.join(configured));
                }
                if let Some(p) = wt.parent() {
                    roots.insert(p.to_path_buf());
                }
            }
        }
    }

    if roots.is_empty() {
        println!(
            "cmux: no known worktree roots — nothing to scan. Configure \
             [git] auto_worktree first, or spawn at least one cmux-managed \
             worktree."
        );
        return;
    }

    let roots_vec: Vec<PathBuf> = roots.into_iter().collect();
    println!("cmux: scanning for orphan worktrees in:");
    for r in &roots_vec {
        println!("  {}", r.display());
    }
    let orphans = worktree::find_orphans(&roots_vec, &known);
    if orphans.is_empty() {
        println!("cmux: no orphan worktrees found.");
        return;
    }
    println!("cmux: found {} orphan worktree(s):", orphans.len());
    let mut removed = 0;
    let mut failed = 0;
    for path in &orphans {
        print!("  removing {} ... ", path.display());
        match worktree::force_remove(path) {
            Ok(()) => {
                println!("ok");
                removed += 1;
            }
            Err(e) => {
                println!("FAIL: {}", e);
                failed += 1;
            }
        }
    }
    println!(
        "cmux: done. removed {}, failed {} (of {}).",
        removed,
        failed,
        orphans.len()
    );
}

/// Print a diagnostics dump (config, layouts, claude binary, terminal env)
/// and exit. Intended as the answer when a user files an issue: paste this
/// output. Doesn't enter raw mode and doesn't touch any files.
fn print_doctor() {
    println!("cmux {} — diagnostics", env!("CARGO_PKG_VERSION"));
    println!();
    println!("[platform]");
    println!("  os                  = {}", std::env::consts::OS);
    println!("  family              = {}", std::env::consts::FAMILY);
    println!("  arch                = {}", std::env::consts::ARCH);
    if let Ok(term) = std::env::var("TERM") {
        println!("  TERM                = {}", term);
    }
    if let Ok(t) = std::env::var("WT_SESSION") {
        println!("  WT_SESSION          = {} (Windows Terminal)", t);
    }
    if let Ok(t) = std::env::var("TERM_PROGRAM") {
        println!("  TERM_PROGRAM        = {}", t);
    }

    println!();
    println!("[claude]");
    let exe = if cfg!(windows) { "claude.cmd" } else { "claude" };
    match find_claude_on_path() {
        Some(p) => {
            println!("  binary on PATH      = {}", p.display());
            let ver = std::process::Command::new(&p)
                .arg("--version")
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                    } else {
                        None
                    }
                });
            println!("  --version           = {}", ver.unwrap_or_else(|| "(no output)".into()));
        }
        None => {
            println!("  binary on PATH      = NOT FOUND ({})", exe);
        }
    }
    println!("  ~/.claude/projects  = {}", sessions::claude_projects_root().display());
    let sess_count = std::fs::read_dir(sessions::claude_projects_root())
        .map(|rd| rd.flatten().count())
        .unwrap_or(0);
    println!("  project dirs        = {}", sess_count);

    println!();
    println!("[cmux paths]");
    let cfg = config::config_path();
    println!("  config              = {} {}", cfg.display(), exists_tag(&cfg));
    let layout = layout::layout_path();
    println!("  auto-layout         = {} {}", layout.display(), exists_tag(&layout));
    let layouts_dir = layout::layouts_dir();
    println!("  named layouts dir   = {} {}", layouts_dir.display(), exists_tag(&layouts_dir));
    let names = layout::list_named();
    if !names.is_empty() {
        println!("  named layouts       = {}", names.join(", "));
    }

    println!();
    println!("[config snapshot]");
    let c = config::load();
    println!("  layout.auto_restore       = {}", c.layout.auto_restore);
    println!("  layout.scrollback_lines   = {}", c.layout.scrollback_lines);
    println!("  shell.follow_tab_cwd      = {}", c.shell.follow_tab_cwd);
    println!("  notify.bell               = {}", c.notify.bell);
    println!("  notify.toast              = {}", c.notify.toast);
    println!("  detect.permission_patterns = {} patterns", c.detect.permission_patterns.len());
    println!("  theme.accent              = {}", c.theme.accent);
    println!("  keys (overrides)          = {}", c.keys.len());
}

/// Helper: returns "(exists)" / "(missing)" suffix.
fn exists_tag(p: &Path) -> &'static str {
    if p.exists() {
        "(exists)"
    } else {
        "(missing)"
    }
}

/// Locate `claude` (or `claude.cmd` on Windows) on PATH. Used by both the
/// pre-flight check and `--doctor`.
fn find_claude_on_path() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "claude.cmd" } else { "claude" };
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Make sure `claude` (or `claude.cmd` on Windows) is somewhere on PATH so we
/// can give a clear pre-raw-mode error instead of a cryptic PTY failure later.
fn claude_on_path() -> bool {
    let exe = if cfg!(windows) { "claude.cmd" } else { "claude" };
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        if dir.join(exe).exists() {
            return true;
        }
    }
    false
}

// ===========================================================================
// Entry
// ===========================================================================
fn main() -> Result<()> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("cmux: {}\n", msg);
            print_usage();
            std::process::exit(2);
        }
    };

    if args.doctor {
        print_doctor();
        std::process::exit(0);
    }
    if args.prune_worktrees {
        run_prune_worktrees();
        std::process::exit(0);
    }

    if !claude_on_path() {
        let exe = if cfg!(windows) { "claude.cmd" } else { "claude" };
        eprintln!(
            "cmux: cannot find `{}` on PATH.\n\nInstall Claude Code (https://claude.com/claude-code) \
             and make sure the binary is on PATH, then re-run.",
            exe
        );
        std::process::exit(127);
    }

    config::ensure_default_written();
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let size = terminal.size()?;
    let pty_rows = size.height.saturating_sub(CHROME_ROWS).max(1);
    let pty_cols = size.width.max(1);

    let launch_cwd = match &args.cwd {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let mut app = App::new(launch_cwd, pty_rows, pty_cols)?;

    // CLI-driven first-tab overrides: --resume / --continue replace the
    // default tab with a flavoured spawn before the run loop starts.
    if let Some(id) = &args.resume {
        if let Err(e) = app.cli_replace_default_with_resume(id, pty_rows, pty_cols) {
            eprintln!("cmux: --resume failed: {}", e);
        }
    } else if args.continue_last {
        if let Err(e) = app.cli_replace_default_with_continue(pty_rows, pty_cols) {
            eprintln!("cmux: --continue failed: {}", e);
        }
    }

    // --layout overrides everything: drain whatever's there and apply the
    // named layout. Falls back silently to the current tab if missing.
    if let Some(name) = &args.layout {
        let _ = app.switch_to_layout(name, pty_rows, pty_cols);
    } else if app.config.layout.auto_restore && app.saved_layout.is_some() {
        // Auto-restore: same as before, only when --layout wasn't given.
        let _ = app.restore_layout(pty_rows, pty_cols);
    }

    let result = run(&mut terminal, &mut app);

    app.save_layout();
    app.kill_all();
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    result
}

enum KeyOutcome {
    Continue,
    LayoutChanged,
    Quit,
}

fn run<B: ratatui::backend::Backend + std::io::Write>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    let mut needs_draw = true;
    let mut last_pty_area = Rect::default();
    let mut applied_pty_size: Option<(u16, u16)> = None;

    loop {
        app.cleanup_dead();
        if app.tabs.is_empty() {
            return Ok(());
        }

        // Apply pending mouse-capture mode change
        if app.mouse_capture_dirty {
            if app.mouse_on {
                execute!(terminal.backend_mut(), EnableMouseCapture)?;
            } else {
                execute!(terminal.backend_mut(), DisableMouseCapture)?;
            }
            terminal.clear()?;
            needs_draw = true;
            app.mouse_capture_dirty = false;
        }

        // Any tab with new output triggers a redraw — the active tab so its
        // PTY view is current, background tabs so their unread badges and
        // state dots are up to date in the tab bar.
        for t in app.tabs.iter() {
            if t.dirty.swap(false, Ordering::Acquire) {
                needs_draw = true;
            }
        }

        if let Some(bt) = app.bottom.as_ref() {
            if app.bottom_open && bt.dirty.swap(false, Ordering::Acquire) {
                needs_draw = true;
            }
        }

        if app.poll_grep() {
            needs_draw = true;
        }

        // Drain OSC 7 cwd hints into ChatTab.cwd. Tab/project bars will
        // pick up the new grouping next draw.
        let mut cwd_changed = false;
        for tab in app.tabs.iter_mut() {
            let Some(new_cwd) = tab
                .pending_cwd
                .lock()
                .ok()
                .and_then(|mut g| g.take())
            else {
                continue;
            };
            if new_cwd != tab.cwd && new_cwd.exists() {
                tab.cwd = new_cwd;
                cwd_changed = true;
            }
        }
        if cwd_changed {
            needs_draw = true;
        }

        // Recompute per-tab state. If anything changed, force redraw.
        let now = Instant::now();
        let patterns = &app.config.detect.permission_patterns;
        let states: Vec<TabState> = app
            .tabs
            .iter()
            .map(|t| t.compute_state(now, patterns))
            .collect();
        // Per-tab state transitions: drive notifications AND increment the
        // unread-replies badge on Streaming → Idle for non-active tabs.
        let bell = app.config.notify.bell;
        let toast = app.config.notify.toast;
        let osc = app.config.notify.osc;
        let webhook_url = app.config.notify.webhook.clone();
        let mut fired_bell = false;
        let mut fired_osc = false;
        let mut fired_webhook = false;
        let active_idx = app.active;
        for (i, &state) in states.iter().enumerate() {
            let prev = app.last_states.get(i).copied().unwrap_or(TabState::Idle);
            let tab = &mut app.tabs[i];
            // A Streaming → Idle edge marks "claude finished a turn". Bump
            // the unread counter only for backgrounded tabs — the user is
            // already looking at the active one.
            if prev == TabState::Streaming && state == TabState::Idle && i != active_idx {
                tab.unread_replies = tab.unread_replies.saturating_add(1);
            }
            match state {
                TabState::AwaitingPermission if !tab.notified_awaiting => {
                    tab.notified_awaiting = true;
                    if bell && !fired_bell {
                        notify::bell();
                        fired_bell = true; // one bell per cycle, even if several tabs flip
                    }
                    if osc && !fired_osc {
                        notify::osc_notify(
                            &format!("cmux: {} needs you", tab.title),
                            "claude is waiting on a permission prompt",
                        );
                        fired_osc = true;
                    }
                    if toast {
                        notify::toast(
                            &format!("{} needs you", tab.title),
                            "claude is waiting on a permission prompt",
                        );
                    }
                    if !webhook_url.is_empty() && !fired_webhook {
                        notify::webhook(
                            &webhook_url,
                            &format!("{} needs you", tab.title),
                            "claude is waiting on a permission prompt",
                            &tab.title,
                            &tab.cwd.to_string_lossy(),
                        );
                        fired_webhook = true;
                    }
                }
                TabState::Idle | TabState::Streaming => {
                    tab.notified_awaiting = false;
                }
                _ => {}
            }
        }
        if states != app.last_states {
            needs_draw = true;
            app.last_states = states;
        }

        if needs_draw {
            terminal.draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1), // 0: project bar
                        Constraint::Length(1), // 1: chat bar
                        Constraint::Min(1),    // 2: body (sidebars + pty + bottom)
                        Constraint::Length(1), // 3: button bar
                    ])
                    .split(f.area());

                // --- project bar (row 0) ---
                let projects = app.projects();
                let active_project_idx = app.active_project_idx();
                let (proj_line, proj_rects, new_proj_rect) = render_project_bar(
                    &projects,
                    active_project_idx,
                    &app.pinned_projects,
                    chunks[0],
                );
                app.project_rects = proj_rects;
                app.new_project_rect = new_proj_rect;
                f.render_widget(
                    Paragraph::new(proj_line).style(Style::default().bg(Color::Black)),
                    chunks[0],
                );

                // --- chat bar (row 1, filtered to active project's chats) ---
                let chat_indices = app.chats_in_active_project();
                let (chat_line, chat_rects, new_rect) = render_chat_bar(
                    &app.tabs,
                    &chat_indices,
                    &app.last_states,
                    app.active,
                    chunks[1],
                );
                app.chat_rects = chat_rects;
                app.new_tab_rect = Some(new_rect);
                f.render_widget(
                    Paragraph::new(chat_line).style(Style::default().bg(Color::Black)),
                    chunks[1],
                );

                // Body: split off bottom terminal first (vertical),
                // then sidebar from what remains (horizontal).
                let body = chunks[2];
                let (upper_body, bottom_pane_area) = if app.bottom_open {
                    let h = app
                        .config
                        .layout
                        .bottom_height
                        .max(4)
                        .min(body.height.saturating_sub(5));
                    let v = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Min(1), Constraint::Length(h)])
                        .split(body);
                    (v[0], Some(v[1]))
                } else {
                    (body, None)
                };

                // Compute widths for both sidebars (clamped to fit)
                let total_w = upper_body.width;
                let left_w = if app.sidebar_open {
                    app.config.layout.sidebar_width.max(20).min(total_w.saturating_sub(20))
                } else {
                    0
                };
                let right_w = if app.right_sidebar_open {
                    app.config
                        .layout
                        .right_sidebar_width
                        .max(20)
                        .min(total_w.saturating_sub(left_w + 20))
                } else {
                    0
                };

                let mut hconstraints: Vec<Constraint> = Vec::new();
                if left_w > 0 {
                    hconstraints.push(Constraint::Length(left_w));
                }
                hconstraints.push(Constraint::Min(1));
                if right_w > 0 {
                    hconstraints.push(Constraint::Length(right_w));
                }
                let h_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(hconstraints)
                    .split(upper_body);

                let mut hi = 0usize;
                let left_area_opt = if left_w > 0 {
                    let r = Some(h_chunks[hi]);
                    hi += 1;
                    r
                } else {
                    None
                };
                let pty_area_outer = h_chunks[hi];
                hi += 1;
                let right_area_opt = if right_w > 0 {
                    Some(h_chunks[hi])
                } else {
                    None
                };

                // Bordered main PTY area; border colour reflects focus.
                let main_focused = !app.sidebar_focused
                    && !app.right_sidebar_focused
                    && !app.bottom_focused;
                let main_color = if main_focused {
                    Color::Green
                } else {
                    Color::DarkGray
                };
                let main_block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(main_color))
                    .title(if main_focused { " chat ● " } else { " chat ○ " });
                let inner_area = main_block.inner(pty_area_outer);
                f.render_widget(main_block, pty_area_outer);

                // Reserve the rightmost column for a vertical scrollbar — the
                // PTY itself renders to (width - 1) and is resized
                // accordingly via the `last_pty_area` propagation below.
                let scrollbar_w: u16 = if inner_area.width > 4 { 1 } else { 0 };
                let pty_area = Rect {
                    x: inner_area.x,
                    y: inner_area.y,
                    width: inner_area.width.saturating_sub(scrollbar_w),
                    height: inner_area.height,
                };
                let scrollbar_area = if scrollbar_w > 0 {
                    Some(Rect {
                        x: inner_area.x + pty_area.width,
                        y: inner_area.y,
                        width: scrollbar_w,
                        height: inner_area.height,
                    })
                } else {
                    None
                };

                last_pty_area = pty_area;
                app.bottom_area = bottom_pane_area.unwrap_or_default();
                app.sidebar_area = left_area_opt.unwrap_or_default();
                app.right_sidebar_area = right_area_opt.unwrap_or_default();
                app.body_bottom_y = body.y + body.height;

                if let Some(sb) = left_area_opt {
                    render_sidebar(f, sb, app);
                }
                if let Some(sb) = right_area_opt {
                    render_files_sidebar(f, sb, app);
                }

                let p = app.tabs[app.active].parser.lock().unwrap_or_else(|p| p.into_inner());
                let term = PseudoTerminal::new(p.screen());
                f.render_widget(term, pty_area);

                // Paint match highlights over the just-rendered PTY when a
                // search is active. Only the currently visible window of
                // vt100's screen needs scanning — fast.
                if app.search.open && !app.search.query.is_empty() {
                    if let Ok(query) = scrollback_text::Query::compile(
                        &app.search.query,
                        app.search.regex_mode,
                    ) {
                        paint_search_highlights(f, p.screen(), pty_area, &query);
                    }
                }

                let scroll_off = p.screen().scrollback();
                drop(p);

                // Vertical scrollbar on the right edge of the chat area.
                // Position maps offset (rows above bottom) → distance from
                // bottom of the bar. content_length stays at SCROLLBACK_LINES
                // for a stable visual range; the thumb auto-sizes from the
                // ratio of viewport_content_length to content_length.
                if let Some(sb_area) = scrollbar_area {
                    let visible = pty_area.height as usize;
                    // Content length = real plain-text buffer of the active
                    // tab, clamped so it's never below the viewport (otherwise
                    // ratatui's math collapses the thumb).
                    let total = active_total_lines(app).max(visible.max(1));
                    let track_max = total.saturating_sub(visible);
                    // position = index of the topmost visible row in content;
                    // bottom of the bar (= live tail) when scroll_off == 0.
                    let position = track_max.saturating_sub(scroll_off);
                    let mut state = ScrollbarState::new(total)
                        .viewport_content_length(visible.max(1))
                        .position(position);
                    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .begin_symbol(Some("▲"))
                        .end_symbol(Some("▼"))
                        .track_symbol(Some("│"))
                        .thumb_symbol("█")
                        .style(if scroll_off > 0 {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        });
                    f.render_stateful_widget(bar, sb_area, &mut state);

                    // Paint a single-cell red marker per match row on top of
                    // the scrollbar track. Lets you see at a glance where in
                    // scrollback the hits are. Dedupe rows so dense clusters
                    // don't redraw repeatedly.
                    if app.search.open && !app.search.matches.is_empty() && sb_area.height > 2 {
                        let track_h = sb_area.height.saturating_sub(2);
                        let track_y0 = sb_area.y + 1;
                        let mut painted = std::collections::HashSet::<u16>::new();
                        for m in &app.search.matches {
                            if total <= 1 {
                                break;
                            }
                            let frac_num =
                                (m.line_idx as u64) * (track_h.saturating_sub(1) as u64);
                            let offset = (frac_num / ((total - 1) as u64)) as u16;
                            let row = track_y0 + offset;
                            if !painted.insert(row) {
                                continue;
                            }
                            f.render_widget(
                                Paragraph::new(Line::from(Span::styled(
                                    "•",
                                    Style::default()
                                        .fg(Color::Red)
                                        .add_modifier(Modifier::BOLD),
                                ))),
                                Rect {
                                    x: sb_area.x,
                                    y: row,
                                    width: 1,
                                    height: 1,
                                },
                            );
                        }
                    }
                }

                let info_tag = if scroll_off > 0 {
                    Some(format!("SCROLL -{}", scroll_off))
                } else if !app.mouse_on {
                    Some("MOUSE OFF · Shift+select".to_string())
                } else {
                    None
                };
                // Bottom terminal pane (drawn after main pty, before overlays)
                if let Some(area) = bottom_pane_area {
                    render_bottom_pane(f, area, app);
                }

                let (line, hits) = render_button_bar(chunks[3], app, info_tag.as_deref());
                app.button_hits = hits;
                f.render_widget(
                    Paragraph::new(line).style(Style::default().bg(Color::DarkGray)),
                    chunks[3],
                );

                // Scrollback search bar — overlays the bottom 2 rows of the
                // PTY area when active. Drawn before the global modals so
                // browser/palette/help still take precedence.
                if app.search.open {
                    render_search_overlay(f, pty_area, app);
                }

                // Overlays — drawn last so they sit on top of everything.
                if app.browser_open {
                    render_browser(f, f.area(), app);
                }
                if app.palette_open {
                    render_palette(f, f.area(), app);
                }
                if app.help_open {
                    render_help(f, f.area());
                }
                if app.global_sessions.open {
                    render_global_sessions(f, f.area(), app);
                }
                if app.rename_open {
                    render_rename_modal(f, f.area(), app);
                }
                if app.save_as_open {
                    render_save_as_modal(f, f.area(), app);
                }
                if app.confirm.open {
                    render_confirm_modal(f, f.area(), app);
                }
                if app.broadcast.open {
                    render_broadcast_modal(f, f.area(), app);
                }
                if app.usage_open {
                    render_usage_modal(f, f.area(), app);
                }
                if app.diff_open {
                    render_diff_modal(f, f.area(), app);
                }
            })?;
            needs_draw = false;
        }

        // After draw, last_pty_area is current. Resize tabs if it changed.
        let want = (last_pty_area.height.max(1), last_pty_area.width.max(1));
        if applied_pty_size != Some(want) && want.0 > 0 && want.1 > 0 {
            app.resize_all(want.0, want.1)?;
            applied_pty_size = Some(want);
        }

        // Resize bottom pane too if open and area changed. Bottom area is
        // outer (includes Borders::TOP), so subtract 1 for the inner PTY size.
        if app.bottom_open && app.bottom_area.width > 0 && app.bottom_area.height > 0 {
            if let Some(bt) = app.bottom.as_mut() {
                let inner_h = app.bottom_area.height.saturating_sub(1).max(1);
                let _ = bt.resize(inner_h, app.bottom_area.width.max(1));
            }
        }

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    match handle_key(k, app, last_pty_area)? {
                        KeyOutcome::Quit => return Ok(()),
                        KeyOutcome::LayoutChanged => {
                            terminal.clear()?;
                            needs_draw = true;
                            applied_pty_size = None;
                        }
                        KeyOutcome::Continue => {
                            needs_draw = true;
                        }
                    }
                }
                Event::Mouse(me) => {
                    if let Some(action) = handle_mouse(me, app, last_pty_area)? {
                        match execute_action(action, app, last_pty_area)? {
                            KeyOutcome::Quit => return Ok(()),
                            KeyOutcome::LayoutChanged => {
                                terminal.clear()?;
                                needs_draw = true;
                                applied_pty_size = None;
                            }
                            KeyOutcome::Continue => {
                                needs_draw = true;
                            }
                        }
                    } else {
                        needs_draw = true;
                    }
                }
                Event::Resize(cols, rows) => {
                    let new_rows = rows.saturating_sub(CHROME_ROWS).max(1);
                    let _ = new_rows;
                    let _ = cols;
                    // Actual size is recomputed from the next draw; just clear and redraw.
                    terminal.clear()?;
                    needs_draw = true;
                    applied_pty_size = None;
                }
                Event::Paste(s) => {
                    // Forward as bracketed paste so the inner program treats it
                    // as a single chunk (no premature Enter on newlines).
                    let mut bytes = Vec::with_capacity(s.len() + 12);
                    bytes.extend_from_slice(b"\x1b[200~");
                    bytes.extend_from_slice(s.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    if app.bottom_open && app.bottom_focused {
                        if let Some(bt) = app.bottom.as_mut() {
                            bt.write_input(&bytes)?;
                        }
                    } else {
                        app.active_tab().scroll_reset();
                        app.active_tab().write_input(&bytes)?;
                    }
                    needs_draw = true;
                }
                _ => {}
            }
        }
    }
}

// ===========================================================================
// Project bar (row 0)
// ===========================================================================
fn render_project_bar(
    projects: &[PathBuf],
    active_idx: usize,
    pinned: &std::collections::HashSet<PathBuf>,
    area: Rect,
) -> (Line<'static>, Vec<(Rect, usize)>, Option<Rect>) {
    let mut spans: Vec<Span> = Vec::new();
    let mut rects: Vec<(Rect, usize)> = Vec::with_capacity(projects.len());
    let mut x = area.x;
    let limit = area.x + area.width;

    // Hint at the leftmost cell so the row is identifiable.
    let prefix = " projects: ";
    spans.push(Span::styled(
        prefix,
        Style::default().bg(Color::Black).fg(Color::DarkGray),
    ));
    x = x.saturating_add(prefix.chars().count() as u16);

    for (i, p) in projects.iter().enumerate() {
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| p.to_str().unwrap_or(""))
            .to_string();
        let pin_prefix = if pinned.contains(p) { "📌 " } else { "" };
        let label = format!(" {}{}: {} ", pin_prefix, i + 1, truncate(&name, 20));
        let w = label.chars().count() as u16;
        if x.saturating_add(w) > limit {
            break;
        }
        let style = if i == active_idx {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::Rgb(40, 40, 50)).fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
        rects.push((
            Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
            i,
        ));
        x = x.saturating_add(w + 1);
    }

    // "+ project" button: opens the file browser so the user can pick a dir.
    let plus_label = " + project ";
    let plus_w = plus_label.chars().count() as u16;
    let new_project_rect = if x.saturating_add(plus_w) <= limit {
        spans.push(Span::styled(
            plus_label,
            Style::default()
                .bg(Color::Rgb(40, 40, 50))
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        Some(Rect {
            x,
            y: area.y,
            width: plus_w,
            height: 1,
        })
    } else {
        None
    };

    (Line::from(spans), rects, new_project_rect)
}

// ===========================================================================
// Chat bar (row 1) — filtered to the active project's chats.
// ===========================================================================
fn render_chat_bar(
    tabs: &[ChatTab],
    chat_indices: &[usize],
    states: &[TabState],
    active_global: usize,
    area: Rect,
) -> (Line<'static>, Vec<(Rect, usize)>, Rect) {
    let mut spans: Vec<Span> = Vec::new();
    let mut rects: Vec<(Rect, usize)> = Vec::with_capacity(chat_indices.len());
    let mut x = area.x;

    for (display_pos, &global_idx) in chat_indices.iter().enumerate() {
        let t = &tabs[global_idx];
        let state = states.get(global_idx).copied().unwrap_or(TabState::Idle);
        let (dot, dot_style) = match state {
            TabState::Idle => (None, None),
            TabState::Streaming => (
                Some(" ● "),
                Some(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            ),
            TabState::AwaitingPermission => (
                Some(" ! "),
                Some(
                    Style::default()
                        .fg(Color::Red)
                        .bg(Color::Black)
                        .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
                ),
            ),
        };

        let is_active = global_idx == active_global;
        let unread = if is_active { 0 } else { t.unread_replies };
        let badge = if unread == 0 {
            None
        } else if unread > 99 {
            Some(" [99+] ".to_string())
        } else {
            Some(format!(" [{}] ", unread))
        };

        let pin_prefix = if t.pinned { "📌 " } else { "" };
        // Per-project chat number (display_pos + 1), not global tab index.
        let label = format!(" {}{}: {} ", pin_prefix, display_pos + 1, t.title);
        let label_w = label.chars().count() as u16;
        let dot_w = dot.map(|s| s.chars().count() as u16).unwrap_or(0);
        let badge_w = badge.as_ref().map(|s| s.chars().count() as u16).unwrap_or(0);
        let total_w = label_w + dot_w + badge_w;

        let label_style = if is_active {
            Style::default()
                .bg(Color::White)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::DarkGray).fg(Color::Gray)
        };
        let label_bg = if is_active { Color::White } else { Color::DarkGray };

        spans.push(Span::styled(label, label_style));
        if let Some(b) = badge {
            spans.push(Span::styled(
                b,
                Style::default()
                    .bg(label_bg)
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if let (Some(d), Some(ds)) = (dot, dot_style) {
            spans.push(Span::styled(d, ds.bg(label_bg)));
        }
        spans.push(Span::raw(" "));
        rects.push((
            Rect {
                x,
                y: area.y,
                width: total_w,
                height: 1,
            },
            global_idx,
        ));
        x = x.saturating_add(total_w + 1);
    }

    let plus = " + ";
    let new_rect = Rect {
        x,
        y: area.y,
        width: plus.chars().count() as u16,
        height: 1,
    };
    spans.push(Span::styled(
        plus,
        Style::default().bg(Color::DarkGray).fg(Color::Green),
    ));

    (Line::from(spans), rects, new_rect)
}

// ===========================================================================
// Global-sessions modal key handler (Shift+F3)
// ===========================================================================
fn handle_global_sessions_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    pty_area: Rect,
) -> Result<KeyOutcome> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    match k.code {
        KeyCode::Esc => {
            if !app.global_sessions.filter.is_empty() {
                app.global_sessions.filter.clear();
                app.rebuild_global_entries();
            } else {
                app.global_sessions.clear();
                return Ok(KeyOutcome::LayoutChanged);
            }
        }
        KeyCode::Up => app.global_sessions.step(-1),
        KeyCode::Down => app.global_sessions.step(1),
        KeyCode::PageUp => {
            for _ in 0..10 {
                app.global_sessions.step(-1);
            }
        }
        KeyCode::PageDown => {
            for _ in 0..10 {
                app.global_sessions.step(1);
            }
        }
        KeyCode::Home => {
            app.global_sessions.idx = 0;
            if !app.global_sessions.is_selectable(0)
                && !app.global_sessions.entries.is_empty()
            {
                app.global_sessions.step(1);
            }
        }
        KeyCode::End => {
            app.global_sessions.idx = app.global_sessions.entries.len().saturating_sub(1);
            if !app.global_sessions.is_selectable(app.global_sessions.idx)
                && !app.global_sessions.entries.is_empty()
            {
                app.global_sessions.step(-1);
            }
        }
        KeyCode::Enter => {
            if let Some(sess_idx) = app.global_sessions_selected_session() {
                let scrollback = app.config.layout.scrollback_lines;
                let (cwd, id, title) = {
                    let s = &app.sessions[sess_idx];
                    (s.cwd.clone(), s.id.clone(), truncate(&s.title, 24))
                };
                let tab = ChatTab::spawn_resume(
                    cwd,
                    &id,
                    title,
                    pty_area.height.max(1),
                    pty_area.width.max(1),
                    scrollback,
                )?;
                app.tabs.push(tab);
                app.active = app.tabs.len() - 1;
                app.on_active_changed();
                app.save_layout();
                app.global_sessions.clear();
                return Ok(KeyOutcome::LayoutChanged);
            }
        }
        // Ctrl+R refresh — picks up new/closed sessions without closing.
        KeyCode::Char('r') | KeyCode::Char('R') if ctrl => {
            app.refresh_sessions();
            app.rebuild_global_entries();
        }
        KeyCode::Backspace if app.global_sessions.filter.pop().is_some() => {
            app.rebuild_global_entries();
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            app.global_sessions.filter.push(c);
            app.rebuild_global_entries();
        }
        _ => {}
    }
    Ok(KeyOutcome::Continue)
}

// ===========================================================================
// Broadcast modal key handler — Enter blasts the prompt to every chat in
// the active project, Esc cancels.
// ===========================================================================
fn handle_broadcast_key(k: crossterm::event::KeyEvent, app: &mut App) -> Result<KeyOutcome> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    match k.code {
        KeyCode::Esc => {
            app.broadcast.clear();
            return Ok(KeyOutcome::LayoutChanged);
        }
        KeyCode::Enter => {
            let _ = app.apply_broadcast();
            app.broadcast.clear();
            return Ok(KeyOutcome::LayoutChanged);
        }
        KeyCode::Backspace if app.broadcast.input.pop().is_some() => {}
        KeyCode::Char('u') if ctrl => app.broadcast.input.clear(),
        KeyCode::Char(c) if !ctrl && !alt => {
            app.broadcast.input.push(c);
        }
        _ => {}
    }
    Ok(KeyOutcome::Continue)
}

// ===========================================================================
// Confirm modal key handler
// ===========================================================================
fn handle_confirm_key(k: crossterm::event::KeyEvent, app: &mut App) -> Result<KeyOutcome> {
    match k.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            app.apply_confirm();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.confirm.clear();
        }
        _ => {}
    }
    Ok(KeyOutcome::LayoutChanged)
}

// ===========================================================================
// Save-layout-as modal key handler
// ===========================================================================
fn handle_save_as_key(k: crossterm::event::KeyEvent, app: &mut App) -> Result<KeyOutcome> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    match k.code {
        KeyCode::Esc => {
            app.close_save_as();
            return Ok(KeyOutcome::LayoutChanged);
        }
        KeyCode::Enter => {
            app.apply_save_as();
            // If apply_save_as succeeded the modal is closed; otherwise it
            // stays open with `save_as_error` populated.
            return Ok(KeyOutcome::LayoutChanged);
        }
        KeyCode::Backspace if app.save_as_input.pop().is_some() => {
            app.save_as_error = None;
        }
        KeyCode::Char('u') if ctrl => {
            app.save_as_input.clear();
            app.save_as_error = None;
        }
        KeyCode::Char(c)
            if !ctrl && !alt && app.save_as_input.chars().count() < 60 =>
        {
            app.save_as_input.push(c);
            app.save_as_error = None;
        }
        _ => {}
    }
    Ok(KeyOutcome::Continue)
}

// ===========================================================================
// Rename-tab modal key handler (Shift+F2)
// ===========================================================================
fn handle_rename_key(k: crossterm::event::KeyEvent, app: &mut App) -> Result<KeyOutcome> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    match k.code {
        KeyCode::Esc => {
            app.close_rename();
            return Ok(KeyOutcome::LayoutChanged);
        }
        KeyCode::Enter => {
            app.apply_rename();
            return Ok(KeyOutcome::LayoutChanged);
        }
        KeyCode::Backspace if app.rename_input.pop().is_some() => {}
        // Ctrl+U clears the field — handy when you want to reset to default.
        KeyCode::Char('u') if ctrl => app.rename_input.clear(),
        KeyCode::Char(c)
            if !ctrl && !alt && app.rename_input.chars().count() < 60 =>
        {
            app.rename_input.push(c);
        }
        _ => {}
    }
    Ok(KeyOutcome::Continue)
}

// ===========================================================================
// Search-overlay key handler (Ctrl+F)
// ===========================================================================
fn handle_search_key(k: crossterm::event::KeyEvent, app: &mut App) -> Result<KeyOutcome> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    match k.code {
        KeyCode::Esc => {
            app.toggle_search();
            return Ok(KeyOutcome::LayoutChanged);
        }
        KeyCode::Enter => {
            app.search_next();
        }
        KeyCode::Up => app.search_prev(),
        KeyCode::Down => app.search_next(),
        // n/N work as case-sensitive next/prev like in vi-style searches.
        KeyCode::Char('n') if ctrl => app.search_next(),
        KeyCode::Char('p') if ctrl => app.search_prev(),
        // Alt+R — toggle regex mode and re-compile.
        KeyCode::Char('r') | KeyCode::Char('R') if alt => {
            app.search.regex_mode = !app.search.regex_mode;
            app.search_rerun();
        }
        // Empty query + Backspace is a no-op; Esc is the documented close.
        KeyCode::Backspace if app.search.query.pop().is_some() => {
            app.search_rerun();
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            app.search.query.push(c);
            app.search_rerun();
        }
        _ => {}
    }
    Ok(KeyOutcome::Continue)
}

// ===========================================================================
// Palette key handler
// ===========================================================================
fn handle_palette_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    pty_area: Rect,
) -> Result<KeyOutcome> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    match k.code {
        KeyCode::Esc => {
            app.close_palette();
            return Ok(KeyOutcome::LayoutChanged);
        }
        KeyCode::Up => {
            app.palette_idx = app.palette_idx.saturating_sub(1);
        }
        KeyCode::Down => {
            let max = app.palette_filtered.len().saturating_sub(1);
            app.palette_idx = (app.palette_idx + 1).min(max);
        }
        KeyCode::PageUp => {
            app.palette_idx = app.palette_idx.saturating_sub(10);
        }
        KeyCode::PageDown => {
            let max = app.palette_filtered.len().saturating_sub(1);
            app.palette_idx = (app.palette_idx + 10).min(max);
        }
        KeyCode::Home => {
            app.palette_idx = 0;
        }
        KeyCode::End => {
            app.palette_idx = app.palette_filtered.len().saturating_sub(1);
        }
        KeyCode::Enter => {
            let result = app.palette_take_selection();
            app.close_palette();
            match result {
                PaletteResult::None => return Ok(KeyOutcome::LayoutChanged),
                PaletteResult::Run(a) => return execute_action(a, app, pty_area),
                PaletteResult::OpenSession(idx) => {
                    let scrollback = app.config.layout.scrollback_lines;
                    let (cwd, id, title) = {
                        let s = &app.sessions[idx];
                        (s.cwd.clone(), s.id.clone(), truncate(&s.title, 24))
                    };
                    let tab = ChatTab::spawn_resume(
                        cwd,
                        &id,
                        title,
                        pty_area.height.max(1),
                        pty_area.width.max(1),
                        scrollback,
                    )?;
                    app.tabs.push(tab);
                    app.active = app.tabs.len() - 1;
                    app.on_active_changed();
                    app.save_layout();
                    return Ok(KeyOutcome::LayoutChanged);
                }
                PaletteResult::SwitchLayout(i) => {
                    if let Some(name) = app.layout_names.get(i).cloned() {
                        app.switch_to_layout(
                            &name,
                            pty_area.height.max(1),
                            pty_area.width.max(1),
                        )?;
                    }
                    return Ok(KeyOutcome::LayoutChanged);
                }
                PaletteResult::DeleteLayout(i) => {
                    if let Some(name) = app.layout_names.get(i).cloned() {
                        let _ = layout::delete_named(&name);
                    }
                    return Ok(KeyOutcome::LayoutChanged);
                }
            }
        }
        KeyCode::Backspace if app.palette_query.pop().is_some() => {
            app.apply_palette_filter();
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            app.palette_query.push(c);
            app.apply_palette_filter();
        }
        _ => {}
    }
    Ok(KeyOutcome::Continue)
}

// ===========================================================================
// Files-sidebar key handler — same actions as F6 modal but Esc closes sidebar.
// ===========================================================================
fn handle_files_sidebar_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    pty_area: Rect,
) -> Result<KeyOutcome> {
    let Some(br) = app.browser.as_mut() else {
        app.right_sidebar_open = false;
        return Ok(KeyOutcome::LayoutChanged);
    };

    match k.code {
        KeyCode::Esc => {
            app.right_sidebar_focused = false;
            return Ok(KeyOutcome::Continue);
        }
        KeyCode::Up => br.move_up(),
        KeyCode::Down => br.move_down(),
        KeyCode::PageUp => br.page_up(),
        KeyCode::PageDown => br.page_down(),
        KeyCode::Home => br.home(),
        KeyCode::End => br.end(),
        KeyCode::Left | KeyCode::Backspace => br.cd_parent(),
        KeyCode::Right => {
            let name = match br.selected() {
                Some(BrowserEntry::Dir(n)) => Some(n.clone()),
                _ => None,
            };
            if let Some(n) = name {
                br.cd_into(&n);
            }
        }
        KeyCode::Char(' ') => {
            // Insert the selected path into the active claude's input.
            let path = match br.selected() {
                Some(BrowserEntry::Dir(n)) | Some(BrowserEntry::File(n)) => {
                    Some(br.cwd.join(n))
                }
                Some(BrowserEntry::OpenHere) => Some(br.cwd.clone()),
                Some(BrowserEntry::Parent)
                | Some(BrowserEntry::Drive(_))
                | None => None,
            };
            if let Some(p) = path {
                let root = app.tabs[app.active].cwd.clone();
                let text = format_path_for_pty(&p, &root, false);
                app.active_tab().write_input(text.as_bytes())?;
            }
        }
        KeyCode::Enter => {
            let action = match br.selected() {
                Some(BrowserEntry::OpenHere) => Some(BrowserAction::OpenHere),
                Some(BrowserEntry::Parent) => Some(BrowserAction::CdParent),
                Some(BrowserEntry::Dir(n)) => Some(BrowserAction::CdInto(n.clone())),
                Some(BrowserEntry::File(_))
                | Some(BrowserEntry::Drive(_))
                | None => None,
            };
            match action {
                Some(BrowserAction::OpenHere) => {
                    let cwd = br.cwd.clone();
                    app.spawn_tab_here(cwd, pty_area.height.max(1), pty_area.width.max(1))?;
                    app.on_active_changed();
                    app.save_layout();
                    return Ok(KeyOutcome::Continue);
                }
                Some(BrowserAction::CdParent) => br.cd_parent(),
                Some(BrowserAction::CdInto(n)) => br.cd_into(&n),
                // Drives-root is unreachable when chrooted; keep the match exhaustive.
                Some(BrowserAction::CdDrive(_)) | None => {}
            }
        }
        _ => {}
    }
    Ok(KeyOutcome::Continue)
}

fn format_path_for_pty(p: &std::path::Path, root: &std::path::Path, absolute: bool) -> String {
    let s = if absolute {
        p.to_string_lossy().replace('\\', "/")
    } else {
        // Prefer a path relative to the active tab's cwd; fall back to absolute
        // if `p` is outside `root` for some reason.
        match p.strip_prefix(root) {
            Ok(rel) if rel.as_os_str().is_empty() => "./".to_string(),
            Ok(rel) => {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                format!("./{}", rel_str)
            }
            Err(_) => p.to_string_lossy().replace('\\', "/"),
        }
    };
    let needs_quote = s.contains(' ') || s.contains('"') || s.contains('\t');
    if needs_quote {
        format!("\"{}\" ", s.replace('"', "\\\""))
    } else {
        format!("{} ", s)
    }
}

// ===========================================================================
// Browser key handler
// ===========================================================================
fn handle_browser_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    pty_area: Rect,
) -> Result<KeyOutcome> {
    let Some(br) = app.browser.as_mut() else {
        app.browser_open = false;
        return Ok(KeyOutcome::LayoutChanged);
    };
    match k.code {
        KeyCode::Esc | KeyCode::F(6) => {
            app.browser_open = false;
            return Ok(KeyOutcome::LayoutChanged);
        }
        KeyCode::Up => br.move_up(),
        KeyCode::Down => br.move_down(),
        KeyCode::PageUp => br.page_up(),
        KeyCode::PageDown => br.page_down(),
        KeyCode::Home => br.home(),
        KeyCode::End => br.end(),
        KeyCode::Left | KeyCode::Backspace => br.cd_parent(),
        KeyCode::Right => {
            let action = match br.selected() {
                Some(BrowserEntry::Dir(n)) => Some(BrowserAction::CdInto(n.clone())),
                Some(BrowserEntry::Drive(l)) => Some(BrowserAction::CdDrive(l.clone())),
                _ => None,
            };
            match action {
                Some(BrowserAction::CdInto(n)) => br.cd_into(&n),
                Some(BrowserAction::CdDrive(l)) => br.cd_drive(&l),
                _ => {}
            }
        }
        KeyCode::Char(' ') => {
            // Insert absolute path of the selected entry into active claude.
            // Modal stays open so the user can pick several entries.
            let path = match br.selected() {
                Some(BrowserEntry::Dir(n)) | Some(BrowserEntry::File(n)) => {
                    Some(br.cwd.join(n))
                }
                Some(BrowserEntry::OpenHere) => Some(br.cwd.clone()),
                Some(BrowserEntry::Drive(l)) => Some(std::path::PathBuf::from(format!("{}:\\", l))),
                Some(BrowserEntry::Parent) | None => None,
            };
            if let Some(p) = path {
                let text = format_path_for_pty(&p, &p, true);
                app.active_tab().write_input(text.as_bytes())?;
            }
        }
        KeyCode::Enter => {
            // capture what we need, then release the borrow on browser
            let action = match br.selected() {
                Some(BrowserEntry::OpenHere) => Some(BrowserAction::OpenHere),
                Some(BrowserEntry::Parent) => Some(BrowserAction::CdParent),
                Some(BrowserEntry::Dir(n)) => Some(BrowserAction::CdInto(n.clone())),
                Some(BrowserEntry::Drive(l)) => Some(BrowserAction::CdDrive(l.clone())),
                Some(BrowserEntry::File(_)) | None => None,
            };
            match action {
                Some(BrowserAction::OpenHere) => {
                    let cwd = br.cwd.clone();
                    app.browser_open = false;
                    app.spawn_tab_here(cwd, pty_area.height.max(1), pty_area.width.max(1))?;
                    app.save_layout();
                    return Ok(KeyOutcome::LayoutChanged);
                }
                Some(BrowserAction::CdParent) => br.cd_parent(),
                Some(BrowserAction::CdInto(n)) => br.cd_into(&n),
                Some(BrowserAction::CdDrive(l)) => br.cd_drive(&l),
                None => {}
            }
        }
        _ => {}
    }
    Ok(KeyOutcome::Continue)
}

enum BrowserAction {
    OpenHere,
    CdParent,
    CdInto(String),
    CdDrive(String),
}

// ===========================================================================
// Browser rendering — modal overlay centered on the screen.
// ===========================================================================
fn render_browser(f: &mut ratatui::Frame, full_area: Rect, app: &mut App) {
    let Some(br) = app.browser.as_mut() else {
        return;
    };

    let w = (full_area.width as u32 * 7 / 10).max(50) as u16;
    let w = w.min(full_area.width);
    let h = (full_area.height as u32 * 7 / 10).max(15) as u16;
    let h = h.min(full_area.height);
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let title = format!(" Browse  ·  {}  ·  Esc close ", browser::path_label(&br.cwd));
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .style(Style::default().bg(Color::Rgb(20, 20, 24)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // hint
            Constraint::Min(1),    // list
        ])
        .split(inner);

    let hint = " Enter open · Space → claude (absolute) · → cd · ← parent (← at drive root → drives) · F6 close ";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[0],
    );

    let list_area = chunks[1];
    if let Some(err) = &br.error {
        f.render_widget(
            Paragraph::new(format!(" ! {} ", err)).style(Style::default().fg(Color::Red)),
            list_area,
        );
        return;
    }

    let visible = list_area.height as usize;
    if br.idx < br.scroll {
        br.scroll = br.idx;
    } else if br.idx >= br.scroll + visible && visible > 0 {
        br.scroll = br.idx + 1 - visible;
    }

    let max_w = list_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(visible);
    for i in 0..visible {
        let e_idx = br.scroll + i;
        let Some(e) = br.entries.get(e_idx) else {
            break;
        };
        let selected = e_idx == br.idx;
        let base = match e {
            BrowserEntry::OpenHere => {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            }
            BrowserEntry::Parent => Style::default().fg(Color::Yellow),
            BrowserEntry::Dir(_) => Style::default().fg(Color::Cyan),
            BrowserEntry::File(_) => Style::default().fg(Color::White),
            BrowserEntry::Drive(_) => {
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)
            }
        };
        let style = if selected {
            base.bg(Color::White).fg(Color::Black).add_modifier(Modifier::BOLD)
        } else {
            base
        };
        let label = truncate(&e.label(), max_w.saturating_sub(2));
        lines.push(Line::from(Span::styled(format!(" {} ", label), style)));
    }
    f.render_widget(Paragraph::new(lines), list_area);
}

// ===========================================================================
// Global sessions modal — every session grouped by cwd.
// ===========================================================================
fn render_global_sessions(f: &mut ratatui::Frame, full_area: Rect, app: &mut App) {
    let w = (full_area.width as u32 * 8 / 10).max(60) as u16;
    let w = w.min(full_area.width);
    let h = (full_area.height as u32 * 85 / 100).max(15) as u16;
    let h = h.min(full_area.height);
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let total_sessions = app.sessions.len();
    let session_count: usize = app
        .global_sessions
        .entries
        .iter()
        .filter(|e| matches!(e, GlobalEntry::Session(_)))
        .count();
    let group_count: usize = app
        .global_sessions
        .entries
        .iter()
        .filter(|e| matches!(e, GlobalEntry::Header(_)))
        .count();
    let title = format!(
        " Global sessions  ·  {} in {} dirs  ·  Esc close  ·  Ctrl+R refresh ",
        session_count, group_count
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(20, 20, 24)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // search input
            Constraint::Length(1), // count line
            Constraint::Min(1),    // list
        ])
        .split(inner);

    let max_visible = (chunks[0].width as usize).saturating_sub(4);
    let q_display = if app.global_sessions.filter.chars().count() > max_visible {
        let skip = app.global_sessions.filter.chars().count() - max_visible;
        app.global_sessions.filter.chars().skip(skip).collect::<String>()
    } else {
        app.global_sessions.filter.clone()
    };
    let input = Line::from(vec![
        Span::styled(" / ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(q_display, Style::default().fg(Color::White)),
        Span::styled("█", Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(
        Paragraph::new(input).style(Style::default().bg(Color::Rgb(28, 28, 32))),
        chunks[0],
    );

    let count_text = if app.global_sessions.filter.is_empty() {
        format!(" total: {} sessions ", total_sessions)
    } else {
        format!(" {} match in {} dirs ", session_count, group_count)
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            count_text,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[1],
    );

    let list_area = chunks[2];
    if app.global_sessions.entries.is_empty() {
        let msg = if total_sessions == 0 {
            " no sessions in ~/.claude/projects "
        } else {
            " (no matches) "
        };
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            list_area,
        );
        return;
    }

    let visible_rows = list_area.height as usize;
    if app.global_sessions.idx < app.global_sessions.scroll {
        app.global_sessions.scroll = app.global_sessions.idx;
    } else if app.global_sessions.idx >= app.global_sessions.scroll + visible_rows
        && visible_rows > 0
    {
        app.global_sessions.scroll = app.global_sessions.idx + 1 - visible_rows;
    }

    let max_w = list_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(visible_rows);
    for i in 0..visible_rows {
        let entry_idx = app.global_sessions.scroll + i;
        let Some(entry) = app.global_sessions.entries.get(entry_idx) else {
            break;
        };
        match entry {
            GlobalEntry::Header(cwd) => {
                let label = format!(" ── {} ──", truncate(cwd, max_w.saturating_sub(6)));
                lines.push(Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            GlobalEntry::Session(sess_idx) => {
                let selected = entry_idx == app.global_sessions.idx;
                let s = &app.sessions[*sess_idx];
                let branch = s
                    .git_branch
                    .as_deref()
                    .map(|b| format!("·{}", truncate(b, 16)))
                    .unwrap_or_default();
                let counts = if s.message_count > 0 || s.total_tokens > 0 {
                    format!("  ·{}msg·{}", s.message_count, format_tokens(s.total_tokens))
                } else {
                    String::new()
                };
                let row = format!(
                    "    ⤴  {}  ·  {}{}{}",
                    truncate(&s.title, max_w.saturating_sub(40)),
                    sessions::relative_time(s.updated),
                    branch,
                    counts,
                );
                let style = if selected {
                    Style::default()
                        .bg(Color::Cyan)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(Span::styled(row, style)));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), list_area);
}

// ===========================================================================
// Git diff modal — read-only `git diff HEAD` viewer with j/k navigation and
// hunk colouring (added → green, removed → red, hunk header → magenta).
// ===========================================================================
fn render_diff_modal(f: &mut ratatui::Frame, full_area: Rect, app: &mut App) {
    let w = (full_area.width as u32 * 9 / 10).max(60) as u16;
    let w = w.min(full_area.width);
    let h = (full_area.height as u32 * 9 / 10).max(15) as u16;
    let h = h.min(full_area.height);
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let accent = app.accent_color();
    let title = format!(
        "{}·  j/k scroll · g/G top/bottom · r refresh · Esc close ",
        app.diff_title
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(Color::Rgb(18, 20, 22)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible = inner.height as usize;
    if app.diff_scroll + visible > app.diff_lines.len() {
        app.diff_scroll = app.diff_lines.len().saturating_sub(visible);
    }
    let max_w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(visible);
    for i in 0..visible {
        let idx = app.diff_scroll + i;
        let Some(raw) = app.diff_lines.get(idx) else {
            break;
        };
        let trimmed = truncate(raw, max_w);
        // Cheap diff colouring on first char.
        let style = match raw.chars().next() {
            Some('+') if !raw.starts_with("+++") => Style::default().fg(Color::LightGreen),
            Some('-') if !raw.starts_with("---") => Style::default().fg(Color::LightRed),
            Some('@') => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            Some('d') if raw.starts_with("diff ") => {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            }
            Some('i') | Some('n') | Some('+') | Some('-') if raw.starts_with("index ")
                || raw.starts_with("new file")
                || raw.starts_with("---")
                || raw.starts_with("+++")
                || raw.starts_with("deleted file") =>
            {
                Style::default().fg(Color::DarkGray)
            }
            Some('─') => Style::default().fg(accent).add_modifier(Modifier::BOLD),
            _ => Style::default().fg(Color::Gray),
        };
        lines.push(Line::from(Span::styled(trimmed, style)));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// ===========================================================================
// Usage modal — read-only token totals for the active chat.
// ===========================================================================
fn render_usage_modal(f: &mut ratatui::Frame, full_area: Rect, app: &mut App) {
    let w = 80u16.min(full_area.width.saturating_sub(4));
    let h = (app.usage_lines.len() as u16 + 3).max(7);
    if w < 30 || full_area.height < h + 2 {
        return;
    }
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let accent = app.accent_color();
    let block = Block::default()
        .title(" Active chat usage  ·  any key to close ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(Color::Rgb(20, 20, 24)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines: Vec<Line> = app
        .usage_lines
        .iter()
        .map(|l| {
            Line::from(Span::styled(l.clone(), Style::default().fg(Color::White)))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

// ===========================================================================
// Broadcast modal — centered prompt; Enter sends to every chat in project.
// ===========================================================================
fn render_broadcast_modal(f: &mut ratatui::Frame, full_area: Rect, app: &mut App) {
    let w = 76u16.min(full_area.width.saturating_sub(4));
    let h = 6u16;
    if w < 30 || full_area.height < h + 2 {
        return;
    }
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let target_count = {
        let cwd = app.active_project_cwd();
        app.tabs.iter().filter(|t| t.cwd == cwd).count()
    };

    let block = Block::default()
        .title(format!(
            " Broadcast prompt → {} chat(s) in active project · Enter send · Esc cancel ",
            target_count
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .style(Style::default().bg(Color::Rgb(20, 18, 28)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let basename = app
        .active_project_cwd()
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" project: {} ", basename),
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[0],
    );

    let max_visible = (chunks[1].width as usize).saturating_sub(4);
    let display = if app.broadcast.input.chars().count() > max_visible {
        let skip = app.broadcast.input.chars().count() - max_visible;
        app.broadcast.input.chars().skip(skip).collect::<String>()
    } else {
        app.broadcast.input.clone()
    };
    let input = Line::from(vec![
        Span::styled(" ▶ ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::styled(display, Style::default().fg(Color::White)),
        Span::styled("█", Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(
        Paragraph::new(input).style(Style::default().bg(Color::Rgb(28, 28, 36))),
        chunks[1],
    );

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Ctrl+U clears · empty input = no-op ",
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );
}

// ===========================================================================
// Confirm modal — small centered Y/N prompt.
// ===========================================================================
fn render_confirm_modal(f: &mut ratatui::Frame, full_area: Rect, app: &mut App) {
    let w = 72u16.min(full_area.width.saturating_sub(4));
    let h = 5u16;
    if w < 20 || full_area.height < h + 2 {
        return;
    }
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm  ·  Y / Enter — yes · N / Esc — no ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .style(Style::default().bg(Color::Rgb(30, 20, 20)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let msg = truncate(&app.confirm.message, w as usize - 4);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {} ", msg),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Press Y to confirm · N to cancel ",
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[1],
    );
}

// ===========================================================================
// Save-layout-as modal — centered prompt for a layout name.
// ===========================================================================
fn render_save_as_modal(f: &mut ratatui::Frame, full_area: Rect, app: &mut App) {
    let w = 64u16.min(full_area.width.saturating_sub(4));
    let h = 7u16;
    if w < 12 || full_area.height < h + 2 {
        return;
    }
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let accent = app.accent_color();
    let block = Block::default()
        .title(" Save layout as  ·  Enter save · Esc cancel ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(Color::Rgb(20, 20, 24)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {} tabs in current layout ", app.tabs.len()),
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[0],
    );

    let max_visible = (chunks[1].width as usize).saturating_sub(4);
    let q_display = if app.save_as_input.chars().count() > max_visible {
        let skip = app.save_as_input.chars().count() - max_visible;
        app.save_as_input.chars().skip(skip).collect::<String>()
    } else {
        app.save_as_input.clone()
    };
    let input = Line::from(vec![
        Span::styled(" ▶ ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(q_display, Style::default().fg(Color::White)),
        Span::styled("█", Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(
        Paragraph::new(input).style(Style::default().bg(Color::Rgb(28, 28, 32))),
        chunks[1],
    );

    // Sanitised preview so users see what the on-disk name will look like.
    let preview = layout::sanitize_name(&app.save_as_input);
    let preview_line = if preview.is_empty() {
        Span::styled(" (empty)", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(
            format!(" → {}.json", preview),
            Style::default().fg(Color::Green),
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(preview_line)),
        chunks[2],
    );

    let footer = if let Some(err) = &app.save_as_error {
        Line::from(Span::styled(
            format!(" ! {}", err),
            Style::default().fg(Color::Red),
        ))
    } else {
        Line::from(Span::styled(
            " Ctrl+U clears · `/` `\\` `:` are stripped ",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(footer), chunks[3]);
}

// ===========================================================================
// Rename-tab modal — small centered prompt.
// ===========================================================================
fn render_rename_modal(f: &mut ratatui::Frame, full_area: Rect, app: &mut App) {
    let w = 60u16.min(full_area.width.saturating_sub(4));
    let h = 5u16;
    if w < 10 || full_area.height < h + 2 {
        return;
    }
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let block = Block::default()
        .title(" Rename tab  ·  Enter apply · Esc cancel ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Rgb(20, 20, 24)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let label = format!(
        " Active tab: {} ",
        truncate(&app.tabs[app.active].title, w as usize - 14)
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[0],
    );

    let max_visible = (chunks[1].width as usize).saturating_sub(4);
    let q_display = if app.rename_input.chars().count() > max_visible {
        let skip = app.rename_input.chars().count() - max_visible;
        app.rename_input.chars().skip(skip).collect::<String>()
    } else {
        app.rename_input.clone()
    };
    let input = Line::from(vec![
        Span::styled(" ▶ ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(q_display, Style::default().fg(Color::White)),
        Span::styled("█", Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(
        Paragraph::new(input).style(Style::default().bg(Color::Rgb(28, 28, 32))),
        chunks[1],
    );

    let hint = " Empty + Enter → reset to cwd basename · Ctrl+U clears ";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );
}

// ===========================================================================
// In-PTY match highlighting for the active scrollback search. Iterates the
// vt100 screen's currently visible rows (so this honours scrollback offset
// set by `apply_search_jump`), finds matches per row, and overpaints each
// match cell-range with a yellow background.
//
// Limitations:
// - Matches that span a line wrap aren't found (each row is scanned alone).
// - The bottom 2 rows are usually obscured by the search overlay; we still
//   paint them in case the user has a tall terminal where the overlay
//   doesn't cover everything.
// ===========================================================================
fn paint_search_highlights(
    f: &mut ratatui::Frame,
    screen: &vt100::Screen,
    pty_area: Rect,
    query: &scrollback_text::Query,
) {
    let (rows, cols) = screen.size();
    let visible_rows = pty_area.height.min(rows);

    for row in 0..visible_rows {
        // Build the row's visible text plus a parallel byte→column map so
        // multibyte characters don't misalign the highlight rectangle.
        let mut row_cells: Vec<String> = Vec::with_capacity(cols as usize);
        for col in 0..cols {
            let c = screen
                .cell(row, col)
                .map(|c| c.contents().to_string())
                .unwrap_or_default();
            row_cells.push(if c.is_empty() { " ".into() } else { c });
        }
        let row_text: String = row_cells.concat();
        if row_text.is_empty() {
            continue;
        }

        // byte_to_col[i] is the column index of the cell containing byte i.
        let mut byte_to_col: Vec<u16> = Vec::with_capacity(row_text.len() + 1);
        for (col, s) in row_cells.iter().enumerate() {
            for _ in 0..s.len() {
                byte_to_col.push(col as u16);
            }
        }
        byte_to_col.push(cols);

        // Per-row match ranges as (byte_start, byte_end). Both query kinds
        // produce these uniformly; the rest of the loop paints them.
        let ranges: Vec<(usize, usize)> = match query {
            scrollback_text::Query::Substring(q_lower) => {
                let hay_lower = row_text.to_lowercase();
                let mut out = Vec::new();
                let mut start = 0;
                while let Some(rel) = hay_lower[start..].find(q_lower.as_str()) {
                    let abs = start + rel;
                    let end = (abs + q_lower.len()).min(row_text.len());
                    if end > abs {
                        out.push((abs, end));
                    }
                    // Advance by at least one byte to avoid an infinite loop.
                    let step = q_lower
                        .chars()
                        .next()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                    start = abs + step;
                    if start > hay_lower.len() {
                        break;
                    }
                }
                out
            }
            scrollback_text::Query::Regex(re) => re
                .find_iter(&row_text)
                .filter(|m| !m.range().is_empty())
                .map(|m| (m.start(), m.end()))
                .collect(),
        };

        for (b_start, b_end) in ranges {
            let col_start = byte_to_col.get(b_start).copied().unwrap_or(0);
            let col_end = byte_to_col.get(b_end).copied().unwrap_or(cols);
            let width = col_end.saturating_sub(col_start);
            if width == 0 || col_start >= pty_area.width {
                continue;
            }
            let w = width.min(pty_area.width - col_start);
            let highlight_text: String = row_cells
                [col_start as usize..(col_start + w) as usize]
                .concat();
            let rect = Rect {
                x: pty_area.x + col_start,
                y: pty_area.y + row,
                width: w,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    highlight_text,
                    Style::default()
                        .bg(Color::Yellow)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                ))),
                rect,
            );
        }
    }
}

// ===========================================================================
// Scrollback search overlay — pinned to the bottom 2 rows of the PTY area.
// ===========================================================================
fn render_search_overlay(f: &mut ratatui::Frame, pty_area: Rect, app: &mut App) {
    if pty_area.height < 2 {
        return;
    }
    let bar_area = Rect {
        x: pty_area.x,
        y: pty_area.y + pty_area.height - 2,
        width: pty_area.width,
        height: 2,
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(bar_area);

    // Header row — input + mode + counter/error + hint.
    let mode_badge = if app.search.regex_mode {
        " [regex] "
    } else {
        ""
    };
    let counter = if let Some(err) = &app.search.regex_error {
        format!(" regex error: {} ", truncate(err, 32))
    } else if app.search.query.is_empty() {
        " type to search ".to_string()
    } else if app.search.matches.is_empty() {
        " 0 matches ".to_string()
    } else {
        format!(" {}/{} ", app.search.idx + 1, app.search.matches.len())
    };
    let hint = " ↑/↓ step · Alt+R regex · Esc close ";

    // Reserve room on the right for counter + hint so the query doesn't push
    // them off-screen.
    let right_w = counter.chars().count() + hint.chars().count() + mode_badge.len();
    let q_max = (chunks[0].width as usize).saturating_sub(right_w + 4);
    let q_display = if app.search.query.chars().count() > q_max {
        let skip = app.search.query.chars().count() - q_max;
        app.search.query.chars().skip(skip).collect::<String>()
    } else {
        app.search.query.clone()
    };

    let counter_style = if app.search.regex_error.is_some() {
        Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    };

    let header = Line::from(vec![
        Span::styled(
            " 🔍 ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            mode_badge,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(q_display, Style::default().fg(Color::White)),
        Span::styled("█", Style::default().fg(Color::Gray)),
        Span::raw(" "),
        Span::styled(counter, counter_style),
        Span::styled(hint, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(
        Paragraph::new(header).style(Style::default().bg(Color::Rgb(28, 28, 32))),
        chunks[0],
    );

    // Snippet row — show the current match's line with the matched substring
    // highlighted in yellow. Empty placeholder when no matches.
    let snippet_line = match (
        app.search.tab_idx,
        app.search.matches.get(app.search.idx).copied(),
    ) {
        (Some(tab_idx), Some(m)) => app
            .tabs
            .get(tab_idx)
            .and_then(|t| t.text_buffer.lock().ok().map(|b| (m, build_snippet(&b, m, chunks[1].width as usize))))
            .map(|(_, line)| line)
            .unwrap_or_else(|| Line::raw("")),
        _ => Line::raw(""),
    };
    f.render_widget(
        Paragraph::new(snippet_line).style(Style::default().bg(Color::Rgb(20, 20, 24))),
        chunks[1],
    );
}

/// Build a single styled line: a window around `m.col` with the matched
/// substring highlighted. Width is capped at `max_w` columns.
fn build_snippet(buf: &ScrollbackText, m: scrollback_text::Match, max_w: usize) -> Line<'static> {
    let line = match buf.line(m.line_idx) {
        Some(s) => s.to_string(),
        None => return Line::raw(""),
    };
    // Index into byte positions of the line — m.col is a byte offset because
    // ScrollbackText::find_all uses str::find on bytes.
    let line_len = line.len();
    let match_end = (m.col + m.len).min(line_len);

    // Center the match inside max_w, leaving ~equal context on both sides.
    let context = max_w.saturating_sub(m.len + 6) / 2;
    let win_start = m.col.saturating_sub(context);
    let win_end = (match_end + context).min(line_len);

    // Snap to char boundaries.
    let win_start = floor_char_boundary(&line, win_start);
    let win_end = ceil_char_boundary(&line, win_end);
    let m_col = floor_char_boundary(&line, m.col);
    let m_end = ceil_char_boundary(&line, match_end);

    let prefix_marker = if win_start > 0 { "…" } else { " " };
    let suffix_marker = if win_end < line_len { "…" } else { " " };

    let pre = &line[win_start..m_col];
    let hit = &line[m_col..m_end];
    let post = &line[m_end..win_end];

    Line::from(vec![
        Span::styled(prefix_marker, Style::default().fg(Color::DarkGray)),
        Span::styled(pre.to_string(), Style::default().fg(Color::Gray)),
        Span::styled(
            hit.to_string(),
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(post.to_string(), Style::default().fg(Color::Gray)),
        Span::styled(suffix_marker, Style::default().fg(Color::DarkGray)),
    ])
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) && i > 0 {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    let len = s.len();
    if i >= len {
        return len;
    }
    while !s.is_char_boundary(i) && i < len {
        i += 1;
    }
    i
}

// ===========================================================================
// Palette rendering — modal overlay centered on the screen.
// ===========================================================================
fn render_palette(f: &mut ratatui::Frame, full_area: Rect, app: &mut App) {
    let w = (full_area.width as u32 * 7 / 10).max(50) as u16;
    let w = w.min(full_area.width);
    let h = (full_area.height as u32 * 7 / 10).max(15) as u16;
    let h = h.min(full_area.height);
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let accent = app.accent_color();
    let block = Block::default()
        .title(" Command palette  ·  Esc close ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(Color::Rgb(20, 20, 24)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // input
            Constraint::Length(1), // count
            Constraint::Min(1),    // list
        ])
        .split(inner);

    // Input
    let input = Line::from(vec![
        Span::styled(" ▶ ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(app.palette_query.clone(), Style::default().fg(Color::White)),
        Span::styled("█", Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(
        Paragraph::new(input).style(Style::default().bg(Color::Rgb(28, 28, 32))),
        chunks[0],
    );

    // Count
    let count = if app.palette_query.is_empty() {
        format!(" {} entries ", app.palette_items.len())
    } else {
        format!(
            " {} of {} ",
            app.palette_filtered.len(),
            app.palette_items.len()
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            count,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[1],
    );

    // List
    let list_area = chunks[2];
    if app.palette_filtered.is_empty() {
        f.render_widget(
            Paragraph::new(" (no matches) ").style(Style::default().fg(Color::DarkGray)),
            list_area,
        );
        return;
    }

    let visible = list_area.height as usize;
    let mut scroll = 0usize;
    if app.palette_idx >= visible {
        scroll = app.palette_idx + 1 - visible;
    }

    let max_w = list_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(visible);
    for i in 0..visible {
        let f_idx = scroll + i;
        let Some(&item_idx) = app.palette_filtered.get(f_idx) else {
            break;
        };
        let it = &app.palette_items[item_idx];
        let selected = f_idx == app.palette_idx;
        let style = if selected {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let label = truncate(&it.label, max_w.saturating_sub(2));
        lines.push(Line::from(Span::styled(format!(" {} ", label), style)));
    }
    f.render_widget(Paragraph::new(lines), list_area);
}

// ===========================================================================
// Bottom terminal pane rendering
// ===========================================================================
fn render_bottom_pane(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let title = match app.bottom.as_ref() {
        Some(bt) => {
            let focus = if app.bottom_focused { "● focused" } else { "○" };
            format!(" {} {}  ·  Esc unfocus  ·  Ctrl+` close ", bt.shell_label, focus)
        }
        None => " shell ".to_string(),
    };
    let border_color = if app.bottom_focused {
        Color::Green
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::TOP)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(bt) = app.bottom.as_ref() {
        let p = bt.parser.lock().unwrap_or_else(|p| p.into_inner());
        let term = PseudoTerminal::new(p.screen());
        f.render_widget(term, inner);
    }
}

// ===========================================================================
// Help overlay
// ===========================================================================
fn render_help(f: &mut ratatui::Frame, full_area: Rect) {
    let w = (full_area.width as u32 * 7 / 10).max(60) as u16;
    let w = w.min(full_area.width);
    let h = (full_area.height as u32 * 8 / 10).max(20) as u16;
    let h = h.min(full_area.height);
    let x = full_area.x + (full_area.width.saturating_sub(w)) / 2;
    let y = full_area.y + (full_area.height.saturating_sub(h)) / 2;
    let area = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, area);

    let block = Block::default()
        .title(" Help  ·  Esc / F1 close ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(20, 20, 24)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let pairs: &[(&str, &str)] = &[
        ("F1",            "Help (this overlay)"),
        ("F2",            "New tab"),
        ("F3",            "Sessions in active tab's cwd hierarchy"),
        ("Shift+F3",      "Global sessions modal (grouped by dir)"),
        ("F4",            "Claude commands sidebar"),
        ("F5",            "Toggle deep-grep (in Sessions)"),
        ("F6",            "File explorer (modal, whole filesystem)"),
        ("F7",            "Toggle mouse mode (off = native select)"),
        ("F8",            "Close active tab (last tab kept)"),
        ("F9",            "Command palette (actions + sessions)"),
        ("F10",           "Quit"),
        ("F11 / F12",     "Previous / next chat (within active project)"),
        ("Ctrl+F11/F12",  "Previous / next project"),
        ("Ctrl+Shift+1..9","Switch to project N"),
        ("Click + project","Open file browser to pick a dir for a new chat"),
        ("Ctrl+B",        "Files sidebar (chroot to tab cwd)"),
        ("Ctrl+`",        "Bottom shell pane (parent shell)"),
        ("Ctrl+F",        "Search active tab's scrollback"),
        ("Ctrl+L",        "Send /clear to active chat"),
        ("Ctrl+Shift+T",  "Reopen most recently closed chat"),
        ("Shift+F2",      "Rename active tab"),
        ("Drag tab",      "Reorder tabs (mouse down on one, up on another)"),
        ("Double-click",  "Close that chat (pinned → ask confirmation)"),
        ("Right-click",   "Rename that chat (opens the rename modal)"),
        ("Ctrl+Click URL","Open the URL under the cursor in your browser"),
        ("2× project",    "Close entire project (asks confirmation)"),
        ("Pin via F9",    "Pinned tabs refuse to close (📌 prefix)"),
        ("Ctrl+Q",        "Quit"),
        ("Ctrl+PgUp/PgDn","Prev / next tab"),
        ("Alt+T/W",       "New / close tab"),
        ("Alt+1..9",      "Switch to chat N within active project"),
        ("Alt+←/→",       "Prev / next tab"),
        ("PgUp/PgDn",     "Scroll PTY history (when claude focused)"),
        ("Ctrl+R (in F3)","Refresh session list from disk"),
        ("Esc (sidebar)", "Unfocus sidebar (keep visible)"),
        ("Esc (bottom)",  "Unfocus bottom shell (keep visible)"),
        ("Sidebar Enter", "Files: cd / Open here · Sessions: resume · Commands: submit"),
        ("Sidebar Space", "Files: insert relative path · Commands: insert (no submit)"),
        ("F6 Space",      "Insert absolute path"),
        ("Drag borders",  "Resize sidebar / bottom pane"),
        ("Shift+drag",    "Native terminal select (for copy)"),
    ];

    let max_left = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(10);
    let lines: Vec<Line> = pairs
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("{:width$}", k, width = max_left),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(*v, Style::default().fg(Color::White)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

// ===========================================================================
// Bottom button bar
// ===========================================================================
fn render_button_bar(
    area: Rect,
    app: &App,
    info: Option<&str>,
) -> (Line<'static>, Vec<ButtonHit>) {
    // (label, action, is_active). F9 is its own slot — uses a sentinel action
    // we map below since we don't want a "TogglePalette" Action variant.
    let buttons: Vec<(&'static str, Action, bool)> = vec![
        (" F1 help ", Action::ToggleHelp, app.help_open),
        (" F2 new ", Action::NewTab, false),
        (" F3 sessions ", Action::ToggleSidebar, app.sidebar_open && app.sidebar_mode == SidebarMode::Sessions),
        (" F4 cmds ", Action::ToggleCommands, app.sidebar_open && app.sidebar_mode == SidebarMode::Commands),
        (" F5 deep ", Action::ToggleDeepGrep, app.deep_grep),
        (" F6 explorer ", Action::ToggleBrowser, app.browser_open),
        (" F7 mouse ", Action::ToggleMouse, !app.mouse_on),
        (" F8 close ", Action::CloseTab, false),
        (" F9 cmd ", Action::TogglePalette, app.palette_open),
        (" F11 < ", Action::PrevTab, false),
        (" F12 > ", Action::NextTab, false),
        (" ^B files ", Action::ToggleFilesSidebar, app.right_sidebar_open),
        (" ^` term ", Action::ToggleBottom, app.bottom_open),
        (" ^F search ", Action::ToggleSearch, app.search.open),
        (" quit ", Action::Quit, false),
    ];

    let mut spans: Vec<Span> = Vec::new();
    let mut hits: Vec<ButtonHit> = Vec::new();
    let mut x = area.x;
    let limit = area.x + area.width;

    for (label, action, active) in buttons {
        let width = label.chars().count() as u16;
        if x.saturating_add(width) > limit {
            break;
        }
        let style = if active {
            Style::default()
                .bg(Color::Green)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::Rgb(80, 80, 80)).fg(Color::White)
        };
        spans.push(Span::styled(label, style));
        // 1-cell gap (DarkGray bg, like the bar)
        if x.saturating_add(width + 1) <= limit {
            spans.push(Span::raw(" "));
        }
        hits.push(ButtonHit {
            rect: Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            action,
        });
        x = x.saturating_add(width + 1);
    }

    // Right-aligned info tag, if it fits.
    if let Some(text) = info {
        let label = format!(" {} ", text);
        let w = label.chars().count() as u16;
        if x.saturating_add(w + 1) <= limit {
            let pad = limit - x - w;
            spans.push(Span::raw(" ".repeat(pad as usize)));
            spans.push(Span::styled(
                label,
                Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    (Line::from(spans), hits)
}

// ===========================================================================
// Sidebar
// ===========================================================================
fn render_sidebar(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    match app.sidebar_mode {
        SidebarMode::Sessions => render_sessions_sidebar(f, area, app),
        SidebarMode::Commands => render_commands_sidebar(f, area, app),
    }
}

fn render_commands_sidebar(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let focused = app.sidebar_focused;
    let focus_tag = if focused { " ● " } else { " ○ " };
    let border_color = if focused { Color::Yellow } else { Color::DarkGray };
    let block = Block::default()
        .title(format!(" Claude commands{} ", focus_tag))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    // search bar
    let search_line = Line::from(vec![
        Span::styled(
            " / ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(app.filter.clone(), Style::default().fg(Color::White)),
        Span::styled("█", Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(
        Paragraph::new(search_line).style(Style::default().bg(Color::Rgb(28, 28, 28))),
        chunks[0],
    );

    let count_text = format!(
        " {} of {}  ·  Enter submit · Space insert ",
        app.commands_filtered.len(),
        app.commands_list.len()
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            count_text,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[1],
    );

    let list_area = chunks[2];
    if app.commands_filtered.is_empty() {
        f.render_widget(
            Paragraph::new(" (no matches) ").style(Style::default().fg(Color::DarkGray)),
            list_area,
        );
        return;
    }

    let row_h = 2usize;
    let visible_rows = (list_area.height as usize) / row_h;
    if app.sidebar_idx < app.sidebar_scroll {
        app.sidebar_scroll = app.sidebar_idx;
    } else if app.sidebar_idx >= app.sidebar_scroll + visible_rows && visible_rows > 0 {
        app.sidebar_scroll = app.sidebar_idx + 1 - visible_rows;
    }

    let max_w = list_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(visible_rows * row_h);
    for i in 0..visible_rows {
        let f_idx = app.sidebar_scroll + i;
        let Some(&c_idx) = app.commands_filtered.get(f_idx) else {
            break;
        };
        let entry = &app.commands_list[c_idx];
        let selected = f_idx == app.sidebar_idx;
        let name_style = if selected {
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        };
        let desc_style = if selected {
            Style::default().bg(Color::Yellow).fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let badge = entry.source.badge();
        let mut name_line: Vec<Span> = Vec::with_capacity(2);
        name_line.push(Span::styled(
            format!(" {} ", truncate(&entry.name, max_w.saturating_sub(2 + badge.len() + 3))),
            name_style,
        ));
        if !badge.is_empty() {
            let badge_color = match entry.source {
                CommandSource::Project => Color::Cyan,
                CommandSource::User => Color::Magenta,
                CommandSource::BuiltIn => Color::DarkGray,
            };
            let badge_style = if selected {
                Style::default().bg(Color::Yellow).fg(badge_color)
            } else {
                Style::default().fg(badge_color)
            };
            name_line.push(Span::styled(format!(" [{}]", badge), badge_style));
        }
        lines.push(Line::from(name_line));
        lines.push(Line::from(Span::styled(
            format!(" {} ", truncate(&entry.desc, max_w.saturating_sub(2))),
            desc_style,
        )));
    }
    f.render_widget(Paragraph::new(lines), list_area);
}

fn handle_commands_sidebar_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
) -> Result<KeyOutcome> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    match k.code {
        KeyCode::Esc => {
            if !app.filter.is_empty() {
                app.filter.clear();
                app.apply_commands_filter();
            } else {
                app.sidebar_focused = false;
                return Ok(KeyOutcome::Continue);
            }
        }
        KeyCode::Up => app.sidebar_idx = app.sidebar_idx.saturating_sub(1),
        KeyCode::Down => {
            let max = app.commands_filtered.len().saturating_sub(1);
            app.sidebar_idx = (app.sidebar_idx + 1).min(max);
        }
        KeyCode::PageUp => app.sidebar_idx = app.sidebar_idx.saturating_sub(10),
        KeyCode::PageDown => {
            let max = app.commands_filtered.len().saturating_sub(1);
            app.sidebar_idx = (app.sidebar_idx + 10).min(max);
        }
        KeyCode::Home => app.sidebar_idx = 0,
        KeyCode::End => app.sidebar_idx = app.commands_filtered.len().saturating_sub(1),
        KeyCode::Enter => {
            if let Some(&c_idx) = app.commands_filtered.get(app.sidebar_idx) {
                let name = app.commands_list[c_idx].name.clone();
                let payload = format!("{}\r", name);
                app.active_tab().write_input(payload.as_bytes())?;
            }
        }
        KeyCode::Char(' ') => {
            if let Some(&c_idx) = app.commands_filtered.get(app.sidebar_idx) {
                let name = app.commands_list[c_idx].name.clone();
                let payload = format!("{} ", name);
                app.active_tab().write_input(payload.as_bytes())?;
            }
        }
        KeyCode::Backspace if app.filter.pop().is_some() => {
            app.apply_commands_filter();
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            app.filter.push(c);
            app.apply_commands_filter();
        }
        _ => {}
    }
    Ok(KeyOutcome::Continue)
}

fn render_files_sidebar(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let focused = app.right_sidebar_focused;
    let Some(br) = app.browser.as_mut() else {
        return;
    };

    let focus_tag = if focused { " ● " } else { " ○ " };
    let title = format!(" Files{}{} ", focus_tag, truncate(&browser::path_label(&br.cwd), 70));
    let border_color = if focused { Color::Magenta } else { Color::DarkGray };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Enter open · Space → claude · → cd · ← parent · Ctrl+B close ",
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[0],
    );

    let list_area = chunks[1];
    if let Some(err) = &br.error {
        f.render_widget(
            Paragraph::new(format!(" ! {} ", err)).style(Style::default().fg(Color::Red)),
            list_area,
        );
        return;
    }

    let visible = list_area.height as usize;
    if br.idx < br.scroll {
        br.scroll = br.idx;
    } else if br.idx >= br.scroll + visible && visible > 0 {
        br.scroll = br.idx + 1 - visible;
    }

    let max_w = list_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(visible);
    for i in 0..visible {
        let e_idx = br.scroll + i;
        let Some(e) = br.entries.get(e_idx) else {
            break;
        };
        let selected = e_idx == br.idx;
        let base = match e {
            BrowserEntry::OpenHere => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            BrowserEntry::Parent => Style::default().fg(Color::Yellow),
            BrowserEntry::Dir(_) => Style::default().fg(Color::Cyan),
            BrowserEntry::File(_) => Style::default().fg(Color::White),
            BrowserEntry::Drive(_) => Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        };
        let style = if selected {
            base.bg(Color::White)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            base
        };
        let label = truncate(&e.label(), max_w.saturating_sub(2));
        lines.push(Line::from(Span::styled(format!(" {} ", label), style)));
    }
    f.render_widget(Paragraph::new(lines), list_area);
}

fn render_sessions_sidebar(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let focus_tag = if app.sidebar_focused { " ● " } else { " ○ " };
    let border_color = if app.sidebar_focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let scope = app.scope_root();
    let scope_label = truncate(&sessions::cwd_label(&scope), 28);
    let block = Block::default()
        .title(format!(" Sessions{}↪ {} ", focus_tag, scope_label))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // search input
            Constraint::Length(1), // count line
            Constraint::Min(1),    // list
        ])
        .split(inner);

    // Search bar
    let mut search_spans = vec![
        Span::styled(" / ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(app.filter.clone(), Style::default().fg(Color::White)),
        Span::styled("█", Style::default().fg(Color::Gray)),
    ];
    if app.deep_grep {
        search_spans.push(Span::raw("  "));
        search_spans.push(Span::styled(
            "[DEEP]",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(search_spans))
            .style(Style::default().bg(Color::Rgb(28, 28, 28))),
        inner_chunks[0],
    );

    // Count / status
    let count_text = if app.filter.is_empty() {
        format!(" {} sessions ", app.sessions.len())
    } else if app.deep_grep {
        let suffix = if app.grep_job.is_some() {
            " · searching…"
        } else {
            " · done"
        };
        format!(" {} match{} ", app.grep_hits.len(), suffix)
    } else {
        format!(
            " {} of {} match ",
            app.filtered.len(),
            app.sessions.len()
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            count_text,
            Style::default().fg(Color::DarkGray),
        ))),
        inner_chunks[1],
    );

    // List
    let list_area = inner_chunks[2];
    if app.filtered.is_empty() {
        let msg = if app.sessions.is_empty() {
            " no sessions in ~/.claude/projects ".to_string()
        } else if app.filter.is_empty() {
            " no sessions under this tab's cwd — Shift+F3 for global ".to_string()
        } else {
            " (no matches) ".to_string()
        };
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            list_area,
        );
        return;
    }

    let row_h: usize = if app.deep_grep && !app.filter.is_empty() {
        3
    } else {
        2
    };
    let visible_rows = (list_area.height as usize) / row_h;
    if app.sidebar_idx < app.sidebar_scroll {
        app.sidebar_scroll = app.sidebar_idx;
    } else if app.sidebar_idx >= app.sidebar_scroll + visible_rows && visible_rows > 0 {
        app.sidebar_scroll = app.sidebar_idx + 1 - visible_rows;
    }

    let max_label_width = list_area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = Vec::with_capacity(visible_rows * row_h);
    for i in 0..visible_rows {
        let f_idx = app.sidebar_scroll + i;
        let Some(&real_idx) = app.filtered.get(f_idx) else {
            break;
        };
        let s = &app.sessions[real_idx];
        let selected = f_idx == app.sidebar_idx;
        let title_style = if selected {
            Style::default()
                .bg(Color::White)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let meta_style = if selected {
            Style::default().bg(Color::White).fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let snippet_style = if selected {
            Style::default().bg(Color::White).fg(Color::Rgb(60, 100, 60))
        } else {
            Style::default().fg(Color::Rgb(120, 180, 120))
        };
        let title_text = format!(" {} ", truncate(&s.title, max_label_width.saturating_sub(2)));
        let branch = s
            .git_branch
            .as_deref()
            .map(|b| format!("·{}", truncate(b, 16)))
            .unwrap_or_default();
        let counts = if s.message_count > 0 || s.total_tokens > 0 {
            format!("  ·{}msg·{}", s.message_count, format_tokens(s.total_tokens))
        } else {
            String::new()
        };
        let meta_text = format!(
            " {}  {}{}{} ",
            sessions::relative_time(s.updated),
            sessions::cwd_label(&s.cwd),
            branch,
            counts,
        );
        let meta_text = truncate(&meta_text, max_label_width);
        lines.push(Line::from(Span::styled(title_text, title_style)));
        lines.push(Line::from(Span::styled(meta_text, meta_style)));
        if row_h == 3 {
            let snip = app
                .grep_hits
                .get(f_idx)
                .map(|h| truncate(&h.snippet, max_label_width.saturating_sub(2)))
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(format!(" {} ", snip), snippet_style)));
        }
    }
    f.render_widget(Paragraph::new(lines), list_area);
}

// ===========================================================================
// Action executor — shared by keyboard and mouse paths.
// ===========================================================================
fn execute_action(action: Action, app: &mut App, pty_area: Rect) -> Result<KeyOutcome> {
    match action {
        Action::NewTab => {
            app.open_tab(pty_area.height.max(1), pty_area.width.max(1))?;
            app.on_active_changed();
            app.save_layout();
            Ok(KeyOutcome::Continue)
        }
        Action::CloseTab => {
            // Pinned active tabs route through the confirm modal; close_active
            // itself silently refuses pinned, so we'd otherwise just no-op.
            let idx = app.active;
            let is_pinned = app.tabs.get(idx).map(|t| t.pinned).unwrap_or(false);
            if is_pinned && app.tabs.len() > 1 {
                let title = app.tabs[idx].title.clone();
                app.ask_confirm(
                    format!("Close pinned chat \"{}\"?", title),
                    PendingConfirm::UnpinAndCloseChat(idx),
                );
                return Ok(KeyOutcome::LayoutChanged);
            }
            app.close_active();
            app.on_active_changed();
            app.save_layout();
            Ok(KeyOutcome::Continue)
        }
        Action::PrevTab => {
            app.prev_tab();
            app.on_active_changed();
            Ok(KeyOutcome::Continue)
        }
        Action::NextTab => {
            app.next_tab();
            app.on_active_changed();
            Ok(KeyOutcome::Continue)
        }
        Action::SwitchTab(i) => {
            app.switch(i);
            app.on_active_changed();
            Ok(KeyOutcome::Continue)
        }
        Action::ToggleSidebar => {
            app.toggle_sidebar();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleFilesSidebar => {
            app.toggle_files_sidebar();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleDeepGrep => {
            if !app.sidebar_open {
                app.toggle_sidebar();
            }
            app.toggle_deep_grep();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleMouse => {
            app.mouse_on = !app.mouse_on;
            app.mouse_capture_dirty = true;
            Ok(KeyOutcome::Continue)
        }
        Action::TogglePalette => {
            app.toggle_palette();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleBrowser => {
            app.toggle_browser();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleBottom => {
            app.toggle_bottom()?;
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::RestoreLayout => {
            app.restore_layout(pty_area.height.max(1), pty_area.width.max(1))?;
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ReloadConfig => {
            app.reload_config();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleCommands => {
            app.toggle_commands_sidebar();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleHelp => {
            app.toggle_help();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleSearch => {
            app.toggle_search();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::RenameActiveTab => {
            app.open_rename();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::OpenSaveLayoutAs => {
            app.open_save_as();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleGlobalSessions => {
            app.toggle_global_sessions();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleActivePin => {
            app.toggle_active_pin();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::PrevProject => {
            app.prev_project();
            app.on_active_changed();
            Ok(KeyOutcome::Continue)
        }
        Action::NextProject => {
            app.next_project();
            app.on_active_changed();
            Ok(KeyOutcome::Continue)
        }
        Action::SwitchProject(n) => {
            app.switch_project(n);
            app.on_active_changed();
            Ok(KeyOutcome::Continue)
        }
        Action::OpenBrowserForNewProject => {
            app.open_browser_for_new_project();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ToggleActiveProjectPin => {
            app.toggle_active_project_pin();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::NewTabContinue => {
            app.open_tab_continue(pty_area.height.max(1), pty_area.width.max(1))?;
            app.on_active_changed();
            app.save_layout();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::NewTabWithModel(idx) => {
            app.open_tab_with_model_idx(
                idx,
                pty_area.height.max(1),
                pty_area.width.max(1),
            )?;
            app.on_active_changed();
            app.save_layout();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::OpenBroadcast => {
            app.broadcast.input.clear();
            app.broadcast.open = true;
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ShowActiveUsage => {
            app.show_active_usage();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ShowGitDiff => {
            app.show_git_diff();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ClearActiveChat => {
            app.active_tab().write_input(b"/clear\r")?;
            Ok(KeyOutcome::Continue)
        }
        Action::CopyChatScrollback => {
            app.copy_scrollback();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::CopyLastResponse => {
            app.copy_last_response();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::ReopenLastClosed => {
            let reopened = app.reopen_last_closed(
                pty_area.height.max(1),
                pty_area.width.max(1),
            )?;
            if reopened {
                app.on_active_changed();
                app.save_layout();
            }
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::InsertSnippet(idx) => {
            // Resolve via the cached name snapshot, then look up the body
            // in the (possibly-reloaded) config. Strip `\r` so an
            // accidentally-Windows-line-ended snippet doesn't auto-submit.
            let Some(name) = app.snippet_keys.get(idx).cloned() else {
                return Ok(KeyOutcome::Continue);
            };
            let Some(text) = app.config.snippets.get(&name).cloned() else {
                return Ok(KeyOutcome::Continue);
            };
            let safe: String = text.chars().filter(|c| *c != '\r').collect();
            app.active_tab().write_input(safe.as_bytes())?;
            Ok(KeyOutcome::Continue)
        }
        Action::ExportSessionNote => {
            app.export_session_note();
            Ok(KeyOutcome::LayoutChanged)
        }
        Action::Quit => Ok(KeyOutcome::Quit),
    }
}

// ===========================================================================
// Keys
// ===========================================================================
fn handle_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    pty_area: Rect,
) -> Result<KeyOutcome> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);

    if k.code == KeyCode::F(10) || (ctrl && k.code == KeyCode::Char('q')) {
        return Ok(KeyOutcome::Quit);
    }

    // F1 — help overlay (global)
    if k.code == KeyCode::F(1) {
        return execute_action(Action::ToggleHelp, app, pty_area);
    }

    // While help overlay is shown, swallow keys (Esc/F1 closes via above)
    if app.help_open {
        if k.code == KeyCode::Esc {
            app.help_open = false;
            return Ok(KeyOutcome::LayoutChanged);
        }
        return Ok(KeyOutcome::Continue);
    }

    // User-defined keybindings from `[keys]` — checked before the hardcoded
    // F-key dispatch so the user can re-route any nullary action to any key.
    // Modal overlays still need their own gates (already handled below).
    if !app.palette_open
        && !app.browser_open
        && !app.search.open
        && !app.rename_open
        && !app.save_as_open
        && !app.confirm.open
        && !app.broadcast.open
        && !app.global_sessions.open
    {
        if let Some(act) = app.key_bindings.lookup(k.code, k.modifiers) {
            return execute_action(act, app, pty_area);
        }
    }

    // Shift+F2 — rename the active tab.
    if shift && k.code == KeyCode::F(2) {
        return execute_action(Action::RenameActiveTab, app, pty_area);
    }

    // Rename modal eats all other keys while open.
    if app.rename_open {
        return handle_rename_key(k, app);
    }

    // Save-layout-as modal eats keys while open.
    if app.save_as_open {
        return handle_save_as_key(k, app);
    }

    // Confirm modal — top priority once visible.
    if app.confirm.open {
        return handle_confirm_key(k, app);
    }

    // Broadcast modal — eats keys until Enter/Esc.
    if app.broadcast.open {
        return handle_broadcast_key(k, app);
    }

    // Usage modal — any key dismisses (read-only popup).
    if app.usage_open {
        app.usage_open = false;
        app.usage_lines.clear();
        return Ok(KeyOutcome::LayoutChanged);
    }

    // Git diff modal — Esc closes, navigation scrolls.
    if app.diff_open {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                app.diff_open = false;
                app.diff_lines.clear();
                app.diff_scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.diff_scroll = app.diff_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = app.diff_lines.len().saturating_sub(1);
                app.diff_scroll = (app.diff_scroll + 1).min(max);
            }
            KeyCode::PageUp => {
                app.diff_scroll = app.diff_scroll.saturating_sub(15);
            }
            KeyCode::PageDown => {
                let max = app.diff_lines.len().saturating_sub(1);
                app.diff_scroll = (app.diff_scroll + 15).min(max);
            }
            KeyCode::Home | KeyCode::Char('g') => app.diff_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => {
                app.diff_scroll = app.diff_lines.len().saturating_sub(1);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                // Refresh — re-run git on the still-active tab.
                app.show_git_diff();
            }
            _ => {}
        }
        return Ok(KeyOutcome::LayoutChanged);
    }

    // Shift+F3 toggles the global-sessions modal.
    if shift && k.code == KeyCode::F(3) {
        return execute_action(Action::ToggleGlobalSessions, app, pty_area);
    }

    // Global-sessions modal eats keys while open.
    if app.global_sessions.open {
        return handle_global_sessions_key(k, app, pty_area);
    }

    // Ctrl+` — toggle bottom terminal (global)
    if ctrl && matches!(k.code, KeyCode::Char('`') | KeyCode::Char('~')) {
        return execute_action(Action::ToggleBottom, app, pty_area);
    }

    // Ctrl+F — toggle scrollback search on the active tab (global).
    if ctrl && matches!(k.code, KeyCode::Char('f') | KeyCode::Char('F')) {
        return execute_action(Action::ToggleSearch, app, pty_area);
    }

    // While search overlay is open, swallow keys.
    if app.search.open {
        return handle_search_key(k, app);
    }

    // If the bottom pane is focused, route keystrokes there (Esc unfocuses).
    if app.bottom_open && app.bottom_focused {
        if k.code == KeyCode::Esc {
            app.bottom_focused = false;
            return Ok(KeyOutcome::Continue);
        }
        if let Some(bytes) = key_to_bytes(&k) {
            if let Some(bt) = app.bottom.as_mut() {
                bt.write_input(&bytes)?;
            }
        }
        return Ok(KeyOutcome::Continue);
    }

    // Command palette has top priority — F9 toggles it.
    if k.code == KeyCode::F(9) {
        app.toggle_palette();
        return Ok(KeyOutcome::LayoutChanged);
    }

    if app.palette_open {
        return handle_palette_key(k, app, pty_area);
    }

    if app.browser_open {
        return handle_browser_key(k, app, pty_area);
    }

    if k.code == KeyCode::F(3) {
        return execute_action(Action::ToggleSidebar, app, pty_area);
    }

    if k.code == KeyCode::F(4) {
        return execute_action(Action::ToggleCommands, app, pty_area);
    }

    if k.code == KeyCode::F(6) {
        return execute_action(Action::ToggleBrowser, app, pty_area);
    }

    // Ctrl+B — toggle Files sidebar (VSCode-style explorer)
    if ctrl && matches!(k.code, KeyCode::Char('b') | KeyCode::Char('B')) {
        return execute_action(Action::ToggleFilesSidebar, app, pty_area);
    }

    if k.code == KeyCode::F(5) {
        return execute_action(Action::ToggleDeepGrep, app, pty_area);
    }

    if k.code == KeyCode::F(7) {
        return execute_action(Action::ToggleMouse, app, pty_area);
    }

    // Right files sidebar — only when focused
    if app.right_sidebar_open && app.right_sidebar_focused {
        return handle_files_sidebar_key(k, app, pty_area);
    }

    // Commands-sidebar handling — only when focused
    if app.sidebar_open && app.sidebar_focused && app.sidebar_mode == SidebarMode::Commands {
        return handle_commands_sidebar_key(k, app);
    }

    // Sessions-sidebar handling — only when focused
    if app.sidebar_open && app.sidebar_focused {
        match k.code {
            KeyCode::Esc => {
                if !app.filter.is_empty() {
                    app.filter.clear();
                    app.apply_filter();
                } else {
                    app.sidebar_focused = false;
                    return Ok(KeyOutcome::Continue);
                }
            }
            KeyCode::Up => {
                app.sidebar_idx = app.sidebar_idx.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = app.filtered.len().saturating_sub(1);
                app.sidebar_idx = (app.sidebar_idx + 1).min(max);
            }
            KeyCode::PageUp => {
                app.sidebar_idx = app.sidebar_idx.saturating_sub(10);
            }
            KeyCode::PageDown => {
                let max = app.filtered.len().saturating_sub(1);
                app.sidebar_idx = (app.sidebar_idx + 10).min(max);
            }
            KeyCode::Home => {
                app.sidebar_idx = 0;
            }
            KeyCode::End => {
                app.sidebar_idx = app.filtered.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                app.open_selected_session(pty_area.height.max(1), pty_area.width.max(1))?;
                return Ok(KeyOutcome::LayoutChanged);
            }
            // Ctrl+R — refresh the session list from disk without closing.
            KeyCode::Char('r') | KeyCode::Char('R') if ctrl => {
                app.refresh_sessions();
                app.sidebar_idx = 0;
                app.sidebar_scroll = 0;
                app.apply_filter();
            }
            KeyCode::Backspace if app.filter.pop().is_some() => {
                app.apply_filter();
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                app.filter.push(c);
                app.apply_filter();
            }
            _ => {}
        }
        return Ok(KeyOutcome::Continue);
    }

    // PTY-focused

    // Ctrl+L — send `/clear` to the active chat. claude handles the command
    // itself; we're a single-stroke shortcut for typing it. Placed AFTER all
    // modal / sidebar / bottom-shell gates so we don't steal Ctrl+L from
    // shells (where it traditionally clears the terminal).
    if ctrl && matches!(k.code, KeyCode::Char('l') | KeyCode::Char('L')) {
        return execute_action(Action::ClearActiveChat, app, pty_area);
    }

    // Host scrollback
    match k.code {
        KeyCode::PageUp if !ctrl && !shift => {
            app.active_tab().scroll_up();
            return Ok(KeyOutcome::Continue);
        }
        KeyCode::PageDown if !ctrl && !shift => {
            app.active_tab().scroll_down();
            return Ok(KeyOutcome::Continue);
        }
        _ => {}
    }

    // Tab management
    match k.code {
        KeyCode::F(2) => return execute_action(Action::NewTab, app, pty_area),
        KeyCode::F(8) => return execute_action(Action::CloseTab, app, pty_area),
        // Ctrl+F11 / Ctrl+F12 jump between projects (project bar);
        // bare F11 / F12 cycle chats within the current project.
        KeyCode::F(11) if ctrl => return execute_action(Action::PrevProject, app, pty_area),
        KeyCode::F(12) if ctrl => return execute_action(Action::NextProject, app, pty_area),
        KeyCode::F(11) => return execute_action(Action::PrevTab, app, pty_area),
        KeyCode::F(12) => return execute_action(Action::NextTab, app, pty_area),
        KeyCode::PageUp if ctrl => return execute_action(Action::PrevTab, app, pty_area),
        KeyCode::PageDown if ctrl => return execute_action(Action::NextTab, app, pty_area),
        _ => {}
    }

    // Ctrl+Shift+1..9 — jump straight to project N (1-based).
    if ctrl && shift {
        if let KeyCode::Char(c) = k.code {
            if c.is_ascii_digit() && c != '0' {
                let idx = (c as u8 - b'1') as usize;
                return execute_action(Action::SwitchProject(idx), app, pty_area);
            }
            // Ctrl+Shift+T — reopen the most recently closed chat.
            if matches!(c, 't' | 'T') {
                return execute_action(Action::ReopenLastClosed, app, pty_area);
            }
        }
    }

    if alt {
        match k.code {
            KeyCode::Char('t') | KeyCode::Char('T') => {
                return execute_action(Action::NewTab, app, pty_area);
            }
            KeyCode::Char('w') | KeyCode::Char('W') => {
                return execute_action(Action::CloseTab, app, pty_area);
            }
            KeyCode::Right => return execute_action(Action::NextTab, app, pty_area),
            KeyCode::Left => return execute_action(Action::PrevTab, app, pty_area),
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as u8 - b'1') as usize;
                return execute_action(Action::SwitchTab(idx), app, pty_area);
            }
            _ => {}
        }
    }

    // Anything else reaches the PTY
    app.active_tab().scroll_reset();
    if let Some(bytes) = key_to_bytes(&k) {
        app.active_tab().write_input(&bytes)?;
    }
    Ok(KeyOutcome::Continue)
}

// ===========================================================================
// Mouse — returns an Action if the click resolved to one; otherwise None.
// ===========================================================================
/// Map a mouse row over the scrollbar to a vt100 scrollback offset and apply
/// it to the active tab. Top of the bar ≈ oldest scrollback (max offset),
/// bottom ≈ live tail (offset 0). Uses the active tab's actual text-buffer
/// length so the gesture matches whatever's really scrollable.
fn set_scrollback_from_mouse_row(app: &mut App, row: u16, pty_area: Rect) {
    if pty_area.height == 0 {
        return;
    }
    let raw = row.saturating_sub(pty_area.y) as usize;
    let height = pty_area.height as usize;
    let row_in_bar = raw.min(height.saturating_sub(1));
    let total = active_total_lines(app);
    let track_max = total.saturating_sub(height);
    let offset = if height <= 1 || track_max == 0 {
        0
    } else {
        track_max * (height - 1 - row_in_bar) / (height - 1)
    };
    let tab = app.active_tab();
    let capped = offset.min(tab.scrollback_max);
    if let Ok(mut p) = tab.parser.lock() {
        p.set_scrollback(capped);
    }
    tab.dirty.store(true, Ordering::Release);
}

/// Active tab's plain-text mirror line count. Read atomically — the reader
/// thread keeps `total_lines` in sync after every feed — so the scrollbar
/// render path can avoid locking `text_buffer`.
fn active_total_lines(app: &App) -> usize {
    app.tabs
        .get(app.active)
        .map(|t| t.total_lines.load(Ordering::Relaxed))
        .unwrap_or(0)
}

fn handle_mouse(me: MouseEvent, app: &mut App, pty_area: Rect) -> Result<Option<Action>> {
    // ---- Tab drag-to-reorder: complete drag on left-button up over the
    // chat bar (row 1). Handled before the generic Up reset so we can read
    // `tab_drag_from`.
    if matches!(me.kind, MouseEventKind::Up(MouseButton::Left))
        && me.row == 1
        && app.tab_drag_from.is_some()
    {
        let src = app.tab_drag_from.take().unwrap();
        for &(r, dst_global) in &app.chat_rects {
            if me.column >= r.x && me.column < r.x + r.width {
                if dst_global != src && dst_global < app.tabs.len() && src < app.tabs.len() {
                    app.tabs.swap(src, dst_global);
                    if app.active == src {
                        app.active = dst_global;
                    } else if app.active == dst_global {
                        app.active = src;
                    }
                    app.save_layout();
                    return Ok(None);
                }
                break;
            }
        }
        return Ok(None);
    }

    // ---- Resize drag handling ----
    if matches!(me.kind, MouseEventKind::Up(MouseButton::Left)) {
        app.resize_drag = ResizeDrag::None;
        // Mouse-up anywhere else cancels an in-flight tab drag.
        app.tab_drag_from = None;
    }
    if app.resize_drag != ResizeDrag::None
        && matches!(me.kind, MouseEventKind::Drag(MouseButton::Left))
    {
        match app.resize_drag {
            ResizeDrag::Sidebar => {
                let base = app.sidebar_area.x;
                let new_w = me.column.saturating_sub(base).saturating_add(1);
                app.config.layout.sidebar_width = new_w.clamp(20, 120);
            }
            ResizeDrag::RightSidebar => {
                // dragging the left border of right sidebar — width grows as cursor moves left
                let right_edge = app.right_sidebar_area.x + app.right_sidebar_area.width;
                let new_w = right_edge.saturating_sub(me.column);
                app.config.layout.right_sidebar_width = new_w.clamp(20, 120);
            }
            ResizeDrag::Bottom => {
                let new_h = app.body_bottom_y.saturating_sub(me.row);
                app.config.layout.bottom_height = new_h.clamp(4, 40);
            }
            ResizeDrag::Scrollbar => {
                set_scrollback_from_mouse_row(app, me.row, pty_area);
            }
            ResizeDrag::None => {}
        }
        return Ok(None);
    }
    // Start drag on border click
    if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
        // Left sidebar right border = last column of sidebar area
        if app.sidebar_open && app.sidebar_area.width > 0 {
            let edge = app.sidebar_area.x + app.sidebar_area.width - 1;
            if me.column == edge
                && me.row >= app.sidebar_area.y
                && me.row < app.sidebar_area.y + app.sidebar_area.height
            {
                app.resize_drag = ResizeDrag::Sidebar;
                return Ok(None);
            }
        }
        // Right sidebar left border = first column of right_sidebar_area
        if app.right_sidebar_open && app.right_sidebar_area.width > 0 {
            let edge = app.right_sidebar_area.x;
            if me.column == edge
                && me.row >= app.right_sidebar_area.y
                && me.row < app.right_sidebar_area.y + app.right_sidebar_area.height
            {
                app.resize_drag = ResizeDrag::RightSidebar;
                return Ok(None);
            }
        }
        // Bottom top border = first row of bottom area (the Borders::TOP line)
        if app.bottom_open
            && app.bottom_area.height > 0
            && me.row == app.bottom_area.y
            && me.column >= app.bottom_area.x
            && me.column < app.bottom_area.x + app.bottom_area.width
        {
            app.resize_drag = ResizeDrag::Bottom;
            return Ok(None);
        }
        // Scrollbar column on the right edge of the chat area — click jumps
        // to that position, drag continues to update scrollback offset.
        if pty_area.width > 0
            && me.column == pty_area.x + pty_area.width
            && me.row >= pty_area.y
            && me.row < pty_area.y + pty_area.height
        {
            app.resize_drag = ResizeDrag::Scrollbar;
            set_scrollback_from_mouse_row(app, me.row, pty_area);
            return Ok(None);
        }
    }

    // Bottom pane has highest priority — route clicks there, including focus switch.
    if app.bottom_open
        && me.row >= app.bottom_area.y
        && me.row < app.bottom_area.y + app.bottom_area.height
        && me.column >= app.bottom_area.x
        && me.column < app.bottom_area.x + app.bottom_area.width
    {
        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.bottom_focused = true;
        }
        // Forward mouse events only if the embedded shell actually asked for
        // mouse reporting. Otherwise the SGR escape sequence would land in
        // its stdin as garbage.
        if let Some(bt) = app.bottom.as_mut() {
            let mouse_requested = {
                let p = bt.parser.lock().unwrap_or_else(|p| p.into_inner());
                !matches!(p.screen().mouse_protocol_mode(), MouseProtocolMode::None)
            };
            if mouse_requested {
                let x = me.column.saturating_sub(app.bottom_area.x) + 1;
                let y = me.row.saturating_sub(app.bottom_area.y) + 1;
                if let Some(bytes) = mouse_to_sgr(me, x, y) {
                    let _ = bt.write_input(&bytes);
                }
            }
        }
        return Ok(None);
    }

    // Click outside bottom while it's focused → return focus to main pty.
    if app.bottom_focused
        && matches!(me.kind, MouseEventKind::Down(MouseButton::Left))
    {
        app.bottom_focused = false;
    }

    // Project bar (row 0):
    //   left-click       — switch project (jump to its first chat)
    //   left-click × 2   — close the project (asks for confirmation)
    //   trailing `+`     — open file browser to pick a dir for a new chat
    if me.row == 0 {
        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
            for &(r, proj_idx) in &app.project_rects {
                if me.column >= r.x && me.column < r.x + r.width {
                    let projs = app.projects();
                    let Some(cwd) = projs.get(proj_idx).cloned() else {
                        return Ok(None);
                    };
                    let now = Instant::now();
                    let is_double = app
                        .last_project_click
                        .as_ref()
                        .map(|(when, target)| {
                            *target == cwd
                                && now.duration_since(*when).as_millis() < DOUBLE_CLICK_MS
                        })
                        .unwrap_or(false);
                    if is_double {
                        app.last_project_click = None;
                        // Multi-chat close always asks — too much to throw
                        // away on a misclick. Pinned vs unpinned shows in
                        // the message but doesn't change the gating.
                        let pin_note = if app.is_project_pinned(&cwd) {
                            " (pinned)"
                        } else {
                            ""
                        };
                        let chat_count = app
                            .tabs
                            .iter()
                            .filter(|t| t.cwd == cwd)
                            .count();
                        let name = cwd
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or_else(|| cwd.to_str().unwrap_or(""))
                            .to_string();
                        app.ask_confirm(
                            format!(
                                "Close project \"{}\"{} — {} chat(s) will be killed.",
                                name, pin_note, chat_count
                            ),
                            PendingConfirm::CloseProject(cwd),
                        );
                        return Ok(None);
                    }
                    app.last_project_click = Some((now, cwd));
                    return Ok(Some(Action::SwitchProject(proj_idx)));
                }
            }
            if let Some(nr) = app.new_project_rect {
                if me.column >= nr.x && me.column < nr.x + nr.width {
                    return Ok(Some(Action::OpenBrowserForNewProject));
                }
            }
        }
        return Ok(None);
    }

    // Chat bar (row 1):
    //   left-click           — switch chat (and arm drag-to-reorder)
    //   left-click × 2 (fast) — close that chat (pin refuses)
    //   right-click          — open rename modal for that chat
    //   release on a different chat — swap (handled by the Up handler above)
    if me.row == 1 {
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                for &(r, global_idx) in &app.chat_rects {
                    if me.column >= r.x && me.column < r.x + r.width {
                        let now = Instant::now();
                        let is_double = app
                            .last_chat_click
                            .map(|(when, target)| {
                                target == global_idx
                                    && now.duration_since(when).as_millis() < DOUBLE_CLICK_MS
                            })
                            .unwrap_or(false);
                        if is_double {
                            // Consume the click — don't let a third click cascade.
                            app.last_chat_click = None;
                            app.active = global_idx;
                            let is_pinned = app
                                .tabs
                                .get(global_idx)
                                .map(|t| t.pinned)
                                .unwrap_or(false);
                            if is_pinned {
                                let title = app.tabs[global_idx].title.clone();
                                app.ask_confirm(
                                    format!("Close pinned chat \"{}\"?", title),
                                    PendingConfirm::UnpinAndCloseChat(global_idx),
                                );
                            } else {
                                app.close_active();
                                app.on_active_changed();
                                app.save_layout();
                            }
                            return Ok(None);
                        }
                        app.last_chat_click = Some((now, global_idx));
                        app.tab_drag_from = Some(global_idx);
                        app.active = global_idx;
                        app.on_active_changed();
                        return Ok(None);
                    }
                }
                if let Some(nr) = app.new_tab_rect {
                    if me.column >= nr.x && me.column < nr.x + nr.width {
                        return Ok(Some(Action::NewTab));
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                for &(r, global_idx) in &app.chat_rects {
                    if me.column >= r.x && me.column < r.x + r.width {
                        app.active = global_idx;
                        app.on_active_changed();
                        app.open_rename();
                        return Ok(None);
                    }
                }
            }
            _ => {}
        }
        return Ok(None);
    }

    // Button bar (last row): button_hits are absolute rects
    if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
        for b in &app.button_hits {
            if me.row == b.rect.y
                && me.column >= b.rect.x
                && me.column < b.rect.x + b.rect.width
            {
                return Ok(Some(b.action));
            }
        }
    }

    // Click inside left sidebar area → focus it.
    // Wheel over the sidebar also steps through its list.
    if app.sidebar_open && app.sidebar_area.width > 0 {
        let in_sidebar = me.row >= app.sidebar_area.y
            && me.row < app.sidebar_area.y + app.sidebar_area.height
            && me.column >= app.sidebar_area.x
            && me.column < app.sidebar_area.x + app.sidebar_area.width;
        if in_sidebar {
            match me.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    app.sidebar_focused = true;
                    app.right_sidebar_focused = false;
                }
                MouseEventKind::ScrollUp => {
                    app.sidebar_idx = app.sidebar_idx.saturating_sub(3);
                }
                MouseEventKind::ScrollDown => {
                    let max = match app.sidebar_mode {
                        SidebarMode::Sessions => app.filtered.len(),
                        SidebarMode::Commands => app.commands_filtered.len(),
                    }
                    .saturating_sub(1);
                    app.sidebar_idx = (app.sidebar_idx + 3).min(max);
                }
                _ => {}
            }
            return Ok(None);
        }
    }

    // Click inside right sidebar area → focus it.
    // Wheel over the right sidebar moves through the file browser.
    if app.right_sidebar_open && app.right_sidebar_area.width > 0 {
        let in_right = me.row >= app.right_sidebar_area.y
            && me.row < app.right_sidebar_area.y + app.right_sidebar_area.height
            && me.column >= app.right_sidebar_area.x
            && me.column < app.right_sidebar_area.x + app.right_sidebar_area.width;
        if in_right {
            match me.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    app.right_sidebar_focused = true;
                    app.sidebar_focused = false;
                }
                MouseEventKind::ScrollUp => {
                    if let Some(br) = app.browser.as_mut() {
                        for _ in 0..3 {
                            br.move_up();
                        }
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(br) = app.browser.as_mut() {
                        for _ in 0..3 {
                            br.move_down();
                        }
                    }
                }
                _ => {}
            }
            return Ok(None);
        }
    }

    // Outside PTY area? Ignore — but the scrollbar column lives at
    // `pty_area.x + pty_area.width`, so accept wheel events one column to the
    // right of the PTY too (so hovering the scrollbar still scrolls).
    if me.row < pty_area.y || me.row >= pty_area.y + pty_area.height {
        return Ok(None);
    }
    let in_chat_band = me.column >= pty_area.x
        && me.column <= pty_area.x + pty_area.width; // inclusive — covers the scrollbar column
    if !in_chat_band {
        return Ok(None);
    }
    // For clicks/drags, the scrollbar column is dead — but a wheel event
    // anywhere in the band scrolls. Translate the column once for further
    // checks below.
    let on_scrollbar = me.column == pty_area.x + pty_area.width;
    if on_scrollbar {
        match me.kind {
            MouseEventKind::ScrollUp => {
                app.active_tab().scroll_up();
            }
            MouseEventKind::ScrollDown => {
                app.active_tab().scroll_down();
            }
            _ => {}
        }
        return Ok(None);
    }

    // Click into main pty area → unfocus all panels.
    if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
        app.sidebar_focused = false;
        app.right_sidebar_focused = false;
    }

    // Ctrl + left-click → look for a URL under the cursor and open it in the
    // OS default handler. Resolved BEFORE forwarding to claude so the click
    // doesn't double-trigger inside the inner TUI.
    if matches!(me.kind, MouseEventKind::Down(MouseButton::Left))
        && me.modifiers.contains(KeyModifiers::CONTROL)
    {
        let row_in_pty = me.row.saturating_sub(pty_area.y);
        let col_in_pty = me.column.saturating_sub(pty_area.x);
        let url = {
            let p = app.active_tab().parser.lock().unwrap_or_else(|p| p.into_inner());
            url_at(p.screen(), row_in_pty, col_in_pty)
        };
        if let Some(u) = url {
            open_url(&u);
            return Ok(None);
        }
    }

    let mouse_enabled = app.active_tab().mouse_enabled();
    if !mouse_enabled {
        match me.kind {
            MouseEventKind::ScrollUp => app.active_tab().scroll_up(),
            MouseEventKind::ScrollDown => app.active_tab().scroll_down(),
            _ => {}
        }
        return Ok(None);
    }

    let pty_x = me.column.saturating_sub(pty_area.x) + 1;
    let pty_y = me.row.saturating_sub(pty_area.y) + 1;
    let bytes = match mouse_to_sgr(me, pty_x, pty_y) {
        Some(b) => b,
        None => return Ok(None),
    };
    app.active_tab().write_input(&bytes)?;
    Ok(None)
}

fn mouse_to_sgr(me: MouseEvent, x: u16, y: u16) -> Option<Vec<u8>> {
    let modifiers = me.modifiers;
    let shift = modifiers.contains(KeyModifiers::SHIFT) as u8;
    let alt = modifiers.contains(KeyModifiers::ALT) as u8;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL) as u8;
    let mods_bits = (shift * 4) | (alt * 8) | (ctrl * 16);

    let (button, is_release) = match me.kind {
        MouseEventKind::Down(b) => (button_code(b), false),
        MouseEventKind::Up(b) => (button_code(b), true),
        MouseEventKind::Drag(b) => (button_code(b) | 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        MouseEventKind::Moved => return None,
    };
    let cb = button | mods_bits;
    let suffix = if is_release { 'm' } else { 'M' };
    Some(format!("\x1b[<{};{};{}{}", cb, x, y, suffix).into_bytes())
}

fn button_code(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

// ===========================================================================
// Key → bytes
// ===========================================================================
fn key_to_bytes(k: &crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use crossterm::event::KeyCode::*;
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }

    match k.code {
        Char(c) => {
            if ctrl {
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    out.push((upper as u8) - b'A' + 1);
                } else {
                    let mut tmp = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
            } else {
                let mut tmp = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
        }
        Enter => out.push(b'\r'),
        Backspace => out.push(0x7f),
        Tab => out.push(b'\t'),
        BackTab => out.extend_from_slice(b"\x1b[Z"),
        Esc => out.push(0x1b),
        Left => out.extend_from_slice(b"\x1b[D"),
        Right => out.extend_from_slice(b"\x1b[C"),
        Up => out.extend_from_slice(b"\x1b[A"),
        Down => out.extend_from_slice(b"\x1b[B"),
        Home => out.extend_from_slice(b"\x1b[H"),
        End => out.extend_from_slice(b"\x1b[F"),
        PageUp => out.extend_from_slice(b"\x1b[5~"),
        PageDown => out.extend_from_slice(b"\x1b[6~"),
        Insert => out.extend_from_slice(b"\x1b[2~"),
        Delete => out.extend_from_slice(b"\x1b[3~"),
        F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => return None,
            };
            out.extend_from_slice(seq);
        }
        _ => return None,
    }

    Some(out)
}
