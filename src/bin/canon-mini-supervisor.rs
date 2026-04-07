use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

fn wait_for_exit(mut child: Child, timeout: Duration) {
    let start = SystemTime::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed().unwrap_or_default() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(200));
            }
            Err(_) => break,
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

fn main() -> Result<()> {
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
    let start_dir = std::env::current_dir().context("current_dir")?;
    let root = find_workspace_root(&start_dir)
        .ok_or_else(|| anyhow!("unable to locate workspace root with target/"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let child_pid = Arc::new(AtomicU32::new(0));
    {
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
    }

    loop {
        let current = newest_candidate(&root, prefer_release)?;
        eprintln!(
            "[canon-mini-supervisor] exec={} root={} watching={}",
            exe,
            root.display(),
            current.path.display()
        );
        let mut child = spawn_child(&current, &filtered_args)?;
        child_pid.store(child.id(), Ordering::SeqCst);
        eprintln!(
            "[canon-mini-supervisor] started pid={} ({:?})",
            child.id(),
            current.kind
        );

        loop {
            thread::sleep(Duration::from_millis(1000));
            if shutdown.load(Ordering::SeqCst) {
                eprintln!("[canon-mini-supervisor] shutdown requested; waiting for child");
                wait_for_exit(child, Duration::from_secs(10));
                return Ok(());
            }
            if let Some(status) = child.try_wait().context("wait child")? {
                eprintln!("[canon-mini-supervisor] child exited: {status}");
                eprintln!("[canon-mini-supervisor] restarting...");
                break;
            }
            if !no_watch {
                if let Some(updated) = has_updated(&root, &current)? {
                    eprintln!(
                        "[canon-mini-supervisor] binary updated; restarting from {}",
                        updated.path.display()
                    );
                    send_sigint(&child);
                    wait_for_exit(child, Duration::from_secs(10));
                    eprintln!("[canon-mini-supervisor] restarting...");
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(1000));
    }
}
