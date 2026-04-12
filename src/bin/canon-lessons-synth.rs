use anyhow::Result;
use canon_mini_agent::{lessons::maybe_synthesize_lessons, set_agent_state_dir, set_workspace};
use std::path::{Path, PathBuf};

fn parse_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|window| window[0] == flag)
        .map(|window| window[1].clone())
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprint!("{}", usage());
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let workspace = parse_flag_value(&args, "--workspace")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.clone());
    let workspace = canonical_or_original(workspace);
    let state_dir = parse_flag_value(&args, "--state-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("agent_state"));
    let state_dir = canonical_or_original(state_dir);

    set_workspace(workspace.to_string_lossy().into_owned());
    set_agent_state_dir(state_dir.to_string_lossy().into_owned());
    maybe_synthesize_lessons(Path::new(&workspace));

    let candidates_path = workspace.join("agent_state").join("lessons_candidates.json");
    println!("{}", candidates_path.display());
    Ok(())
}
