use crate::events::{EffectEvent, Event};
use crate::system_state::{replay_event_log, SystemState};
use anyhow::Result;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

struct TlogAppendLock {
    lock_path: PathBuf,
}

impl TlogAppendLock {
    const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
    const OWNERLESS_LOCK_RECLAIM_AGE: Duration = Duration::from_secs(2);

    fn lock_path_for(tlog_path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.lock", tlog_path.display()))
    }

    fn lock_owner_pid(lock_path: &Path) -> Option<u32> {
        let raw = std::fs::read_to_string(lock_path).ok()?;
        raw.trim().parse::<u32>().ok()
    }

    fn process_is_alive(pid: u32) -> Option<bool> {
        if !Path::new("/proc").exists() {
            return None;
        }
        Some(PathBuf::from(format!("/proc/{pid}")).exists())
    }

    fn reclaim_lock_file(lock_path: &Path) -> bool {
        match std::fs::remove_file(lock_path) {
            Ok(()) => true,
            Err(err) if err.kind() == ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }

    fn stale_ownerless_lock(lock_path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(lock_path) else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        modified.elapsed().unwrap_or_default() >= Self::OWNERLESS_LOCK_RECLAIM_AGE
    }

    /// Intent: event_append
    /// Resource: tlog_append_lock
    /// Inputs: tlog append lock path containing an optional owner pid
    /// Outputs: true only when a stale or dead-owner lock file was reclaimed
    /// Effects: may remove stale lock file
    /// Invariants: never reclaims a lock owned by the current process
    fn try_reclaim_stale_lock(lock_path: &Path) -> bool {
        if let Some(owner_pid) = Self::lock_owner_pid(lock_path) {
            if owner_pid == std::process::id() {
                return false;
            }
            if matches!(Self::process_is_alive(owner_pid), Some(false)) {
                return Self::reclaim_lock_file(lock_path);
            }
            return false;
        }
        if Self::stale_ownerless_lock(lock_path) {
            return Self::reclaim_lock_file(lock_path);
        }
        false
    }

    fn lock_owner_label(lock_path: &Path) -> String {
        match Self::lock_owner_pid(lock_path) {
            Some(pid) => match Self::process_is_alive(pid) {
                Some(true) => format!("pid={pid} (alive)"),
                Some(false) => format!("pid={pid} (dead)"),
                None => format!("pid={pid} (liveness=unknown)"),
            },
            None => "pid=unknown".to_string(),
        }
    }

    /// Intent: canonical_write
    /// Resource: error
    /// Inputs: &mut std::fs::File
    /// Outputs: std::result::Result<(), anyhow::Error>
    /// Effects: error
    /// Forbidden: error
    /// Invariants: error
    /// Failure: error
    /// Provenance: rustc:facts + rustc:docstring
    fn write_owner_metadata(file: &mut std::fs::File) -> Result<()> {
        writeln!(file, "{}", std::process::id())?;
        file.flush()?;
        Ok(())
    }

    fn acquire(tlog_path: &Path) -> Result<Self> {
        let lock_path = Self::lock_path_for(tlog_path);
        let start = Instant::now();
        loop {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    if let Err(err) = Self::write_owner_metadata(&mut file) {
                        let _ = Self::reclaim_lock_file(&lock_path);
                        return Err(err);
                    }
                    return Ok(Self { lock_path });
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    if Self::try_reclaim_stale_lock(&lock_path) {
                        continue;
                    }
                    if start.elapsed() >= Self::LOCK_TIMEOUT {
                        let owner = Self::lock_owner_label(&lock_path);
                        return Err(anyhow::anyhow!(
                            "timed out waiting for tlog append lock {} ({owner})",
                            lock_path.display(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
}

impl Drop for TlogAppendLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

fn observed_seq_floor(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }

    observed_seq_from_tail(path).unwrap_or_else(|| observed_seq_floor_slow(path))
}

fn observed_seq_from_tail(path: &Path) -> Option<u64> {
    const TAIL_BYTES: u64 = 8 * 1024;

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return Some(0);
    }

    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);

    let mut line_count = 0_u64;
    let mut max_seq = 0_u64;

    for line in tail.lines().filter(|line| !line.trim().is_empty()) {
        line_count += 1;
        if let Some(seq) = seq_from_record_fragment(line) {
            max_seq = max_seq.max(seq);
        }
    }

    if start == 0 {
        Some(line_count.max(max_seq))
    } else if max_seq > 0 {
        Some(max_seq)
    } else {
        None
    }
}

fn seq_from_record_fragment(fragment: &str) -> Option<u64> {
    let marker = "\"seq\":";
    let start = fragment.rfind(marker)? + marker.len();
    let digits = fragment[start..]
        .chars()
        .skip_while(|ch| ch.is_ascii_whitespace())
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse::<u64>().ok()
}

fn observed_seq_floor_slow(path: &Path) -> u64 {
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut line_count = 0_u64;
    let mut max_seq = 0_u64;

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        line_count += 1;
        if let Some(seq) = seq_from_record_fragment(&line) {
            max_seq = max_seq.max(seq);
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(seq) = value.get("seq").and_then(|seq| seq.as_u64()) {
                max_seq = max_seq.max(seq);
            }
        }
    }

    line_count.max(max_seq)
}

impl Tlog {
    /// Open (or create) the tlog at `path`.
    /// Counts existing lines to initialize the sequence number so appends
    /// continue from where the previous run left off.
    /// Ordinary build or process restarts must preserve this file so replay
    /// and sequence continuity remain intact.
    pub fn open(path: &Path) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let seq = observed_seq_floor(path);
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
        let _append_lock = TlogAppendLock::acquire(&self.path)?;
        let observed = observed_seq_floor(&self.path);
        let next_seq = observed.max(self.seq) + 1;
        let record = serde_json::json!({
            "seq": next_seq,
            "evolution": self.evolution,
            "ts_ms": crate::logging::now_ms(),
            "event": event,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(&record)?)?;
        self.seq = next_seq;
        Ok(())
    }

    /// Intent: canonical_read
    /// Resource: error
    /// Inputs: &std::path::Path
    /// Outputs: std::result::Result<std::vec::Vec<events::Event>, anyhow::Error>
    /// Effects: logging
    /// Forbidden: error
    /// Invariants: error
    /// Failure: error
    /// Provenance: rustc:facts + rustc:docstring
    pub fn read_events(path: &Path) -> Result<Vec<Event>> {
        Ok(Self::read_records(path)?
            .into_iter()
            .map(|record| record.event)
            .collect())
    }

    /// Intent: canonical_read
    /// Resource: error
    /// Inputs: &std::path::Path
    /// Outputs: std::result::Result<std::vec::Vec<tlog::TlogRecord>, anyhow::Error>
    /// Effects: fs_read
    /// Forbidden: error
    /// Invariants: error
    /// Failure: error
    /// Provenance: rustc:facts + rustc:docstring
    pub fn read_records(path: &Path) -> Result<Vec<TlogRecord>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        let file = std::fs::File::open(path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let record: TlogRecord = serde_json::from_str(&line)?;
            records.push(record);
        }
        Ok(records)
    }

    pub fn read_recent_records(path: &Path, max_records: usize) -> Result<Vec<TlogRecord>> {
        if max_records == 0 || !path.exists() {
            return Ok(Vec::new());
        }

        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(Vec::new());
        }

        let mut window = 64 * 1024_u64;
        let mut records = Vec::new();
        loop {
            let start = len.saturating_sub(window);
            file.seek(SeekFrom::Start(start))?;
            let mut bytes = Vec::with_capacity((len - start) as usize);
            file.read_to_end(&mut bytes)?;
            let raw = String::from_utf8_lossy(&bytes);
            let mut lines = raw.lines().collect::<Vec<_>>();
            if start > 0 && !lines.is_empty() {
                lines.remove(0);
            }

            records.clear();
            for line in lines.into_iter().filter(|line| !line.trim().is_empty()) {
                if let Ok(record) = serde_json::from_str::<TlogRecord>(line) {
                    records.push(record);
                }
            }

            if records.len() >= max_records || start == 0 || window >= 4 * 1024 * 1024 {
                break;
            }
            window = (window * 2).min(len);
        }

        if records.len() > max_records {
            let keep_from = records.len() - max_records;
            records.drain(0..keep_from);
        }
        Ok(records)
    }

    pub fn latest_record_by_seq<T, F>(path: &Path, mut select: F) -> Result<Option<T>>
    where
        F: FnMut(Event) -> Option<T>,
    {
        let mut latest: Option<(u64, T)> = None;
        for record in Self::read_records(path)? {
            let Some(value) = select(record.event) else {
                continue;
            };
            let replace = match latest.as_ref() {
                None => true,
                Some((seq, _)) => record.seq >= *seq,
            };
            if replace {
                latest = Some((record.seq, value));
            }
        }
        Ok(latest.map(|(_, value)| value))
    }

    pub fn latest_effect_by_seq<T, F>(path: &Path, mut select: F) -> Result<Option<T>>
    where
        F: FnMut(EffectEvent) -> Option<T>,
    {
        Self::latest_record_by_seq(path, |event| match event {
            Event::Effect { event } => select(event),
            _ => None,
        })
    }

    pub fn latest_effect_from_workspace<T, F>(workspace: &Path, select: F) -> Option<T>
    where
        F: FnMut(EffectEvent) -> Option<T>,
    {
        let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
        Self::latest_effect_by_seq(&tlog_path, select).ok()?
    }

    pub fn replay_with_lane_inference(events: &[Event]) -> Result<SystemState> {
        let lane_indices = crate::events::lane_indices_from_events(events);
        let lane_count = lane_indices.iter().max().map(|idx| idx + 1).unwrap_or(1);
        let initial = SystemState::new(&lane_indices, lane_count);
        replay_event_log(initial, events).map_err(anyhow::Error::msg)
    }

    pub fn replay_canonical_state(path: &Path) -> Result<SystemState> {
        let events = Self::read_events(path)?;
        Self::replay_with_lane_inference(&events)
    }

    pub fn replay(path: &Path, initial: SystemState) -> Result<SystemState> {
        let events = Self::read_events(path)?;
        replay_event_log(initial, &events).map_err(anyhow::Error::msg)
    }

    /// Current sequence number (total events appended since the file was created).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Intent: canonical_write
    /// Resource: error
    /// Inputs: &mut tlog::Tlog, u64
    /// Outputs: ()
    /// Effects: error
    /// Forbidden: error
    /// Invariants: error
    /// Failure: error
    /// Provenance: rustc:facts + rustc:docstring
    pub fn set_evolution(&mut self, evolution: u64) {
        self.evolution = evolution;
    }
}

#[cfg(test)]
mod tests {
    use super::Tlog;
    use crate::events::{ControlEvent, Event};
    use std::path::{Path, PathBuf};

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!(
                "canon-mini-agent-tlog-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system time before unix epoch")
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).expect("create temp test dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn planner_pending_event(pending: bool) -> Event {
        Event::control(ControlEvent::PlannerPendingSet { pending })
    }

    #[test]
    fn stale_handles_share_monotonic_seq_for_same_path() {
        let dir = TestDir::new();
        let path = dir.path().join("tlog.ndjson");

        let mut first = Tlog::open(&path);
        let mut stale = Tlog::open(&path);
        first
            .append(&planner_pending_event(true))
            .expect("first append");
        stale
            .append(&planner_pending_event(false))
            .expect("stale append");

        let records = Tlog::read_records(&path).expect("read records");
        let seqs: Vec<u64> = records.into_iter().map(|record| record.seq).collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn open_uses_observed_seq_floor_not_raw_line_count_only() {
        let dir = TestDir::new();
        let path = dir.path().join("tlog.ndjson");
        std::fs::write(
            &path,
            concat!(
                "{\"seq\":10,\"ts_ms\":1,\"event\":{\"class\":\"control\",\"event\":{\"kind\":\"planner_pending_set\",\"pending\":true}}}\n",
                "{\"seq\":4,\"ts_ms\":2,\"event\":{\"class\":\"control\",\"event\":{\"kind\":\"planner_pending_set\",\"pending\":false}}}\n"
            ),
        )
        .expect("seed tlog");

        let mut tlog = Tlog::open(&path);
        tlog.append(&planner_pending_event(true)).expect("append");

        let records = Tlog::read_records(&path).expect("read records");
        let last_seq = records.last().map(|record| record.seq).expect("last seq");
        assert_eq!(last_seq, 11);
    }

    #[test]
    fn latest_record_by_seq_returns_latest_matching_value() {
        let dir = TestDir::new();
        let path = dir.path().join("tlog.ndjson");

        let mut tlog = Tlog::open(&path);
        tlog.append(&planner_pending_event(true))
            .expect("append planner pending true");
        tlog.append(&Event::control(ControlEvent::PhaseSet {
            phase: "executor".to_string(),
            lane: None,
        }))
        .expect("append phase set");
        tlog.append(&planner_pending_event(false))
            .expect("append planner pending false");

        let latest = Tlog::latest_record_by_seq(&path, |event| match event {
            Event::Control {
                event: ControlEvent::PlannerPendingSet { pending },
            } => Some(pending),
            _ => None,
        })
        .expect("latest record")
        .expect("planner pending event present");

        assert!(!latest);
    }

    #[test]
    fn append_reclaims_stale_lock_from_dead_pid() {
        if !Path::new("/proc").exists() {
            return;
        }

        let dir = TestDir::new();
        let path = dir.path().join("tlog.ndjson");
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        std::fs::write(&lock_path, "999999\n").expect("seed stale lock");

        let mut tlog = Tlog::open(&path);
        tlog.append(&planner_pending_event(true))
            .expect("append reclaims stale lock");

        let records = Tlog::read_records(&path).expect("read records");
        assert_eq!(records.len(), 1);
    }
}
