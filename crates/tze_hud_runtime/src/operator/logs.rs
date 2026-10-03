//! Durable log file for the console-less overlay, and its tail.
//!
//! The overlay has no stdout/stderr, so tracing also goes to
//! `<log dir>/tze_hud.log`, rotated to `tze_hud.log.1` at [`MAX_LOG_BYTES`].
//! The same directory holds `hud-diag.log` (panics, see [`crate::diag`]).
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

struct State {
    file: File,
    size: u64,
}

/// Append-only log file that rotates to `<path>.1` when it reaches `cap`.
pub struct RotatingFile {
    path: PathBuf,
    cap: u64,
    state: Mutex<State>,
}

impl RotatingFile {
    /// Open (creating the directory and file) for appending.
    pub fn open(path: PathBuf, cap: u64) -> io::Result<Arc<Self>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        Ok(Arc::new(Self {
            path,
            cap,
            state: Mutex::new(State { file, size }),
        }))
    }

    fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.size > 0 && st.size + buf.len() as u64 > self.cap {
            let prev = rotated(&self.path);
            // Windows cannot rename over an existing file.
            let _ = std::fs::remove_file(&prev);
            std::fs::rename(&self.path, &prev)?;
            st.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            st.size = 0;
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
fn last_lines(path: &Path, n: usize) -> io::Result<Vec<String>> {
    const BLOCK: u64 = 8192;
    let mut file = File::open(path)?;
    let mut pos = file.metadata()?.len();
    let mut data: Vec<u8> = Vec::new();
    // Need n complete lines plus the newline that precedes the first of them.
    while pos > 0 && data.iter().filter(|&&b| b == b'\n').count() <= n {
        let take = BLOCK.min(pos);
        pos -= take;
        file.seek(SeekFrom::Start(pos))?;
        let mut block = vec![0u8; take as usize];
        file.read_exact(&mut block)?;
        block.extend_from_slice(&data);
        data = block;
    }
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
    }
}
