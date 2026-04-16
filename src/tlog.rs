use crate::events::Event;
use crate::system_state::{replay_event_log, SystemState};
use anyhow::Result;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TlogRecord {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub ts_ms: u64,
    pub event: Event,
}

/// Total-ordered, append-only log of all system events.
///
/// Every call to `append` atomically writes a JSON record to disk and bumps
/// the monotonic sequence number.  The tlog is the authoritative record of
/// every state transition and effect — it enables full replay and debugging.
pub struct Tlog {
    path: PathBuf,
    seq: u64,
    evolution: u64,
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
            evolution: crate::evolution::current_evolution_for_tlog(path),
        }
    }

    /// Append `event` to the log as a newline-delimited JSON record.
    /// Returns an error only if the write itself fails; callers may choose
    /// to log and continue rather than treating a log write failure as fatal.
    pub fn append(&mut self, event: &Event) -> Result<()> {
        self.seq += 1;
        let record = serde_json::json!({
            "seq": self.seq,
            "evolution": self.evolution,
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

    pub fn read_events(path: &Path) -> Result<Vec<Event>> {
        Ok(Self::read_records(path)?
            .into_iter()
            .map(|record| record.event)
            .collect())
    }

    pub fn read_records(path: &Path) -> Result<Vec<TlogRecord>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(path)?;
        let mut records = Vec::new();
        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            let record: TlogRecord = serde_json::from_str(line)?;
            records.push(record);
        }
        Ok(records)
    }

    pub fn replay(path: &Path, initial: SystemState) -> Result<SystemState> {
        let events = Self::read_events(path)?;
        replay_event_log(initial, &events).map_err(anyhow::Error::msg)
    }

    /// Current sequence number (total events appended since the file was created).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn set_evolution(&mut self, evolution: u64) {
        self.evolution = evolution;
    }
}
