use anyhow::{Context, Result};
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;

fn find_flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].as_str())
}

fn take_flag_value(args: &[String], name: &str) -> Option<String> {
    find_flag_value(args, name).map(str::to_owned)
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn usage() -> &'static str {
    "canon-exec: execute one canon tool action (stdin JSON -> stdout JSON)\n\
\n\
Usage:\n\
  canon-exec --workspace <path> --role <role> --step <n> [--state-dir <path>] [--check-on-done]\n\
\n\
Input (stdin): a single JSON object representing a tool action.\n\
Output (stdout): {\"ok\":true,\"done\":false,\"output\":\"...\"}\n"
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if has_flag(&args, "--help") || has_flag(&args, "-h") {
        eprint!("{}", usage());
        return Ok(());
    }

    let workspace = take_flag_value(&args, "--workspace")
        .context("missing --workspace")?;
    let role = take_flag_value(&args, "--role").unwrap_or_else(|| "capability".to_string());
    let step: usize = take_flag_value(&args, "--step")
        .context("missing --step")?
        .parse()
        .context("--step must be an integer")?;
    let state_dir = take_flag_value(&args, "--state-dir");
    let check_on_done = has_flag(&args, "--check-on-done");

    configure_runtime(&workspace, state_dir);

    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).context("read stdin")?;
    let action: Value = serde_json::from_str(&raw).context("stdin is not valid JSON")?;

    let (done, output) = canon_mini_agent::execute_action_capability(
        &role,
        step,
        &action,
        &PathBuf::from(&workspace),
        check_on_done,
    )?;

    let out = serde_json::json!({
        "ok": true,
        "done": done,
        "output": output,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn configure_runtime(workspace: &str, state_dir: Option<String>) {
    canon_mini_agent::set_workspace(workspace.to_string());
    if let Some(dir) = state_dir {
        canon_mini_agent::set_agent_state_dir(dir);
    }
    canon_mini_agent::logging::init_log_paths("capability");
}
