//! Parser for the *** Begin Patch / *** End Patch format.
use std::path::{Path, PathBuf};

use thiserror::Error;

const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";
const CHANGE_CONTEXT_MARKER: &str = "@@ ";
const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

const PARSE_IN_STRICT_MODE: bool = false;

#[derive(Debug, PartialEq, Error, Clone)]
pub enum ParseError {
    #[error("invalid patch: {0}")]
    InvalidPatchError(String),
    #[error("invalid hunk at line {line_number}, {message}")]
    InvalidHunkError { message: String, line_number: usize },
}
use ParseError::*;

#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::enum_variant_names)]
pub enum Hunk {
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<UpdateFileChunk>,
    },
}

impl Hunk {
    pub fn resolve_path(&self, cwd: &Path) -> PathBuf {
        match self {
            Hunk::AddFile { path, .. } => cwd.join(path),
            Hunk::DeleteFile { path } => cwd.join(path),
            Hunk::UpdateFile { path, .. } => cwd.join(path),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct UpdateFileChunk {
    pub change_context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub is_end_of_file: bool,
}

#[derive(Debug, PartialEq)]
pub struct ApplyPatchArgs {
    pub patch: String,
    pub hunks: Vec<Hunk>,
    pub workdir: Option<String>,
}

pub fn parse_patch(patch: &str) -> Result<ApplyPatchArgs, ParseError> {
    let mode = if PARSE_IN_STRICT_MODE {
        ParseMode::Strict
    } else {
        ParseMode::Lenient
    };
    parse_patch_text(patch, mode)
}

enum ParseMode {
    Strict,
    Lenient,
}

fn parse_patch_text(patch: &str, mode: ParseMode) -> Result<ApplyPatchArgs, ParseError> {
    let lines: Vec<&str> = patch.trim().lines().collect();
    let lines: &[&str] = match check_patch_boundaries_strict(&lines) {
        Ok(()) => &lines,
        Err(e) => match mode {
            ParseMode::Strict => return Err(e),
            ParseMode::Lenient => check_patch_boundaries_lenient(&lines, e)?,
        },
    };

    let mut hunks: Vec<Hunk> = Vec::new();
    let last_line_index = lines.len().saturating_sub(1);
    let mut remaining_lines = &lines[1..last_line_index];
    let mut line_number = 2;
    while !remaining_lines.is_empty() {
        let (hunk, hunk_lines) = parse_one_hunk(remaining_lines, line_number)?;
        hunks.push(hunk);
        line_number += hunk_lines;
        remaining_lines = &remaining_lines[hunk_lines..]
    }
    let patch = lines.join("\n");
    Ok(ApplyPatchArgs {
        hunks,
        patch,
        workdir: None,
    })
}

fn check_patch_boundaries_strict(lines: &[&str]) -> Result<(), ParseError> {
    let (first_line, last_line) = match lines {
        [] => (None, None),
        [first] => (Some(first), Some(first)),
        [first, .., last] => (Some(first), Some(last)),
    };
    check_start_and_end_lines_strict(first_line, last_line)
}

fn check_start_and_end_lines_strict(
    first_line: Option<&&str>,
    last_line: Option<&&str>,
) -> Result<(), ParseError> {
    match (first_line, last_line) {
        (Some(first), Some(last))
            if first.trim() == BEGIN_PATCH_MARKER && last.trim() == END_PATCH_MARKER =>
        {
            Ok(())
        }
        (Some(first), _) if first.trim() != BEGIN_PATCH_MARKER => Err(InvalidPatchError(format!(
            "patch should start with {BEGIN_PATCH_MARKER}"
        ))),
        (_, Some(last)) if last.trim() != END_PATCH_MARKER => Err(InvalidPatchError(format!(
            "patch should end with {END_PATCH_MARKER}"
        ))),
        _ => Err(InvalidPatchError("patch is empty".to_string())),
    }
}

fn check_patch_boundaries_lenient<'a>(
    lines: &'a [&str],
    strict_error: ParseError,
) -> Result<&'a [&'a str], ParseError> {
    if lines.len() < 4 {
        return Err(strict_error);
    }

    let raw_first = lines.first().copied().unwrap_or("");
    let raw_last = lines.last().copied().unwrap_or("");
    let (first_prefix, first_suffix) = raw_first.split_once('\n').unwrap_or((raw_first, ""));
    let (last_prefix, last_suffix) = raw_last.split_once('\n').unwrap_or((raw_last, ""));

    let looks_like_heredoc =
        first_prefix.trim().starts_with("<<") && last_suffix.trim().ends_with("EOF");

    if looks_like_heredoc
        && first_suffix.contains(BEGIN_PATCH_MARKER)
        && last_prefix.contains(END_PATCH_MARKER)
    {
        let first_idx = lines
            .iter()
            .position(|l| l.contains(BEGIN_PATCH_MARKER))
            .unwrap_or(0);
        let last_idx = lines
            .iter()
            .rposition(|l| l.contains(END_PATCH_MARKER))
            .unwrap_or(lines.len() - 1);
        return Ok(&lines[first_idx..=last_idx]);
    }

    Err(strict_error)
}

fn parse_one_hunk(
    lines: &[&str],
    starting_line_number: usize,
) -> Result<(Hunk, usize), ParseError> {
    let Some(first_line) = lines.first() else {
        return Err(InvalidPatchError("missing hunk".to_string()));
    };
    let first_line = first_line.trim();
    let mut consumed = 1;
    let mut current_line_number = starting_line_number;

    if let Some(filename) = first_line.strip_prefix(ADD_FILE_MARKER) {
        let filename = filename.trim();
        if filename.is_empty() {
            return Err(InvalidHunkError {
                message: "missing filename for Add File".to_string(),
                line_number: current_line_number,
            });
        }
        let mut contents: Vec<String> = Vec::new();
        for line in lines[1..].iter() {
            let trimmed = line.trim_end();
            if trimmed == ADD_FILE_MARKER
                || trimmed == DELETE_FILE_MARKER
                || trimmed == UPDATE_FILE_MARKER
                || trimmed == EOF_MARKER
            {
                break;
            }
            if trimmed.starts_with('+') {
                contents.push(trimmed.trim_start_matches('+').to_string());
                consumed += 1;
                current_line_number += 1;
            } else {
                break;
            }
        }
        if contents.is_empty() {
            return Err(InvalidHunkError {
                message: "Add File hunk requires at least one + line".to_string(),
                line_number: current_line_number,
            });
        }
        return Ok((
            Hunk::AddFile {
                path: PathBuf::from(filename),
                contents: contents.join("\n") + "\n",
            },
            consumed,
        ));
    }

    if let Some(filename) = first_line.strip_prefix(DELETE_FILE_MARKER) {
        let filename = filename.trim();
        if filename.is_empty() {
            return Err(InvalidHunkError {
                message: "missing filename for Delete File".to_string(),
                line_number: current_line_number,
            });
        }
        return Ok((
            Hunk::DeleteFile {
                path: PathBuf::from(filename),
            },
            consumed,
        ));
    }

    if let Some(filename) = first_line.strip_prefix(UPDATE_FILE_MARKER) {
        let filename = filename.trim();
        if filename.is_empty() {
            return Err(InvalidHunkError {
                message: "missing filename for Update File".to_string(),
                line_number: current_line_number,
            });
        }
        let mut move_path: Option<PathBuf> = None;
        let mut remaining = &lines[1..];
        if let Some(first_rest) = remaining.first().copied() {
            if let Some(dest) = first_rest.trim().strip_prefix(MOVE_TO_MARKER) {
                move_path = Some(PathBuf::from(dest.trim()));
                remaining = &remaining[1..];
                consumed += 1;
                current_line_number += 1;
            }
        }

        let mut chunks: Vec<UpdateFileChunk> = Vec::new();
        while !remaining.is_empty() {
            let (chunk, used_lines) = parse_update_chunk(remaining, current_line_number)?;
            consumed += used_lines;
            current_line_number += used_lines;
            chunks.push(chunk);
            remaining = &remaining[used_lines..];
            if remaining.first().is_none_or(|l| {
                let t = l.trim();
                t.starts_with(ADD_FILE_MARKER)
                    || t.starts_with(DELETE_FILE_MARKER)
                    || t.starts_with(UPDATE_FILE_MARKER)
            }) {
                break;
            }
        }

        return Ok((
            Hunk::UpdateFile {
                path: PathBuf::from(filename),
                move_path,
                chunks,
            },
            consumed,
        ));
    }

    Err(InvalidHunkError {
        message: "expected Add/Delete/Update hunk".to_string(),
        line_number: current_line_number,
    })
}

fn parse_update_chunk(
    lines: &[&str],
    starting_line_number: usize,
) -> Result<(UpdateFileChunk, usize), ParseError> {
    let Some(first_line) = lines.first().copied() else {
        return Err(InvalidHunkError {
            message: "missing update chunk".to_string(),
            line_number: starting_line_number,
        });
    };
    let mut consumed = 1;
    let mut current_line_number = starting_line_number;
    let mut change_context: Option<String> = None;
    if first_line.trim().starts_with(CHANGE_CONTEXT_MARKER)
        || first_line.trim() == EMPTY_CHANGE_CONTEXT_MARKER
    {
        let ctx = first_line
            .trim()
            .strip_prefix(CHANGE_CONTEXT_MARKER)
            .unwrap_or("")
            .trim()
            .to_string();
        if !ctx.is_empty() {
            change_context = Some(ctx);
        }
    } else {
        return Err(InvalidHunkError {
            message: "missing @@ context header".to_string(),
            line_number: current_line_number,
        });
    }

    let mut old_lines: Vec<String> = Vec::new();
    let mut new_lines: Vec<String> = Vec::new();
    let mut is_end_of_file = false;

    for line in &lines[1..] {
        let trimmed = line.trim_end();
        current_line_number += 1;
        if trimmed == EOF_MARKER {
            consumed += 1;
            is_end_of_file = true;
            break;
        }
        if trimmed.starts_with("@@")
            || trimmed.starts_with(ADD_FILE_MARKER)
            || trimmed.starts_with(DELETE_FILE_MARKER)
            || trimmed.starts_with(UPDATE_FILE_MARKER)
        {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix('+') {
            new_lines.push(rest.to_string());
        } else if let Some(rest) = trimmed.strip_prefix('-') {
            old_lines.push(rest.to_string());
        } else if let Some(rest) = trimmed.strip_prefix(' ') {
            old_lines.push(rest.to_string());
            new_lines.push(rest.to_string());
        } else if trimmed.is_empty() {
            // bare blank line - treat as blank context line
            old_lines.push(String::new());
            new_lines.push(String::new());
        } else {
            return Err(InvalidHunkError {
                message: format!("unexpected line in update chunk: {trimmed}"),
                line_number: current_line_number,
            });
        }
        consumed += 1;
    }

    Ok((
        UpdateFileChunk {
            change_context,
            old_lines,
            new_lines,
            is_end_of_file,
        },
        consumed,
    ))
}
