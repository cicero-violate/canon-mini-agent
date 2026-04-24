use crate::events::{EffectEvent, Event};
use crate::system_state::{replay_event_log, SystemState};
use anyhow::Result;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
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

    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut line_count = 0_u64;
    let mut max_seq = 0_u64;

    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        line_count += 1;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
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
    pub fn read_events(path: &Path) -> Result<Vec<Event>> {
        Ok(Self::read_records(path)?
            .into_iter()
            .map(|record| record.event)
            .collect())
    }

    /// Intent: canonical_read
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
