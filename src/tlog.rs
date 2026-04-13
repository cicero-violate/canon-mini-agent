use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use anyhow::Result;
use crate::events::Event;

/// Total-ordered, append-only log of all system events.
///
/// Every call to `append` atomically writes a JSON record to disk and bumps
/// the monotonic sequence number.  The tlog is the authoritative record of
/// every state transition and effect — it enables full replay and debugging.
pub struct Tlog {
    path: PathBuf,
    seq: u64,
}

impl Tlog {
    /// Open (or create) the tlog at `path`.
    /// Counts existing lines to initialize the sequence number so appends
    /// continue from where the previous run left off.
    pub fn open(path: &Path) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let seq = if path.exists() {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count() as u64
        } else {
            0
        };
        Self {
            path: path.to_path_buf(),
            seq,
        }
    }

    /// Append `event` to the log as a newline-delimited JSON record.
    /// Returns an error only if the write itself fails; callers may choose
    /// to log and continue rather than treating a log write failure as fatal.
    pub fn append(&mut self, event: &Event) -> Result<()> {
        self.seq += 1;
        let record = serde_json::json!({
            "seq": self.seq,
            "ts_ms": crate::logging::now_ms(),
            "event": event,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(&record)?)?;
        Ok(())
    }

    /// Current sequence number (total events appended since the file was created).
    pub fn seq(&self) -> u64 {
        self.seq
    }
}
