//! Durable log file for the console-less overlay, and its tail.
//!
//! The overlay has no stdout/stderr, so tracing also goes to
//! `<log dir>/tze_hud.log`, rotated to `tze_hud.log.1` at [`MAX_LOG_BYTES`].
//! The same directory holds `hud-diag.log` (panics, see [`crate::diag`]),
//! bounded by the same rotation through [`append_rotating`].
//! `GET /admin/logs?tail=N` returns the last N lines across both files.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Size at which the log rotates to `.1` (one previous file is kept).
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Most lines one tail request may return.
pub const MAX_TAIL_LINES: usize = 2000;

pub const LOG_FILE_NAME: &str = "tze_hud.log";

/// Where logs live: `TZE_HUD_LOG_DIR`, else `%LOCALAPPDATA%\tze_hud\logs`, else
/// `<temp>/tze_hud/logs`.
pub fn log_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TZE_HUD_LOG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if cfg!(windows)
        && let Some(base) = std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty())
    {
        return PathBuf::from(base).join("tze_hud").join("logs");
    }
    std::env::temp_dir().join("tze_hud").join("logs")
}

pub fn log_path() -> PathBuf {
    log_dir().join(LOG_FILE_NAME)
}

fn rotated(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// Move `path` to `<path>.1`, replacing an older `.1`.
fn rotate(path: &Path) -> io::Result<()> {
    let prev = rotated(path);
    // Windows cannot rename over an existing file.
    let _ = std::fs::remove_file(&prev);
    std::fs::rename(path, &prev)
}

/// Open `path` for appending, creating it.
fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Append `buf` to `path` (opened per call), rotating first when it would
/// pass `cap`. For rare writers such as the panic hook: it holds no lock, so
/// a panic while writing cannot deadlock the next write. If rotation fails
/// the file just grows past `cap`; the line is never dropped for it.
pub fn append_rotating(path: &Path, buf: &[u8], cap: u64) -> io::Result<()> {
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    if size > 0 && size + buf.len() as u64 > cap {
        let _ = rotate(path);
    }
    open_append(path)?.write_all(buf)
}

struct State {
    file: File,
    size: u64,
    /// The rename to `.1` succeeded but reopening `path` failed: `file` still
    /// writes to `.1`. Retry only the reopen; rotating again would delete
    /// `.1` and leave `file` on an unlinked inode.
    reopen_pending: bool,
}

type Opener = fn(&Path) -> io::Result<File>;

/// Append-only log file that rotates to `<path>.1` when it reaches `cap`.
pub struct RotatingFile {
    path: PathBuf,
    cap: u64,
    state: Mutex<State>,
    /// [`open_append`]; replaceable in tests to fail the reopen.
    open: Opener,
}

impl RotatingFile {
    /// Open (creating the directory and file) for appending.
    pub fn open(path: PathBuf, cap: u64) -> io::Result<Arc<Self>> {
        Self::open_with(path, cap, open_append)
    }

    fn open_with(path: PathBuf, cap: u64, open: Opener) -> io::Result<Arc<Self>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = open(&path)?;
        let size = file.metadata()?.len();
        Ok(Arc::new(Self {
            path,
            cap,
            state: Mutex::new(State {
                file,
                size,
                reopen_pending: false,
            }),
            open,
        }))
    }

    fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // A failed rotation keeps the line: it goes to the file still open
        // (past the cap, or the just-rotated `.1` if the reopen fails), and
        // the next write retries the step that failed.
        let rotate_now = st.reopen_pending
            || (st.size > 0 && st.size + buf.len() as u64 > self.cap && rotate(&self.path).is_ok());
        if rotate_now {
            match (self.open)(&self.path) {
                Ok(file) => {
                    st.file = file;
                    st.size = 0;
                    st.reopen_pending = false;
                }
                Err(_) => st.reopen_pending = true,
            }
        }
        st.file.write_all(buf)?;
        st.size += buf.len() as u64;
        Ok(())
    }
}

/// `io::Write` handle for tracing's `MakeWriter` (`move || LogWriter(file.clone())`).
/// Rotation happens between `write` calls, so a writer must emit a whole log
/// line per call (tracing's fmt layer does).
pub struct LogWriter(pub Arc<RotatingFile>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The last `n` lines of `path` (fewer if the file is shorter), oldest first,
/// read backwards in blocks so a large file costs only its tail.
/// Each byte is read and scanned once (blocks are counted as they arrive and
/// joined once), so a long line costs its length, not its length squared.
fn last_lines(path: &Path, n: usize) -> io::Result<Vec<String>> {
    const BLOCK: u64 = 8192;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path)?;
    let mut pos = file.metadata()?.len();
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    let mut newlines = 0;
    // Need n complete lines plus the newline that precedes the first of them.
    while pos > 0 && newlines <= n {
        let take = BLOCK.min(pos);
        pos -= take;
        file.seek(SeekFrom::Start(pos))?;
        let mut block = vec![0u8; take as usize];
        file.read_exact(&mut block)?;
        newlines += block.iter().filter(|&&b| b == b'\n').count();
        blocks.push(block);
    }
    blocks.reverse();
    let data = blocks.concat();
    let text = String::from_utf8_lossy(&data);
    let mut lines: Vec<&str> = text.lines().collect();
    if pos > 0 && !lines.is_empty() {
        lines.remove(0); // partial first line of a mid-file block
    }
    let skip = lines.len().saturating_sub(n);
    Ok(lines[skip..].iter().map(|l| (*l).to_owned()).collect())
}

/// The last `n` (clamped to [`MAX_TAIL_LINES`]) lines of the log at `path`,
/// continuing into `<path>.1` when the current file is shorter than `n`.
pub fn tail(path: &Path, n: usize) -> String {
    let n = n.min(MAX_TAIL_LINES);
    if n == 0 {
        return String::new();
    }
    let mut lines = last_lines(path, n).unwrap_or_default();
    if lines.len() < n
        && let Ok(mut older) = last_lines(&rotated(path), n - lines.len())
    {
        older.append(&mut lines);
        lines = older;
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tze_hud_logs_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(LOG_FILE_NAME)
    }

    #[test]
    fn rotates_at_the_cap_and_tail_spans_the_boundary() {
        let path = temp("rotate");
        let file = RotatingFile::open(path.clone(), 100).unwrap();
        let mut w = LogWriter(file);
        for i in 0..12 {
            // One write per event, as tracing's fmt layer does; 19 bytes each.
            w.write_all(format!("line {i:02} xxxxxxxxxx\n").as_bytes())
                .unwrap();
        }
        let prev = std::fs::metadata(rotated(&path)).unwrap().len();
        assert!(prev > 0 && prev <= 100, "rotated file holds <= cap: {prev}");
        assert!(std::fs::metadata(&path).unwrap().len() <= 100);
        assert!(!rotated(&rotated(&path)).exists(), "only one old file kept");

        let cur = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(cur < 7, "current file alone cannot satisfy the tail: {cur}");
        // Lines 0..5 were rotated out of the single kept `.1` file.
        let want: Vec<String> = (5..12).map(|i| format!("line {i:02} xxxxxxxxxx")).collect();
        for n in [7, 20] {
            assert_eq!(tail(&path, n).lines().collect::<Vec<_>>(), want, "n={n}");
        }

        // The rare-writer path (hud-diag.log) is bounded by the same rotation.
        let diag = temp("rotate_diag").with_file_name("hud-diag.log");
        std::fs::create_dir_all(diag.parent().unwrap()).unwrap();
        for i in 0..12 {
            append_rotating(&diag, format!("diag {i:02} xxxxxxxxxx\n").as_bytes(), 100).unwrap();
        }
        assert!(std::fs::metadata(&diag).unwrap().len() <= 100);
        assert!(std::fs::metadata(rotated(&diag)).unwrap().len() <= 100);
        assert!(tail(&diag, 1).starts_with("diag 11"));
    }

    #[test]
    fn a_failed_rotation_keeps_every_line() {
        // `<path>.1` is a non-empty directory, so the rotate rename fails.
        let path = temp("stuck");
        std::fs::create_dir_all(rotated(&path).join("x")).unwrap();
        let mut w = LogWriter(RotatingFile::open(path.clone(), 50).unwrap());
        for i in 0..6 {
            w.write_all(format!("kept {i} xxxxxxxxxx\n").as_bytes())
                .unwrap();
        }
        let diag = path.with_file_name("hud-diag.log");
        std::fs::create_dir_all(rotated(&diag).join("x")).unwrap();
        for i in 0..6 {
            append_rotating(&diag, format!("kept {i} xxxxxxxxxx\n").as_bytes(), 50).unwrap();
        }
        for p in [&path, &diag] {
            let got = std::fs::read_to_string(p).unwrap();
            assert_eq!(
                got.lines().count(),
                6,
                "{p:?} past the cap, nothing dropped"
            );
        }
    }

    #[test]
    fn a_failed_reopen_after_rotating_retries_only_the_reopen() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static FAIL_OPEN: AtomicBool = AtomicBool::new(false);
        fn flaky(p: &Path) -> io::Result<File> {
            if FAIL_OPEN.load(Ordering::SeqCst) {
                return Err(io::Error::other("cannot open"));
            }
            open_append(p)
        }
        let path = temp("reopen");
        let mut w = LogWriter(RotatingFile::open_with(path.clone(), 30, flaky).unwrap());
        w.write_all(b"one xxxxxxxxxxxxxxxxxxxxx\n").unwrap();
        FAIL_OPEN.store(true, Ordering::SeqCst);
        // Rotates `one` to .1, the reopen fails: these land in .1, which must
        // survive (no second rotation deletes it).
        w.write_all(b"two xxxxxxxxxxxxxxxxxxxxx\n").unwrap();
        w.write_all(b"three xxxxxxxxxxxxxxxxxxx\n").unwrap();
        FAIL_OPEN.store(false, Ordering::SeqCst);
        w.write_all(b"four\n").unwrap();
        w.write_all(b"five\n").unwrap();
        let old = std::fs::read_to_string(rotated(&path)).unwrap();
        assert_eq!(
            old.lines()
                .map(|l| &l[..l.find(' ').unwrap_or(l.len())])
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "four\nfive\n");
    }

    #[test]
    fn tail_reads_backwards_across_blocks_and_clamps() {
        let path = temp("blocks");
        let file = RotatingFile::open(path.clone(), MAX_LOG_BYTES).unwrap();
        let mut w = LogWriter(file);
        for i in 0..3000 {
            writeln!(w, "entry {i}").unwrap();
        }
        let out = tail(&path, 5);
        assert_eq!(
            out,
            "entry 2995\nentry 2996\nentry 2997\nentry 2998\nentry 2999\n"
        );
        assert_eq!(tail(&path, 10_000).lines().count(), MAX_TAIL_LINES);
        assert_eq!(tail(&temp("missing"), 5), "");
        // tail=0 does no I/O at all (it would fail on a missing file).
        assert!(last_lines(&temp("missing"), 0).unwrap().is_empty());
        assert_eq!(tail(&path, 0), "");

        // One 2 MiB line spans 256 blocks; each byte is scanned once.
        let path = temp("long");
        let mut w = LogWriter(RotatingFile::open(path.clone(), MAX_LOG_BYTES).unwrap());
        let long = "x".repeat(2 << 20);
        writeln!(w, "{long}").unwrap();
        writeln!(w, "a").unwrap();
        writeln!(w, "b").unwrap();
        let out = tail(&path, 3);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!((lines[0].len(), &lines[1..]), (long.len(), &["a", "b"][..]));
    }
}
