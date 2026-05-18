use std::path::{Path, PathBuf};

pub enum Entry {
    OpenHere,
    Parent,
    Dir(String),
    File(String),
    /// Windows-only: a drive root like `C:\` shown at the virtual "drives" root.
    Drive(String),
}

impl Entry {
    pub fn label(&self) -> String {
        match self {
            Entry::OpenHere => "★  Open Claude in this folder".to_string(),
            Entry::Parent => "..  (up)".to_string(),
            Entry::Dir(n) => format!("📁  {}", n),
            Entry::File(n) => format!("📄  {}", n),
            Entry::Drive(letter) => format!("💽  {}:\\", letter),
        }
    }
}

pub struct FileBrowser {
    pub cwd: PathBuf,
    pub show_hidden: bool,
    /// If set, navigation is jailed to this directory (no `..` above it).
    pub root: Option<PathBuf>,
    /// True when sitting above all drives (Windows-only virtual root). `cwd`
    /// is empty in this state and `entries` lists the available drives.
    pub at_drives_root: bool,
    pub entries: Vec<Entry>,
    pub idx: usize,
    pub scroll: usize,
    pub error: Option<String>,
}

impl FileBrowser {
    pub fn new(cwd: PathBuf, show_hidden: bool) -> Self {
        let mut b = Self {
            cwd,
            show_hidden,
            root: None,
            at_drives_root: false,
            entries: Vec::new(),
            idx: 0,
            scroll: 0,
            error: None,
        };
        b.refresh();
        b
    }

    /// Like `new`, but navigation is jailed to `root` — `cd ..` stops there
    /// and the `..` entry is hidden when at root.
    pub fn new_chrooted(cwd: PathBuf, show_hidden: bool, root: PathBuf) -> Self {
        let canon_root = root.canonicalize().map(strip_unc).unwrap_or(root);
        let canon_cwd = cwd.canonicalize().map(strip_unc).unwrap_or(cwd);
        let mut b = Self {
            cwd: canon_cwd,
            show_hidden,
            root: Some(canon_root),
            at_drives_root: false,
            entries: Vec::new(),
            idx: 0,
            scroll: 0,
            error: None,
        };
        b.refresh();
        b
    }

    fn at_root(&self) -> bool {
        matches!(&self.root, Some(r) if &self.cwd == r)
    }

    pub fn set_show_hidden(&mut self, show: bool) {
        if self.show_hidden != show {
            self.show_hidden = show;
            self.refresh();
        }
    }

    pub fn refresh(&mut self) {
        self.entries.clear();
        self.error = None;

        if self.at_drives_root {
            for letter in enum_drives() {
                self.entries.push(Entry::Drive(letter));
            }
            if self.idx >= self.entries.len() {
                self.idx = self.entries.len().saturating_sub(1);
            }
            self.scroll = 0;
            return;
        }

        self.entries.push(Entry::OpenHere);
        // `..` exits to drives-root from a drive root (e.g. `C:\`) on Windows
        // when not chrooted; otherwise climbs one directory.
        let show_parent = if self.root.is_some() {
            !self.at_root()
        } else {
            cfg!(windows) || self.cwd.parent().is_some()
        };
        if show_parent {
            self.entries.push(Entry::Parent);
        }
        match std::fs::read_dir(&self.cwd) {
            Ok(rd) => {
                let mut dirs: Vec<String> = Vec::new();
                let mut files: Vec<String> = Vec::new();
                for ent in rd.flatten() {
                    let name = ent.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') && !self.show_hidden {
                        continue;
                    }
                    let is_dir = ent
                        .file_type()
                        .map(|t| t.is_dir() || t.is_symlink())
                        .unwrap_or(false);
                    if is_dir {
                        dirs.push(name);
                    } else {
                        files.push(name);
                    }
                }
                dirs.sort_by_key(|s| s.to_lowercase());
                files.sort_by_key(|s| s.to_lowercase());
                for d in dirs {
                    self.entries.push(Entry::Dir(d));
                }
                for f in files {
                    self.entries.push(Entry::File(f));
                }
            }
            Err(e) => {
                self.error = Some(format!("read_dir: {}", e));
            }
        }
        if self.idx >= self.entries.len() {
            self.idx = self.entries.len().saturating_sub(1);
        }
        self.scroll = 0;
    }

    pub fn move_to(&mut self, dest: PathBuf) {
        self.at_drives_root = false;
        self.cwd = match dest.canonicalize() {
            Ok(p) => strip_unc(p),
            Err(_) => dest,
        };
        self.idx = 0;
        self.refresh();
    }

    /// Switch into the Windows virtual "list of drives" view.
    pub fn enter_drives_root(&mut self) {
        if self.root.is_some() {
            return; // chrooted: drives-root is not reachable
        }
        self.at_drives_root = true;
        self.cwd = PathBuf::new();
        self.idx = 0;
        self.refresh();
    }

    pub fn cd_parent(&mut self) {
        if self.at_drives_root {
            return;
        }
        if self.at_root() {
            return;
        }
        match self.cwd.parent() {
            Some(p) => self.move_to(p.to_path_buf()),
            None => {
                // No parent — at a drive root like `C:\`. On Windows show the
                // drives list; elsewhere stay put.
                if cfg!(windows) && self.root.is_none() {
                    self.enter_drives_root();
                }
            }
        }
    }

    pub fn cd_into(&mut self, name: &str) {
        let p = self.cwd.join(name);
        // Guard against symlinks/junctions that escape the chroot.
        if let Some(root) = &self.root {
            let canon = p.canonicalize().map(strip_unc).unwrap_or_else(|_| p.clone());
            if !canon.starts_with(root) {
                return;
            }
        }
        self.move_to(p);
    }

    /// Pick a drive from the drives-root view (or any time on Windows).
    pub fn cd_drive(&mut self, letter: &str) {
        if self.root.is_some() {
            return;
        }
        let root = format!("{}:\\", letter);
        self.move_to(PathBuf::from(root));
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.idx)
    }

    pub fn move_up(&mut self) {
        self.idx = self.idx.saturating_sub(1);
    }
    pub fn move_down(&mut self) {
        let max = self.entries.len().saturating_sub(1);
        self.idx = (self.idx + 1).min(max);
    }
    pub fn page_up(&mut self) {
        self.idx = self.idx.saturating_sub(10);
    }
    pub fn page_down(&mut self) {
        let max = self.entries.len().saturating_sub(1);
        self.idx = (self.idx + 10).min(max);
    }
    pub fn home(&mut self) {
        self.idx = 0;
    }
    pub fn end(&mut self) {
        self.idx = self.entries.len().saturating_sub(1);
    }
}

/// Windows canonicalize() returns `\\?\C:\...`; strip the UNC prefix for display.
fn strip_unc(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p
    }
}

pub fn path_label(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.is_empty() {
        "(drives)".to_string()
    } else {
        s.to_string()
    }
}

/// Enumerate available Windows drive letters by probing `X:\`. Returns an
/// empty list on non-Windows platforms.
#[cfg(windows)]
pub fn enum_drives() -> Vec<String> {
    let mut out = Vec::new();
    for c in b'A'..=b'Z' {
        let letter = c as char;
        let root = format!("{}:\\", letter);
        if Path::new(&root).exists() {
            out.push(letter.to_string());
        }
    }
    out
}

#[cfg(not(windows))]
pub fn enum_drives() -> Vec<String> {
    Vec::new()
}
