use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, TryRecvError},
        Arc,
    },
    thread,
};

pub struct GrepHit {
    pub session_idx: usize,
    pub snippet: String,
}

pub struct GrepJob {
    pub cancelled: Arc<AtomicBool>,
    pub rx: Receiver<GrepHit>,
}

impl GrepJob {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

impl Drop for GrepJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Spawn a background grep over the provided (session_idx, jsonl_path) pairs.
/// Sends at most one GrepHit per session (the first match). Stops promptly on cancel.
pub fn spawn(targets: Vec<(usize, PathBuf)>, query: String) -> GrepJob {
    let (tx, rx) = channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_thread = cancelled.clone();

    thread::spawn(move || {
        let q = query.to_lowercase();
        if q.is_empty() {
            return;
        }
        for (idx, path) in targets {
            if cancelled_thread.load(Ordering::SeqCst) {
                return;
            }
            let Ok(file) = File::open(&path) else { continue };
            let reader = BufReader::new(file);
            for line in reader.lines() {
                if cancelled_thread.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(line) = line else { continue };
                let lower = line.to_lowercase();
                if let Some(pos) = lower.find(&q) {
                    // pos is a byte index into `lower`. For most scripts
                    // lowercase preserves byte length (ASCII, Cyrillic) but
                    // for some (Turkish İ → i̇, German ß → ss) it differs.
                    // We snippet from `lower` to stay byte-safe.
                    let snippet = make_snippet(&lower, pos, q.len());
                    if tx
                        .send(GrepHit {
                            session_idx: idx,
                            snippet,
                        })
                        .is_err()
                    {
                        return;
                    }
                    break; // first hit per session is enough
                }
            }
        }
    });

    GrepJob { cancelled, rx }
}

/// Non-blocking drain. Returns (new_hits, completed).
pub fn drain(job: &GrepJob, into: &mut Vec<GrepHit>) -> (bool, bool) {
    let mut pushed = false;
    loop {
        match job.rx.try_recv() {
            Ok(h) => {
                into.push(h);
                pushed = true;
            }
            Err(TryRecvError::Empty) => return (pushed, false),
            Err(TryRecvError::Disconnected) => return (pushed, true),
        }
    }
}

/// Extract a short window around the match position, clamped to char boundaries.
/// Replaces newlines with spaces.
fn make_snippet(line: &str, pos: usize, qlen: usize) -> String {
    let want_before: usize = 40;
    let want_after: usize = 60;

    // walk left to a char boundary
    let mut start = pos.saturating_sub(want_before);
    while start > 0 && !line.is_char_boundary(start) {
        start -= 1;
    }
    let want_end = pos + qlen + want_after;
    let mut end = want_end.min(line.len());
    while end < line.len() && !line.is_char_boundary(end) {
        end += 1;
    }

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&line[start..end].replace(['\n', '\r', '\t'], " "));
    if end < line.len() {
        out.push('…');
    }
    out
}
