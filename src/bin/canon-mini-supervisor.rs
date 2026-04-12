use anyhow::{anyhow, Context, Result};
use canon_mini_agent::{set_agent_state_dir, set_workspace};
use canon_mini_agent::logging::init_log_paths;
use canon_mini_agent::logging::log_error_event;
use canon_mini_agent::complexity::write_complexity_report;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, atomic::{AtomicBool, AtomicU32, Ordering}};
use std::thread;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug)]
enum BuildKind {
    Debug,
    Release,
}

#[derive(Clone, Debug)]
struct BinaryCandidate {
    path: PathBuf,
    kind: BuildKind,
    mtime: SystemTime,
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join("target").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

fn binary_path(root: &Path, kind: BuildKind) -> PathBuf {
    match kind {
        BuildKind::Debug => root.join("target").join("debug").join("canon-mini-agent"),
        BuildKind::Release => root.join("target").join("release").join("canon-mini-agent"),
    }
}

fn tickets_binary_path(root: &Path, kind: BuildKind) -> PathBuf {
    match kind {
        BuildKind::Debug => root.join("target").join("debug").join("canon-tickets"),
        BuildKind::Release => root.join("target").join("release").join("canon-tickets"),
    }
}

fn candidate_from_path(path: PathBuf, kind: BuildKind) -> Result<BinaryCandidate> {
    let meta = fs::metadata(&path).with_context(|| format!("metadata: {}", path.display()))?;
    let mtime = meta.modified().with_context(|| format!("mtime: {}", path.display()))?;
    Ok(BinaryCandidate { path, kind, mtime })
}

fn newest_candidate(root: &Path, prefer_release: bool) -> Result<BinaryCandidate> {
    let mut candidates = Vec::new();
    for (kind, prefer) in [
        (BuildKind::Release, prefer_release),
        (BuildKind::Debug, !prefer_release),
    ] {
        let path = binary_path(root, kind);
        if path.exists() {
            candidates.push((prefer, candidate_from_path(path, kind)?));
        }
    }
    if candidates.is_empty() {
        return Err(anyhow!(
            "canon-mini-agent binary not found in target/{}/ or target/{}/",
            "debug",
            "release"
        ));
    }
    candidates.sort_by(|(pref_a, a), (pref_b, b)| {
        pref_b
            .cmp(pref_a)
            .then_with(|| b.mtime.cmp(&a.mtime))
    });
    Ok(candidates[0].1.clone())
}

fn spawn_child(bin: &BinaryCandidate, args: &[String]) -> Result<Child> {
    let mut cmd = Command::new(&bin.path);
    cmd.args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("spawn {}", bin.path.display()))?;
    if let Some(stdout) = child.stdout.take() {
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut buf = [0u8; 8192];
            let mut out = std::io::stdout();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = out.write_all(&buf[..n]);
                        let _ = out.flush();
                    }
                    Err(_) => break,
                }
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stderr);
            let mut buf = [0u8; 8192];
            let mut out = std::io::stderr();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = out.write_all(&buf[..n]);
                        let _ = out.flush();
                    }
                    Err(_) => break,
                }
            }
        });
    }
    Ok(child)
}

fn send_sigint(child: &Child) {
    let pid = child.id();
    let _ = Command::new("kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status();
}

fn wait_for_exit(child: &mut Child, timeout: Duration) {
    let start = SystemTime::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed().unwrap_or_default() > timeout {
                    log_error_event(
                        "supervisor",
                        "supervisor_wait_for_exit",
                        None,
                        "wait_for_exit timed out; killing child process",
                        None,
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(200));
            }
            Err(err) => {
                log_error_event(
                    "supervisor",
                    "supervisor_wait_for_exit",
                    None,
                    &format!("wait_for_exit try_wait error: {err:#}"),
                    None,
                );
                break;
            }
        }
    }
}

fn has_updated(root: &Path, last: &BinaryCandidate) -> Result<Option<BinaryCandidate>> {
    let mut updated = None;
    for kind in [BuildKind::Debug, BuildKind::Release] {
        let path = binary_path(root, kind);
        if !path.exists() {
            continue;
        }
        let cand = candidate_from_path(path, kind)?;
        if cand.mtime > last.mtime {
            updated = Some(cand);
        }
    }
    Ok(updated)
}

fn agent_state_dir_from_args(args: &[String]) -> PathBuf {
    let mut i = 0usize;
    while i + 1 < args.len() {
        if args[i] == "--state-dir" {
            return PathBuf::from(&args[i + 1]);
        }
        i += 1;
    }
    PathBuf::from("/workspace/ai_sandbox/canon-mini-agent/agent_state")
}

fn workspace_from_args(args: &[String]) -> Option<String> {
    let mut i = 0usize;
    while i + 1 < args.len() {
        if args[i] == "--workspace" {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

fn cycle_idle_marker_path(args: &[String]) -> PathBuf {
    agent_state_dir_from_args(args).join("orchestrator_cycle_idle.flag")
}

fn orchestrator_mode_flag_path(args: &[String]) -> PathBuf {
    agent_state_dir_from_args(args).join("orchestrator_mode.flag")
}

fn read_orchestrator_mode(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn file_mtime_if_exists(path: &Path) -> Option<SystemTime> {
    let meta = fs::metadata(path).ok()?;
    meta.modified().ok()
}

fn run_cmd(root: &Path, program: &str, args: &[&str]) -> Result<bool> {
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("run {} {}", program, args.join(" ")))?;
    Ok(status.success())
}

fn run_ticket_refresh(root: &Path, kind: BuildKind) {
    let bin = tickets_binary_path(root, kind);
    if !bin.exists() {
        eprintln!(
            "[canon-mini-supervisor] ticket refresh skipped (missing {}); run `cargo build` to produce it",
            bin.display()
        );
        return;
    }

    let ws = root.to_string_lossy();
    let args = [
        "--workspace",
        ws.as_ref(),
        "--all-crates",
        "--top",
        "3",
        "--prune",
    ];
    eprintln!(
        "[canon-mini-supervisor] pre-restart: refreshing refactor tickets via {}",
        bin.display()
    );
    let status = Command::new(&bin)
        .args(args)
        .current_dir(root)
        .status();
    match status {
        Ok(st) if st.success() => {
            eprintln!("[canon-mini-supervisor] ticket refresh ok");
        }
        Ok(st) => {
            eprintln!(
                "[canon-mini-supervisor] ticket refresh failed (status={st}); continuing restart"
            );
        }
        Err(err) => {
            eprintln!(
                "[canon-mini-supervisor] ticket refresh errored ({err:#}); continuing restart"
            );
        }
    }
}

fn stage_commit_push_before_restart(root: &Path, reason: &str, prefer_release: bool) {
    eprintln!(
        "[canon-mini-supervisor] pre-restart checkpoint start ({reason})"
    );
    eprintln!(
        "[canon-mini-supervisor] pre-restart: running `cargo build --workspace` ({reason})"
    );
    match run_cmd(root, "cargo", &["build", "--workspace"]) {
        Ok(true) => {
            eprintln!(
                "[canon-mini-supervisor] pre-restart: cargo build passed ({reason})"
            );
        }
        Ok(false) => {
            eprintln!(
                "[canon-mini-supervisor] pre-restart cargo build failed; skipping git add/commit/push ({reason})"
            );
            return;
        }
        Err(err) => {
            eprintln!(
                "[canon-mini-supervisor] pre-restart cargo build errored; skipping git add/commit/push ({reason}): {err:#}"
            );
            return;
        }
    }

    // IMPORTANT: `cargo build` (with rustc wrapper) generates the latest state/rustc/*/graph.json.
    // Refresh the top auto-generated refactor tickets *after* the build, before staging/committing.
    // Use the same build kind preference as the watched binary.
    run_ticket_refresh(
        root,
        if prefer_release {
            BuildKind::Release
        } else {
            BuildKind::Debug
        },
    );

    eprintln!(
        "[canon-mini-supervisor] pre-restart: running `git add -A` ({reason})"
    );
    if let Err(err) = run_cmd(root, "git", &["add", "-A"]) {
        eprintln!("[canon-mini-supervisor] git add failed ({reason}): {err:#}");
        return;
    }
    eprintln!(
        "[canon-mini-supervisor] pre-restart: git add completed ({reason})"
    );

    let has_changes = match run_cmd(root, "git", &["diff", "--cached", "--quiet"]) {
        Ok(true) => false,
        Ok(false) => true,
        Err(err) => {
            eprintln!("[canon-mini-supervisor] git diff --cached failed ({reason}): {err:#}");
            return;
        }
    };
    if !has_changes {
        eprintln!(
            "[canon-mini-supervisor] no staged changes after successful build; skipping commit/push ({reason})"
        );
        return;
    }

    let commit_msg = format!("supervisor pre-restart checkpoint ({reason})");
    eprintln!(
        "[canon-mini-supervisor] pre-restart: running `git commit -m \"{}\"` ({reason})",
        commit_msg
    );
    match run_cmd(root, "git", &["commit", "-m", &commit_msg]) {
        Ok(true) => {
            eprintln!(
                "[canon-mini-supervisor] pre-restart: git commit completed ({reason})"
            );
        }
        Ok(false) => {
            eprintln!("[canon-mini-supervisor] git commit returned non-zero ({reason})");
            return;
        }
        Err(err) => {
            eprintln!("[canon-mini-supervisor] git commit failed ({reason}): {err:#}");
            return;
        }
    }

    eprintln!(
        "[canon-mini-supervisor] pre-restart: running `git push` ({reason})"
    );
    match run_cmd(root, "git", &["push"]) {
        Ok(true) => {
            eprintln!(
                "[canon-mini-supervisor] pre-restart: git push completed ({reason})"
            );
            eprintln!(
                "[canon-mini-supervisor] pre-restart checkpoint done ({reason})"
            );
        }
        Ok(false) => {
            eprintln!("[canon-mini-supervisor] git push returned non-zero ({reason})");
        }
        Err(err) => {
            eprintln!("[canon-mini-supervisor] git push failed ({reason}): {err:#}");
        }
    }
}

fn main() -> Result<()> {
    let SupervisorArgs {
        exe,
        prefer_release,
        no_watch,
        filtered_args,
    } = parse_supervisor_args();
    let start_dir = std::env::current_dir().context("current_dir")?;
    let root = find_workspace_root(&start_dir)
        .ok_or_else(|| anyhow!("unable to locate workspace root with target/"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let child_pid = Arc::new(AtomicU32::new(0));
    initialize_supervisor_runtime(&filtered_args, &shutdown, &child_pid)?;
    let idle_marker = cycle_idle_marker_path(&filtered_args);
    let orchestrator_mode_flag = orchestrator_mode_flag_path(&filtered_args);

    loop {
        let current = newest_candidate(&root, prefer_release)?;
        emit_iteration_status_and_report(&exe, &root, &current, &filtered_args);
        let (mut child, mut pending_update, child_started_at) =
            start_supervisor_child(&current, &filtered_args, &child_pid)?;

        if supervise_current_child(
            shutdown.as_ref(),
            no_watch,
            &root,
            &current,
            &mut pending_update,
            &orchestrator_mode_flag,
            &idle_marker,
            child_started_at,
            prefer_release,
            &mut child,
        )? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(1000));
    }
}

fn emit_iteration_status_and_report(
    exe: &str,
    root: &Path,
    current: &BinaryCandidate,
    filtered_args: &[String],
) {
    eprintln!(
        "[canon-mini-supervisor] exec={} root={} watching={}",
        exe,
        root.display(),
        current.path.display()
    );
    let report_workspace = workspace_from_args(filtered_args)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.to_path_buf());
    emit_complexity_report_status(&report_workspace);
}

fn supervise_current_child(
    shutdown: &AtomicBool,
    no_watch: bool,
    root: &Path,
    current: &BinaryCandidate,
    pending_update: &mut Option<BinaryCandidate>,
    orchestrator_mode_flag: &Path,
    idle_marker: &Path,
    child_started_at: SystemTime,
    prefer_release: bool,
    child: &mut Child,
) -> Result<bool> {
    loop {
        thread::sleep(Duration::from_millis(1000));
        if handle_shutdown_request(shutdown, child) {
            return Ok(true);
        }
        if handle_child_exit_status(child.try_wait().context("wait child")?, root, prefer_release) {
            break;
        }
        if should_restart_for_pending_update(
            no_watch,
            root,
            current,
            pending_update,
            orchestrator_mode_flag,
            idle_marker,
            child_started_at,
            prefer_release,
            child,
        )? {
            break;
        }
    }
    Ok(false)
}

fn handle_shutdown_request(shutdown: &AtomicBool, child: &mut Child) -> bool {
    if !shutdown.load(Ordering::SeqCst) {
        return false;
    }
    eprintln!("[canon-mini-supervisor] shutdown requested; waiting for child");
    log_error_event(
        "supervisor",
        "supervisor_main",
        None,
        "shutdown requested; waiting for child",
        None,
    );
    wait_for_exit(child, Duration::from_secs(10));
    true
}

fn handle_child_exit_status(
    status: Option<ExitStatus>,
    root: &Path,
    prefer_release: bool,
) -> bool {
    let Some(status) = status else {
        return false;
    };
    eprintln!("[canon-mini-supervisor] child exited: {status}");
    if status.success() {
        eprintln!("[canon-mini-supervisor] child exited cleanly; not restarting");
        std::process::exit(0);
    }
    eprintln!("[canon-mini-supervisor] restarting due to failure...");
    stage_commit_push_before_restart(root, "failure-restart", prefer_release);
    log_error_event(
        "supervisor",
        "supervisor_main",
        None,
        &format!("child exited unsuccessfully: {status}"),
        None,
    );
    true
}

fn initialize_supervisor_runtime(
    filtered_args: &[String],
    shutdown: &Arc<AtomicBool>,
    child_pid: &Arc<AtomicU32>,
) -> Result<()> {
    // Initialize structured logging for the supervisor itself. These settings are derived from the
    // same args we forward to the child binary so logs land in the same workspace/state-dir.
    if let Some(workspace) = workspace_from_args(filtered_args) {
        set_workspace(workspace);
    }
    set_agent_state_dir(agent_state_dir_from_args(filtered_args).to_string_lossy().to_string());
    init_log_paths("supervisor");
    install_supervisor_ctrlc_handler(shutdown, child_pid)
}

fn install_supervisor_ctrlc_handler(
    shutdown: &Arc<AtomicBool>,
    child_pid: &Arc<AtomicU32>,
) -> Result<()> {
    let shutdown = shutdown.clone();
    let child_pid = child_pid.clone();
    ctrlc::set_handler(move || {
        shutdown.store(true, Ordering::SeqCst);
        let pid = child_pid.load(Ordering::SeqCst);
        if pid != 0 {
            let _ = Command::new("kill")
                .arg("-INT")
                .arg(pid.to_string())
                .status();
        }
    })
    .context("install ctrlc handler")?;
    Ok(())
}

fn start_supervisor_child(
    current: &BinaryCandidate,
    filtered_args: &[String],
    child_pid: &Arc<AtomicU32>,
) -> Result<(Child, Option<BinaryCandidate>, SystemTime)> {
    let child = spawn_child(current, filtered_args)?;
    child_pid.store(child.id(), Ordering::SeqCst);
    eprintln!(
        "[canon-mini-supervisor] started pid={} ({:?})",
        child.id(),
        current.kind
    );
    Ok((child, None, SystemTime::now()))
}

fn should_restart_for_pending_update(
    no_watch: bool,
    root: &Path,
    current: &BinaryCandidate,
    pending_update: &mut Option<BinaryCandidate>,
    orchestrator_mode_flag: &Path,
    idle_marker: &Path,
    child_started_at: SystemTime,
    prefer_release: bool,
    child: &mut Child,
) -> Result<bool> {
    if no_watch {
        return Ok(false);
    }
    record_pending_update(root, current, pending_update)?;
    maybe_restart_for_pending_update(
        root,
        pending_update.as_ref(),
        orchestrator_mode_flag,
        idle_marker,
        child_started_at,
        prefer_release,
        child,
    )
}

fn record_pending_update(
    root: &Path,
    current: &BinaryCandidate,
    pending_update: &mut Option<BinaryCandidate>,
) -> Result<()> {
    if let Some(updated) = has_updated(root, current)? {
        let should_record = pending_update
            .as_ref()
            .map(|prev| prev.mtime < updated.mtime)
            .unwrap_or(true);
        if should_record {
            eprintln!(
                "[canon-mini-supervisor] binary updated; deferring restart until idle from {}",
                updated.path.display()
            );
            log_error_event(
                "supervisor",
                "supervisor_main",
                None,
                &format!(
                    "binary updated; deferring restart until idle from {}",
                    updated.path.display()
                ),
                None,
            );
            *pending_update = Some(updated);
        }
    }
    Ok(())
}

fn maybe_restart_for_pending_update(
    root: &Path,
    pending_update: Option<&BinaryCandidate>,
    orchestrator_mode_flag: &Path,
    idle_marker: &Path,
    child_started_at: SystemTime,
    prefer_release: bool,
    child: &mut Child,
) -> Result<bool> {
    let Some(updated) = pending_update else {
        return Ok(false);
    };
    let mode = read_orchestrator_mode(orchestrator_mode_flag);
    if mode.as_deref() != Some("orchestrate") {
        eprintln!(
            "[canon-mini-supervisor] binary updated in single-role; restarting from {}",
            updated.path.display()
        );
        log_error_event(
            "supervisor",
            "supervisor_main",
            None,
            &format!(
                "binary updated in single-role; restarting from {}",
                updated.path.display()
            ),
            None,
        );
        stage_commit_push_before_restart(root, "single-role-update", prefer_release);
        send_sigint(child);
        wait_for_exit(child, Duration::from_secs(10));
        eprintln!("[canon-mini-supervisor] restarting...");
        return Ok(true);
    }

    let idle_marker_is_fresh = file_mtime_if_exists(idle_marker)
        .map(|mtime| mtime >= child_started_at && mtime >= updated.mtime)
        .unwrap_or(false);
    if idle_marker_is_fresh {
        eprintln!(
            "[canon-mini-supervisor] idle marker observed; restarting from {}",
            updated.path.display()
        );
        log_error_event(
            "supervisor",
            "supervisor_main",
            None,
            &format!(
                "idle marker observed; restarting from {}",
                updated.path.display()
            ),
            None,
        );
        stage_commit_push_before_restart(root, "orchestrate-idle-update", prefer_release);
        send_sigint(child);
        wait_for_exit(child, Duration::from_secs(10));
        eprintln!("[canon-mini-supervisor] restarting...");
        return Ok(true);
    }

    if idle_marker.exists() {
        eprintln!(
            "[canon-mini-supervisor] ignoring stale idle marker while update is pending from {}",
            updated.path.display()
        );
        log_error_event(
            "supervisor",
            "supervisor_main",
            None,
            &format!(
                "ignoring stale idle marker while update is pending from {}",
                updated.path.display()
            ),
            None,
        );
    }
    Ok(false)
}

struct SupervisorArgs {
    exe: String,
    prefer_release: bool,
    no_watch: bool,
    filtered_args: Vec<String>,
}

fn parse_supervisor_args() -> SupervisorArgs {
    let mut args: Vec<String> = std::env::args().collect();
    let exe = args.remove(0);
    let mut prefer_release = false;
    let mut no_watch = false;
    let mut filtered_args = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--release" {
            prefer_release = true;
            i += 1;
            continue;
        }
        if arg == "--no-watch" {
            no_watch = true;
            i += 1;
            continue;
        }
        if arg == "--" {
            filtered_args.extend_from_slice(&args[i + 1..]);
            break;
        }
        filtered_args.push(arg.clone());
        i += 1;
    }
    SupervisorArgs {
        exe,
        prefer_release,
        no_watch,
        filtered_args,
    }
}

fn emit_complexity_report_status(report_workspace: &Path) {
    match write_complexity_report(report_workspace) {
        Ok(Some(path)) => {
            eprintln!(
                "[canon-mini-supervisor] complexity_report: {}",
                path.display()
            );
        }
        Ok(None) => {}
        Err(err) => {
            eprintln!(
                "[canon-mini-supervisor] complexity_report failed: {err:#}"
            );
            log_error_event(
                "supervisor",
                "complexity_report",
                None,
                &format!("complexity_report failed: {err:#}"),
                None,
            );
        }
    }
}
