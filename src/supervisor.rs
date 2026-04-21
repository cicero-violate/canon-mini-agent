use crate::complexity::write_complexity_report;
use crate::events::EffectEvent;
use crate::logging::{
    artifact_write_signature, init_log_paths, log_error_event, record_effect_for_workspace,
    write_projection_with_artifact_effects,
};
use crate::SemanticIndex;
use crate::{load_issues_file, load_violations_report, set_agent_state_dir, set_workspace};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc, OnceLock,
};
use std::thread;
use std::time::{Duration, SystemTime};

const STARTUP_UPDATE_GRACE_SECS: u64 = 15;
static COMPLEXITY_REPORT_RUNNING: OnceLock<AtomicBool> = OnceLock::new();

fn complexity_report_running_flag() -> &'static AtomicBool {
    COMPLEXITY_REPORT_RUNNING.get_or_init(|| AtomicBool::new(false))
}

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
    let mtime = meta
        .modified()
        .with_context(|| format!("mtime: {}", path.display()))?;
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
    candidates
        .sort_by(|(pref_a, a), (pref_b, b)| pref_b.cmp(pref_a).then_with(|| b.mtime.cmp(&a.mtime)));
    Ok(candidates[0].1.clone())
}

fn spawn_child(bin: &BinaryCandidate, args: &[String]) -> Result<Child> {
    let mut cmd = Command::new(&bin.path);
    cmd.args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.path.display()))?;
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

fn agent_state_flag_path(args: &[String], filename: &str) -> PathBuf {
    agent_state_dir_from_args(args).join(filename)
}

fn rust_patch_verification_flag_path(state_dir: &Path) -> PathBuf {
    state_dir.join("rust_patch_verification_requested.flag")
}

fn rust_patch_verification_requested(state_dir: &Path) -> bool {
    rust_patch_verification_flag_path(state_dir).exists()
}

fn clear_rust_patch_verification_request(state_dir: &Path) {
    let _ = fs::remove_file(rust_patch_verification_flag_path(state_dir));
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
    agent_state_flag_path(args, "orchestrator_cycle_idle.flag")
}

fn orchestrator_mode_flag_path(args: &[String]) -> PathBuf {
    agent_state_flag_path(args, "orchestrator_mode.flag")
}

fn read_orchestrator_mode(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
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
    let status = Command::new(&bin).args(args).current_dir(root).status();
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

fn stage_commit_push_before_restart(
    root: &Path,
    state_dir: &Path,
    reason: &str,
    prefer_release: bool,
) {
    eprintln!("[canon-mini-supervisor] pre-restart checkpoint start ({reason})");
    if !checkpoint_build_succeeded(root, state_dir, reason) {
        return;
    }

    // IMPORTANT: `cargo build` (with rustc wrapper) generates the latest state/rustc/*/graph.json.
    // Refresh the top auto-generated refactor tickets *after* the build, before staging/committing.
    // Use the same build kind preference as the watched binary.
    run_ticket_refresh(root, preferred_build_kind(prefer_release));

    if !stage_git_checkpoint(root, reason) {
        return;
    }

    commit_and_push_checkpoint(root, reason);
}

fn preferred_build_kind(prefer_release: bool) -> BuildKind {
    if prefer_release {
        BuildKind::Release
    } else {
        BuildKind::Debug
    }
}

fn checkpoint_build_succeeded(root: &Path, state_dir: &Path, reason: &str) -> bool {
    if !rust_patch_verification_requested(state_dir) {
        eprintln!(
            "[canon-mini-supervisor] pre-restart: skipping `cargo build --workspace` ({reason}); no Rust apply_patch verification requested"
        );
        return true;
    }
    eprintln!("[canon-mini-supervisor] pre-restart: running `cargo build --workspace` ({reason})");
    match run_cmd(root, "cargo", &["build", "--workspace"]) {
        Ok(true) => {
            eprintln!("[canon-mini-supervisor] pre-restart: cargo build passed ({reason})");
            if let Err(err) = export_semantic_maps_jsonl(root) {
                eprintln!(
                    "[canon-mini-supervisor] semantic_map jsonl export failed ({reason}): {err:#}"
                );
            }
            true
        }
        Ok(false) => {
            eprintln!(
                "[canon-mini-supervisor] pre-restart cargo build failed; skipping git add/commit/push ({reason})"
            );
            clear_rust_patch_verification_request(state_dir);
            false
        }
        Err(err) => {
            eprintln!(
                "[canon-mini-supervisor] pre-restart cargo build errored; skipping git add/commit/push ({reason}): {err:#}"
            );
            clear_rust_patch_verification_request(state_dir);
            false
        }
    }
}

fn export_semantic_maps_jsonl(root: &Path) -> Result<()> {
    let crates = SemanticIndex::available_crates(root);
    if crates.is_empty() {
        eprintln!(
            "[canon-mini-supervisor] semantic_map jsonl export skipped (no crates in state/rustc/index.json)"
        );
        return Ok(());
    }

    let out_dir = root.join("state").join("reports").join("semantic_map");
    fs::create_dir_all(&out_dir).with_context(|| format!("create dir {}", out_dir.display()))?;

    for crate_name in crates {
        let idx = SemanticIndex::load(root, &crate_name)
            .with_context(|| format!("load semantic index for {crate_name}"))?;
        let mut triples = idx.semantic_triples(None);
        triples.sort_by(|a, b| {
            a.from
                .cmp(&b.from)
                .then(a.relation.cmp(&b.relation))
                .then(a.to.cmp(&b.to))
        });

        let out_path = out_dir.join(format!("{crate_name}.jsonl"));
        let mut file = fs::File::create(&out_path)
            .with_context(|| format!("create {}", out_path.display()))?;
        for triple in triples {
            serde_json::to_writer(&mut file, &triple)
                .with_context(|| format!("write {}", out_path.display()))?;
            file.write_all(b"\n")
                .with_context(|| format!("newline {}", out_path.display()))?;
        }
        eprintln!(
            "[canon-mini-supervisor] semantic_map jsonl: {}",
            out_path.display()
        );
    }
    Ok(())
}

fn newest_file_mtime(root: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            newest = match newest {
                Some(cur) if cur >= modified => Some(cur),
                _ => Some(modified),
            };
        }
    }
    newest
}

fn newest_graph_json_mtime(rustc_root: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut stack = vec![rustc_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) != Some("graph.json") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            newest = match newest {
                Some(cur) if cur >= modified => Some(cur),
                _ => Some(modified),
            };
        }
    }
    newest
}

fn semantic_graph_is_stale(workspace: &Path) -> bool {
    let src_dir = workspace.join("src");
    let rustc_dir = workspace.join("state").join("rustc");
    let Some(src_newest) = newest_file_mtime(&src_dir) else {
        return false;
    };
    // Only graph artifacts represent semantic freshness. Metadata files under
    // state/rustc (for example index.json) can be newer and must not mask stale
    // graph.json captures.
    let Some(graph_newest) = newest_graph_json_mtime(&rustc_dir) else {
        return true;
    };
    src_newest > graph_newest
}

fn refresh_semantic_graph_if_stale(root: &Path, workspace: &Path) {
    if !semantic_graph_is_stale(workspace) {
        return;
    }
    eprintln!(
        "[canon-mini-supervisor] semantic graph stale: src/ is newer than state/rustc; refreshing via cargo build --workspace"
    );
    let build_root = if workspace.join("Cargo.toml").exists() {
        workspace
    } else {
        root
    };
    match run_cmd(build_root, "cargo", &["build", "--workspace"]) {
        Ok(true) => {
            eprintln!("[canon-mini-supervisor] semantic graph refresh build passed");
            if let Err(err) = export_semantic_maps_jsonl(workspace) {
                eprintln!(
                    "[canon-mini-supervisor] semantic_map jsonl export failed after graph refresh: {err:#}"
                );
            }
        }
        Ok(false) => {
            eprintln!(
                "[canon-mini-supervisor] semantic graph refresh build failed; continuing with existing graph"
            );
        }
        Err(err) => {
            eprintln!(
                "[canon-mini-supervisor] semantic graph refresh build errored; continuing with existing graph: {err:#}"
            );
        }
    }
}

fn stage_git_checkpoint(root: &Path, reason: &str) -> bool {
    eprintln!("[canon-mini-supervisor] pre-restart: running `git add -A` ({reason})");
    if let Err(err) = run_cmd(root, "git", &["add", "-A"]) {
        eprintln!("[canon-mini-supervisor] git add failed ({reason}): {err:#}");
        return false;
    }
    eprintln!("[canon-mini-supervisor] pre-restart: git add completed ({reason})");

    let has_changes = match run_cmd(root, "git", &["diff", "--cached", "--quiet"]) {
        Ok(true) => false,
        Ok(false) => true,
        Err(err) => {
            eprintln!("[canon-mini-supervisor] git diff --cached failed ({reason}): {err:#}");
            return false;
        }
    };
    if !has_changes {
        eprintln!(
            "[canon-mini-supervisor] no staged changes after successful build; skipping commit/push ({reason})"
        );
        return false;
    }

    true
}

fn commit_and_push_checkpoint(root: &Path, reason: &str) {
    let commit_msg = format!("supervisor pre-restart checkpoint ({reason})");
    eprintln!(
        "[canon-mini-supervisor] pre-restart: running `git commit -m \"{}\"` ({reason})",
        commit_msg
    );
    match run_cmd(root, "git", &["commit", "-m", &commit_msg]) {
        Ok(true) => {
            eprintln!("[canon-mini-supervisor] pre-restart: git commit completed ({reason})");
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

    eprintln!("[canon-mini-supervisor] pre-restart: running `git push` ({reason})");
    match run_cmd(root, "git", &["push"]) {
        Ok(true) => {
            eprintln!("[canon-mini-supervisor] pre-restart: git push completed ({reason})");
            eprintln!("[canon-mini-supervisor] pre-restart checkpoint done ({reason})");
        }
        Ok(false) => {
            eprintln!("[canon-mini-supervisor] git push returned non-zero ({reason})");
        }
        Err(err) => {
            eprintln!("[canon-mini-supervisor] git push failed ({reason}): {err:#}");
        }
    }
}

pub fn run() -> Result<()> {
    let supervisor_args = parse_supervisor_args();
    if maybe_handle_user_chat_mode(&supervisor_args)? {
        return Ok(());
    }
    let SupervisorArgs {
        exe,
        prefer_release,
        no_watch,
        loop_max,
        filtered_args,
        ..
    } = supervisor_args;
    let start_dir = std::env::current_dir().context("current_dir")?;
    let root = find_workspace_root(&start_dir)
        .ok_or_else(|| anyhow!("unable to locate workspace root with target/"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let child_pid = Arc::new(AtomicU32::new(0));
    initialize_supervisor_runtime(&filtered_args, &shutdown, &child_pid)?;

    // Bounded repair loop mode: run up to N agent iterations then stop.
    if let Some(max_iterations) = loop_max {
        let workspace = workspace_from_args(&filtered_args)
            .map(PathBuf::from)
            .unwrap_or_else(|| root.clone());
        return run_repair_loop(
            &root,
            &workspace,
            max_iterations,
            &filtered_args,
            prefer_release,
            &shutdown,
            &child_pid,
        );
    }

    let idle_marker = cycle_idle_marker_path(&filtered_args);
    let orchestrator_mode_flag = orchestrator_mode_flag_path(&filtered_args);

    run_supervisor_loop(
        &exe,
        &root,
        &filtered_args,
        shutdown.as_ref(),
        &child_pid,
        no_watch,
        &orchestrator_mode_flag,
        &idle_marker,
        prefer_release,
    )
}

fn run_supervisor_loop(
    exe: &str,
    root: &Path,
    filtered_args: &[String],
    shutdown: &AtomicBool,
    child_pid: &Arc<AtomicU32>,
    no_watch: bool,
    orchestrator_mode_flag: &Path,
    idle_marker: &Path,
    prefer_release: bool,
) -> Result<()> {
    loop {
        let current = newest_candidate(root, prefer_release)?;
        emit_iteration_status_and_report(exe, root, &current, filtered_args);
        let (mut child, mut pending_update, child_started_at) =
            start_supervisor_child(&current, filtered_args, child_pid)?;

        if supervise_current_child(
            shutdown,
            no_watch,
            root,
            &current,
            &mut pending_update,
            orchestrator_mode_flag,
            idle_marker,
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
    refresh_semantic_graph_if_stale(root, &report_workspace);
    trigger_complexity_report_status_async(report_workspace);
}

fn trigger_complexity_report_status_async(report_workspace: PathBuf) {
    let running = complexity_report_running_flag();
    if running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        eprintln!("[canon-mini-supervisor] complexity_report: already running (skip trigger)");
        return;
    }
    thread::spawn(move || {
        emit_complexity_report_status(&report_workspace);
        complexity_report_running_flag().store(false, Ordering::Release);
    });
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
    let mut pending_update_defer_checks: u32 = 0;
    loop {
        thread::sleep(Duration::from_millis(1000));
        match supervise_current_child_iteration(
            shutdown,
            no_watch,
            root,
            current,
            pending_update,
            &mut pending_update_defer_checks,
            orchestrator_mode_flag,
            idle_marker,
            child_started_at,
            prefer_release,
            child,
        )? {
            SuperviseCurrentChildFlow::Continue => {}
            SuperviseCurrentChildFlow::BreakLoop => break,
            SuperviseCurrentChildFlow::ReturnOkTrue => return Ok(true),
        }
    }
    Ok(false)
}

enum SuperviseCurrentChildFlow {
    Continue,
    BreakLoop,
    ReturnOkTrue,
}

fn supervise_current_child_iteration(
    shutdown: &AtomicBool,
    no_watch: bool,
    root: &Path,
    current: &BinaryCandidate,
    pending_update: &mut Option<BinaryCandidate>,
    pending_update_defer_checks: &mut u32,
    orchestrator_mode_flag: &Path,
    idle_marker: &Path,
    child_started_at: SystemTime,
    prefer_release: bool,
    child: &mut Child,
) -> Result<SuperviseCurrentChildFlow> {
    if handle_shutdown_request(shutdown, child) {
        return Ok(SuperviseCurrentChildFlow::ReturnOkTrue);
    }
    if handle_child_exit_status(child.try_wait().context("wait child")?, root, prefer_release) {
        return Ok(SuperviseCurrentChildFlow::BreakLoop);
    }
    if should_restart_for_pending_update(
        no_watch,
        root,
        current,
        pending_update,
        pending_update_defer_checks,
        orchestrator_mode_flag,
        idle_marker,
        child_started_at,
        prefer_release,
        child,
    )? {
        return Ok(SuperviseCurrentChildFlow::BreakLoop);
    }
    Ok(SuperviseCurrentChildFlow::Continue)
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

fn handle_child_exit_status(status: Option<ExitStatus>, root: &Path, prefer_release: bool) -> bool {
    let Some(status) = status else {
        return false;
    };
    eprintln!("[canon-mini-supervisor] child exited: {status}");
    if status.success() {
        eprintln!("[canon-mini-supervisor] child exited cleanly; not restarting");
        std::process::exit(0);
    }
    eprintln!("[canon-mini-supervisor] restarting due to failure...");
    let state_dir = root.join("agent_state");
    stage_commit_push_before_restart(root, &state_dir, "failure-restart", prefer_release);
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
    set_agent_state_dir(
        agent_state_dir_from_args(filtered_args)
            .to_string_lossy()
            .to_string(),
    );
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
    pending_update_defer_checks: &mut u32,
    orchestrator_mode_flag: &Path,
    idle_marker: &Path,
    child_started_at: SystemTime,
    prefer_release: bool,
    child: &mut Child,
) -> Result<bool> {
    if no_watch {
        return Ok(false);
    }
    record_pending_update(root, current, pending_update, child_started_at)?;
    maybe_restart_for_pending_update(
        root,
        pending_update.as_ref(),
        pending_update_defer_checks,
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
    child_started_at: SystemTime,
) -> Result<()> {
    if let Some(updated) = has_updated(root, current)? {
        let within_startup_grace = updated.path == current.path
            && child_started_at.elapsed().unwrap_or_default()
                < Duration::from_secs(STARTUP_UPDATE_GRACE_SECS);
        if within_startup_grace {
            return Ok(());
        }
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
    pending_update_defer_checks: &mut u32,
    orchestrator_mode_flag: &Path,
    idle_marker: &Path,
    child_started_at: SystemTime,
    prefer_release: bool,
    child: &mut Child,
) -> Result<bool> {
    let Some(updated) = pending_update else {
        *pending_update_defer_checks = 0;
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
        let state_dir = root.join("agent_state");
        stage_commit_push_before_restart(root, &state_dir, "single-role-update", prefer_release);
        send_sigint(child);
        wait_for_exit(child, Duration::from_secs(10));
        eprintln!("[canon-mini-supervisor] restarting...");
        return Ok(true);
    }

    let idle_marker_is_fresh = file_mtime_if_exists(idle_marker)
        .map(|mtime| mtime >= child_started_at && mtime >= updated.mtime)
        .unwrap_or(false);
    if idle_marker_is_fresh {
        *pending_update_defer_checks = 0;
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
        let state_dir = root.join("agent_state");
        stage_commit_push_before_restart(
            root,
            &state_dir,
            "orchestrate-idle-update",
            prefer_release,
        );
        send_sigint(child);
        wait_for_exit(child, Duration::from_secs(10));
        eprintln!("[canon-mini-supervisor] restarting...");
        return Ok(true);
    }

    *pending_update_defer_checks = pending_update_defer_checks.saturating_add(1);
    if *pending_update_defer_checks >= 10 {
        eprintln!(
            "[canon-mini-supervisor] forcing restart after {} deferred checks from {}",
            pending_update_defer_checks,
            updated.path.display()
        );
        log_error_event(
            "supervisor",
            "supervisor_main",
            None,
            &format!(
                "forcing restart after {} deferred checks from {}",
                pending_update_defer_checks,
                updated.path.display()
            ),
            None,
        );
        let state_dir = root.join("agent_state");
        stage_commit_push_before_restart(
            root,
            &state_dir,
            "orchestrate-deferred-update-timeout",
            prefer_release,
        );
        send_sigint(child);
        wait_for_exit(child, Duration::from_secs(10));
        eprintln!("[canon-mini-supervisor] restarting...");
        *pending_update_defer_checks = 0;
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
    /// When Some(n), run the bounded repair loop for up to n iterations instead
    /// of the normal indefinite-watch supervisor mode.
    loop_max: Option<u32>,
    user_message: Option<String>,
    user_message_file: Option<String>,
    read_user_reply: bool,
    user_to_role: String,
    filtered_args: Vec<String>,
}

fn take_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

fn sanitize_role(role: &str) -> String {
    role.trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
}

fn read_user_message_cli(args: &SupervisorArgs) -> Result<Option<String>> {
    if let Some(message) = &args.user_message {
        let trimmed = message.trim().to_string();
        if trimmed.is_empty() {
            bail!("--message cannot be empty");
        }
        return Ok(Some(trimmed));
    }
    if let Some(path) = &args.user_message_file {
        let text =
            fs::read_to_string(path).with_context(|| format!("read --message-file {}", path))?;
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            bail!("--message-file contained only whitespace");
        }
        return Ok(Some(trimmed));
    }
    Ok(None)
}

fn write_external_user_message(
    workspace: &Path,
    state_dir: &Path,
    to_role: &str,
    message: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create state dir {}", state_dir.display()))?;
    let to_key = sanitize_role(to_role);
    let action_text = external_user_message_text(&to_key, message)?;
    let signature = artifact_write_signature(&[
        "external_user_message",
        &to_key,
        &action_text.len().to_string(),
        action_text.as_str(),
    ]);
    record_effect_for_workspace(
        workspace,
        EffectEvent::ExternalUserMessageRecorded {
            to_role: to_key.clone(),
            message: action_text.clone(),
            signature,
        },
    )?;
    let msg_path = state_dir.join(format!("external_user_message_to_{}.json", to_key));
    write_projection_with_artifact_effects(
        workspace,
        &msg_path,
        &format!("agent_state/external_user_message_to_{}.json", to_key),
        "write",
        "external_user_message_projection",
        &action_text,
    )?;
    let wake_path = state_dir.join(format!("wakeup_{}.flag", to_key));
    write_projection_with_artifact_effects(
        workspace,
        &wake_path,
        &format!("agent_state/wakeup_{}.flag", to_key),
        "write",
        "external_user_message_wakeup",
        "user_message",
    )?;
    Ok(msg_path)
}

fn external_user_message_text(to_key: &str, message: &str) -> Result<String> {
    serde_json::to_string_pretty(&external_user_message_payload(to_key, message)).map_err(Into::into)
}

fn external_user_message_payload(to_key: &str, message: &str) -> serde_json::Value {
    json!({
        "kind": "external_user_message",
        "from": "user",
        "to": to_key,
        "message": message,
        "reply_to": "user"
    })
}

fn read_external_user_reply(state_dir: &Path) -> Result<Option<String>> {
    let reply_path = state_dir.join("last_message_to_user.json");
    match fs::read_to_string(&reply_path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", reply_path.display())),
    }
}

fn maybe_handle_user_chat_mode(args: &SupervisorArgs) -> Result<bool> {
    let message = read_user_message_cli(args)?;
    if !args.read_user_reply && message.is_none() {
        return Ok(false);
    }

    let workspace =
        PathBuf::from(workspace_from_args(&args.filtered_args).context("missing --workspace")?);
    let state_dir = agent_state_dir_from_args(&args.filtered_args);
    if args.read_user_reply {
        match read_external_user_reply(&state_dir)? {
            Some(reply) => println!("{}", reply),
            None => println!("{{}}"),
        }
        return Ok(true);
    }

    let msg_path = write_external_user_message(
        &workspace,
        &state_dir,
        &args.user_to_role,
        &message.expect("checked above"),
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "delivered_to": sanitize_role(&args.user_to_role),
            "message_path": msg_path,
            "wakeup_flag": state_dir.join(format!("wakeup_{}.flag", sanitize_role(&args.user_to_role))),
        }))?
    );
    Ok(true)
}

fn parse_supervisor_args() -> SupervisorArgs {
    let mut args: Vec<String> = std::env::args().collect();
    let exe = args.remove(0);
    let mut prefer_release = false;
    let mut no_watch = false;
    let mut loop_max: Option<u32> = None;
    let user_message = take_flag_value(&args, "--message");
    let user_message_file = take_flag_value(&args, "--message-file");
    let read_user_reply = has_flag(&args, "--read-reply");
    let user_to_role = take_flag_value(&args, "--to").unwrap_or_else(|| "solo".to_string());
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
        if arg == "--loop" {
            if i + 1 < args.len() {
                loop_max = args[i + 1].parse::<u32>().ok();
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if matches!(arg.as_str(), "--message" | "--message-file" | "--to") {
            i += if i + 1 < args.len() { 2 } else { 1 };
            continue;
        }
        if arg == "--read-reply" {
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
        loop_max,
        user_message,
        user_message_file,
        read_user_reply,
        user_to_role,
        filtered_args,
    }
}

// ---------------------------------------------------------------------------
// Gap 3: Bounded iterative repair loop
// ---------------------------------------------------------------------------

/// Write `agent_state/loop_context.json` so the agent's solo prompt knows which
/// symbol to focus on in this iteration.
fn write_loop_context(
    state_dir: &Path,
    target: &crate::SemanticIndex,
    workspace: &Path,
    violation_symbols: &[String],
    iteration: u32,
    max_iterations: u32,
    tests_passing: bool,
) {
    let vs: Vec<&str> = violation_symbols.iter().map(|s| s.as_str()).collect();
    let top = target.top_repair_targets(&vs, 1);
    let ctx = if let Some(t) = top.first() {
        serde_json::json!({
            "iteration": iteration,
            "max_iterations": max_iterations,
            "tests_passing": tests_passing,
            "target_symbol": t.symbol,
            "target_file": t.file.as_deref().unwrap_or(""),
            "target_line": t.line.unwrap_or(0),
            "score": t.score,
            "patch_kind": t.patch_kind.as_ref().map(|k| format!("{k:?}")).unwrap_or_else(|| "General".into()),
            "reasons": t.reasons,
        })
    } else {
        serde_json::json!({
            "iteration": iteration,
            "max_iterations": max_iterations,
            "tests_passing": tests_passing,
            "note": "no repair targets found in semantic index",
        })
    };
    let out = state_dir.join("loop_context.json");
    if let Ok(body) = serde_json::to_string_pretty(&ctx) {
        let _ = std::fs::write(&out, body);
        eprintln!(
            "[canon-mini-supervisor] loop_context written: {}",
            out.display()
        );
    }
    let _ = workspace; // workspace available for future multi-crate expansion
}

/// Run `cargo test --workspace` only when a Rust apply_patch requested verification.
fn check_test_gate(root: &Path, state_dir: &Path, prior_value: bool) -> bool {
    if !rust_patch_verification_requested(state_dir) {
        eprintln!(
            "[canon-mini-supervisor] loop: skipping cargo test gate; no Rust apply_patch verification requested"
        );
        return prior_value;
    }
    eprintln!("[canon-mini-supervisor] loop: running cargo test --workspace");
    let result = match run_cmd(root, "cargo", &["test", "--workspace"]) {
        Ok(true) => {
            eprintln!("[canon-mini-supervisor] loop: tests PASSING");
            true
        }
        Ok(false) => {
            eprintln!("[canon-mini-supervisor] loop: tests FAILING");
            false
        }
        Err(err) => {
            eprintln!("[canon-mini-supervisor] loop: cargo test errored: {err:#}");
            false
        }
    };
    clear_rust_patch_verification_request(state_dir);
    result
}

/// Extract `symbol::path` tokens from a free-form string.
fn extract_symbol_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|word| {
            let clean = word.trim_matches(|c: char| !c.is_alphanumeric() && c != ':' && c != '_');
            if clean.contains("::") && !clean.contains('/') {
                Some(clean.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Read VIOLATIONS.json and extract symbol paths from violation evidence/location fields.
fn load_violation_symbols(workspace: &Path) -> Vec<String> {
    let mut symbols = Vec::new();
    let report = load_violations_report(workspace);
    for violation in report.violations {
        for file in violation.files {
            if file.contains("::") {
                symbols.push(file);
            }
        }
        for entry in violation.evidence {
            symbols.extend(extract_symbol_tokens(&entry));
        }
    }
    symbols.sort();
    symbols.dedup();
    symbols
}

/// A file + line number extracted from an issue location string.
struct FileLocation {
    file: String,
    line: u32,
}

/// Read ISSUES.json and return:
/// - symbol paths extracted directly from evidence strings (`::` tokens)
/// - file locations from `location` fields for semantic resolution
///
/// Only open issues are considered; resolved issues are skipped.
fn load_issue_failure_signals(workspace: &Path) -> (Vec<String>, Vec<FileLocation>) {
    let file = load_issues_file(workspace);
    let mut symbols: Vec<String> = Vec::new();
    let mut locations: Vec<FileLocation> = Vec::new();

    for issue in file.issues {
        if issue.status == "resolved" {
            continue;
        }

        if !issue.location.trim().is_empty() {
            for part in issue.location.split(';') {
                let part = part.trim();
                if let Some(colon_pos) = part.rfind(':') {
                    let file = part[..colon_pos].trim().to_string();
                    let line_part = part[colon_pos + 1..].trim();
                    let line_str = line_part.split('-').next().unwrap_or("0");
                    if let Ok(line) = line_str.parse::<u32>() {
                        if line > 0 && !file.is_empty() {
                            locations.push(FileLocation { file, line });
                        }
                    }
                }
                symbols.extend(extract_symbol_tokens(part));
            }
        }

        for entry in issue.evidence {
            symbols.extend(extract_symbol_tokens(&entry));
        }

        symbols.extend(extract_symbol_tokens(&issue.description));
    }

    symbols.sort();
    symbols.dedup();
    (symbols, locations)
}

/// Resolve file-location pairs to symbol paths using the semantic index.
fn resolve_file_locations(
    idx: &crate::SemanticIndex,
    workspace: &Path,
    locations: &[FileLocation],
) -> Vec<String> {
    let ws = workspace.to_string_lossy();
    let ws_prefix = if ws.ends_with('/') {
        ws.into_owned()
    } else {
        format!("{}/", ws)
    };
    let mut resolved = Vec::new();
    for loc in locations {
        // Resolve relative paths against the workspace root.
        let abs_file = if loc.file.starts_with('/') {
            loc.file.clone()
        } else {
            format!("{}{}", ws_prefix, loc.file)
        };
        if let Some(symbol) = idx.symbol_at_file_line(&abs_file, loc.line) {
            resolved.push(symbol);
        }
    }
    resolved.sort();
    resolved.dedup();
    resolved
}

/// Load the SemanticIndex for the primary crate in the workspace.
/// Returns the first available crate's index, preferring the one with most nodes.
fn load_primary_semantic_index(workspace: &Path) -> Option<crate::SemanticIndex> {
    let mut crates = crate::SemanticIndex::available_crates(workspace);
    if crates.is_empty() {
        eprintln!("[canon-mini-supervisor] loop: no crates in state/rustc/index.json; skipping semantic scoring");
        return None;
    }
    // Sort by name length descending as a proxy for "most specific" (non-trivial) crate.
    crates.sort_by(|a, b| b.len().cmp(&a.len()));
    for crate_name in &crates {
        match crate::SemanticIndex::load(workspace, crate_name) {
            Ok(idx) => {
                eprintln!("[canon-mini-supervisor] loop: loaded semantic index for {crate_name}");
                return Some(idx);
            }
            Err(err) => {
                eprintln!("[canon-mini-supervisor] loop: could not load {crate_name}: {err:#}");
            }
        }
    }
    None
}

fn collect_failure_signals(
    workspace: &Path,
    maybe_idx: Option<&crate::SemanticIndex>,
) -> Vec<String> {
    let mut failure_symbols = load_violation_symbols(workspace);
    let (issue_symbols, issue_locations) = load_issue_failure_signals(workspace);
    failure_symbols.extend(issue_symbols);
    if let Some(idx) = maybe_idx {
        let resolved = resolve_file_locations(idx, workspace, &issue_locations);
        failure_symbols.extend(resolved);
    }
    failure_symbols.sort();
    failure_symbols.dedup();
    failure_symbols
}

fn run_child_until_exit(
    current: &BinaryCandidate,
    filtered_args: &[String],
    shutdown: &AtomicBool,
    child_pid: &AtomicU32,
) -> Result<bool> {
    let mut child = spawn_child(current, filtered_args)?;
    child_pid.store(child.id(), Ordering::SeqCst);

    loop {
        thread::sleep(Duration::from_millis(1000));
        if shutdown.load(Ordering::SeqCst) {
            send_sigint(&child);
            wait_for_exit(&mut child, Duration::from_secs(10));
            eprintln!("[canon-mini-supervisor] loop: shutdown; stopping");
            child_pid.store(0, Ordering::SeqCst);
            return Ok(true);
        }
        match child.try_wait().context("wait child")? {
            Some(status) => {
                eprintln!("[canon-mini-supervisor] loop: agent exited: {status}");
                child_pid.store(0, Ordering::SeqCst);
                return Ok(false);
            }
            None => continue,
        }
    }
}

/// Bounded repair loop: run the agent for up to `max_iterations` cycles, stopping
/// early when `cargo test --workspace` passes cleanly.
///
/// Each iteration:
///  1. Load the semantic graph and score targets (Gap 2).
///  2. Classify the top target's required patch kind (Gap 1).
///  3. Write `agent_state/loop_context.json` with the target — the agent's solo
///     prompt will pick this up and focus the repair.
///  4. Spawn the agent child and wait for it to exit.
///  5. Check the cargo test gate.  If passing, return Ok.
///  6. Otherwise commit a checkpoint and continue to the next iteration.
fn run_repair_loop(
    root: &Path,
    workspace: &Path,
    max_iterations: u32,
    filtered_args: &[String],
    prefer_release: bool,
    shutdown: &AtomicBool,
    child_pid: &AtomicU32,
) -> Result<()> {
    eprintln!("[canon-mini-supervisor] repair loop starting (max={max_iterations})");

    let state_dir = agent_state_dir_from_args(filtered_args);
    let mut tests_passing = false;

    for iteration in 1..=max_iterations {
        if shutdown.load(Ordering::SeqCst) {
            eprintln!("[canon-mini-supervisor] loop: shutdown requested; stopping");
            break;
        }

        eprintln!("[canon-mini-supervisor] loop: iteration {iteration}/{max_iterations}");

        // Refresh semantic graph (cargo build regenerates state/rustc/*/graph.json).
        if !checkpoint_build_succeeded(root, &state_dir, &format!("loop-iter-{iteration}")) {
            eprintln!("[canon-mini-supervisor] loop: build failed; aborting loop");
            break;
        }

        let maybe_idx = load_primary_semantic_index(workspace);
        let failure_symbols = collect_failure_signals(workspace, maybe_idx.as_ref());
        eprintln!(
            "[canon-mini-supervisor] loop: {} failure signal(s) loaded",
            failure_symbols.len()
        );

        if let Some(ref idx) = maybe_idx {
            write_loop_context(
                &state_dir,
                idx,
                workspace,
                &failure_symbols,
                iteration,
                max_iterations,
                tests_passing,
            );
        }

        // Spawn agent child and wait for it to exit.
        let current = newest_candidate(root, prefer_release)?;
        eprintln!(
            "[canon-mini-supervisor] loop: spawning agent from {}",
            current.path.display()
        );
        if run_child_until_exit(&current, filtered_args, shutdown, child_pid)? {
            return Ok(());
        }

        // Check test gate.
        tests_passing = check_test_gate(root, &state_dir, tests_passing);
        if tests_passing {
            eprintln!(
                "[canon-mini-supervisor] loop: tests passing after iteration {iteration}; done"
            );
            // Clean up loop context so the agent doesn't see stale data on next normal run.
            let _ = std::fs::remove_file(state_dir.join("loop_context.json"));
            stage_commit_push_before_restart(
                root,
                &state_dir,
                &format!("loop-success-{iteration}"),
                prefer_release,
            );
            return Ok(());
        }

        eprintln!("[canon-mini-supervisor] loop: tests still failing after iteration {iteration}");
        stage_commit_push_before_restart(
            root,
            &state_dir,
            &format!("loop-iter-{iteration}"),
            prefer_release,
        );
    }

    // Loop exhausted without passing tests.
    let _ = std::fs::remove_file(state_dir.join("loop_context.json"));
    if tests_passing {
        eprintln!("[canon-mini-supervisor] repair loop completed successfully");
        Ok(())
    } else {
        eprintln!(
            "[canon-mini-supervisor] repair loop exhausted {max_iterations} iterations without passing tests"
        );
        Ok(()) // Return Ok — not a hard error; caller decides how to proceed.
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
            eprintln!("[canon-mini-supervisor] complexity_report failed: {err:#}");
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
