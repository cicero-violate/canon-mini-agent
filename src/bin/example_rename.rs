use anyhow::{Context, Result};
use std::process::{Command, Stdio};

struct ExampleRenameSession {
    head: String,
    had_local_changes: bool,
}

fn run(cmd: &mut Command) -> Result<(i32, String)> {
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn {:?}", cmd))?;
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let code = output.status.code().unwrap_or(1);
    Ok((code, text))
}

fn git(workspace: &std::path::Path, args: &[&str]) -> Result<(i32, String)> {
    run(Command::new("git").current_dir(workspace).args(args))
}

fn cargo(workspace: &std::path::Path, args: &[&str]) -> Result<(i32, String)> {
    run(Command::new("cargo").current_dir(workspace).args(args))
}

fn prepare_workspace(workspace: &std::path::Path) -> Result<ExampleRenameSession> {
    let (code, out) = git(workspace, &["rev-parse", "--is-inside-work-tree"])?;
    if code != 0 || !out.trim().ends_with("true") {
        anyhow::bail!("expected to run inside a git worktree; got:\n{out}");
    }

    let (code, head_out) = git(workspace, &["rev-parse", "HEAD"])?;
    if code != 0 {
        anyhow::bail!("could not read git HEAD:\n{head_out}");
    }
    let head = head_out.trim().to_string();

    let (code, branch_out) = git(workspace, &["branch", "-f", "example_rename_checkpoint", &head])?;
    if code != 0 {
        anyhow::bail!("failed to create checkpoint branch:\n{branch_out}");
    }

    let (code, status_out) = git(workspace, &["status", "--porcelain"])?;
    if code != 0 {
        anyhow::bail!("failed to check git status:\n{status_out}");
    }
    let had_local_changes = !status_out.trim().is_empty();
    if had_local_changes {
        let (code, stash_out) = git(
            workspace,
            &[
                "stash",
                "push",
                "-u",
                "-m",
                "example_rename: pre-existing working tree changes",
            ],
        )?;
        if code != 0 {
            anyhow::bail!("failed to stash local changes:\n{stash_out}");
        }
    }

    Ok(ExampleRenameSession {
        head,
        had_local_changes,
    })
}

fn rollback_failed_check(
    workspace: &std::path::Path,
    session: &ExampleRenameSession,
    check_out: &str,
) -> Result<()> {
    eprintln!(
        "cargo check failed; rolling back to {} (example_rename_checkpoint)\n{check_out}",
        session.head
    );
    let _ = git(workspace, &["reset", "--hard", &session.head]);
    let _ = git(workspace, &["clean", "-fd"]);
    if session.had_local_changes {
        let _ = git(workspace, &["stash", "pop"]);
    }
    anyhow::bail!("cargo check failed; rollback applied");
}

fn restore_workspace_after_success(
    workspace: &std::path::Path,
    session: &ExampleRenameSession,
    touched_files: &[std::path::PathBuf],
) -> Result<()> {
    let mut restore_args = vec!["checkout", session.head.as_str(), "--"];
    let touched_strs: Vec<String> = touched_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    for s in &touched_strs {
        restore_args.push(s.as_str());
    }
    let (code, restore_out) = git(workspace, &restore_args)?;
    if code != 0 {
        eprintln!("note: git restore after successful check failed (non-fatal):\n{restore_out}");
    }

    if session.had_local_changes {
        let (code, pop_out) = git(workspace, &["stash", "pop"])?;
        if code != 0 {
            eprintln!(
                "note: stash pop reported conflicts or errors; resolve manually:\n{pop_out}"
            );
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    let workspace = std::env::current_dir().context("get current dir")?;
    let session = prepare_workspace(&workspace)?;

    let idx = canon_mini_agent::SemanticIndex::load(&workspace, "canon_mini_agent").context(
        "load semantic index (expected state/rustc/canon_mini_agent/graph.json). Run `cargo build` or `cargo test` first if missing.",
    )?;

    let old_symbol = "rename_example_target::example_target_fn";
    let new_symbol = "rename_example_target::example_target_fn_renamed";

    let report = canon_mini_agent::rename_semantic::rename_symbols_via_semantic_spans(
        &workspace,
        &idx,
        &[(old_symbol.to_string(), new_symbol.to_string())],
    )
    .with_context(|| format!("rename {old_symbol} -> {new_symbol}"))?;

    println!(
        "rename ok: replacements={} touched_files={}",
        report.replacements,
        report.touched_files.len()
    );
    for path in &report.touched_files {
        println!("  {}", path.display());
    }

    let (code, check_out) = cargo(&workspace, &["check", "--workspace"])?;
    if code != 0 {
        rollback_failed_check(&workspace, &session, &check_out)?;
    }

    // Cargo check passed — restore the renamed files to the pre-rename state so
    // the integration test is idempotent and leaves the tree clean.
    restore_workspace_after_success(&workspace, &session, &report.touched_files)?;

    println!(
        "cargo check ok; files restored; checkpoint saved at branch `example_rename_checkpoint` ({})",
        session.head
    );
    Ok(())
}
