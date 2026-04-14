use anyhow::Result;
use canon_mini_agent::{lessons::maybe_synthesize_lessons, set_agent_state_dir, set_workspace};
use std::path::{Path, PathBuf};

fn find_flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|window| window[0] == flag)
        .map(|window| window[1].as_str())
}

fn parse_flag_value(args: &[String], flag: &str) -> Option<String> {
    find_flag_value(args, flag).map(str::to_owned)
}

fn usage() -> &'static str {
    "canon-lessons-synth\n\
\n\
Synthesizes lessons candidates from the action log and writes:\n\
  <workspace>/agent_state/lessons_candidates.json\n\
\n\
Usage:\n\
  canon-lessons-synth [--workspace PATH] [--state-dir PATH]\n\
\n\
Defaults:\n\
  workspace = current working directory\n\
  state-dir  = <workspace>/agent_state\n"
}

fn canonical_or_original(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn resolve_path_flag(args: &[String], flag: &str, default: PathBuf) -> PathBuf {
    canonical_or_original(
        parse_flag_value(args, flag)
            .map(PathBuf::from)
            .unwrap_or(default),
    )
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprint!("{}", usage());
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let workspace = resolve_path_flag(&args, "--workspace", cwd.clone());
    let state_dir = resolve_path_flag(&args, "--state-dir", workspace.join("agent_state"));

    set_workspace(workspace.to_string_lossy().into_owned());
    set_agent_state_dir(state_dir.to_string_lossy().into_owned());
    maybe_synthesize_lessons(Path::new(&workspace));

    let candidates_path = workspace
        .join("agent_state")
        .join("lessons_candidates.json");
    println!("{}", candidates_path.display());
    Ok(())
}
