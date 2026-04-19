mod parser;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
pub use parser::{parse_patch, ApplyPatchArgs, Hunk, ParseError, UpdateFileChunk};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{context}: {source}")]
pub struct IoError {
    context: String,
    #[source]
    source: std::io::Error,
}

#[derive(Debug, Error)]
pub enum ApplyPatchError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Io(#[from] IoError),
    #[error("{0}")]
    ComputeReplacements(String),
}

#[derive(Debug, Clone)]
pub struct AffectedPaths {
    pub added: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}

const APPLY_PATCH_BIN: &str = "/usr/local/bin/apply_patch";

/// Apply a patch string to the filesystem. Relative paths are resolved against `cwd`.
pub fn apply_patch(patch: &str, cwd: &Path) -> Result<AffectedPaths, ApplyPatchError> {
    let mut args = parse_patch(patch)?;

    // Resolve relative paths against cwd.
    for hunk in args.hunks.iter_mut() {
        match hunk {
            parser::Hunk::AddFile { path, .. }
            | parser::Hunk::DeleteFile { path }
            | parser::Hunk::UpdateFile { path, .. } => {
                if path.is_relative() {
                    *path = cwd.join(&*path);
                }
            }
        }
        if let parser::Hunk::UpdateFile { move_path, .. } = hunk {
            if let Some(dest) = move_path {
                if dest.is_relative() {
                    *dest = cwd.join(&*dest);
                }
            }
        }
    }

    let affected = affected_paths_from_hunks(&args.hunks)?;
    run_external_apply_patch(patch, cwd)?;
    Ok(affected)
}

fn affected_paths_from_hunks(hunks: &[Hunk]) -> Result<AffectedPaths, ApplyPatchError> {
    if hunks.is_empty() {
        return Err(ApplyPatchError::ComputeReplacements(
            "No files were modified.".into(),
        ));
    }

    let mut added: Vec<PathBuf> = Vec::new();
    let mut modified: Vec<PathBuf> = Vec::new();
    let mut deleted: Vec<PathBuf> = Vec::new();

    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, .. } => {
                added.push(path.clone());
            }
            Hunk::DeleteFile { path } => {
                deleted.push(path.clone());
            }
            Hunk::UpdateFile {
                path, move_path, ..
            } => {
                if let Some(dest) = move_path {
                    modified.push(dest.clone());
                } else {
                    modified.push(path.clone());
                }
            }
        }
    }

    Ok(AffectedPaths {
        added,
        modified,
        deleted,
    })
}

fn run_external_apply_patch(patch: &str, cwd: &Path) -> Result<(), ApplyPatchError> {
    let child = Command::new(APPLY_PATCH_BIN)
        .arg(patch)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| {
            ApplyPatchError::Io(IoError {
                context: format!("failed to spawn {}", APPLY_PATCH_BIN),
                source,
            })
        })?;

    let output = child
        .wait_with_output()
        .with_context(|| format!("wait for {}", APPLY_PATCH_BIN))
        .map_err(to_io)?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("{APPLY_PATCH_BIN} exited with status {}", output.status)
        };
        Err(ApplyPatchError::ComputeReplacements(message))
    }
}

fn to_io(err: anyhow::Error) -> ApplyPatchError {
    match err.downcast::<std::io::Error>() {
        Ok(ioe) => ApplyPatchError::Io(IoError {
            context: "I/O error".to_string(),
            source: ioe,
        }),
        Err(other) => ApplyPatchError::ComputeReplacements(other.to_string()),
    }
}
