use anyhow::{bail, Context, Result};
use canon_mini_agent::events::EffectEvent;
use canon_mini_agent::logging::{
    artifact_write_signature, record_effect_for_workspace, write_projection_with_artifact_effects,
};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn usage() -> &'static str {
    "canon-user-chat: inject an external user message into canon-mini-agent\n\
\n\
Usage:\n\
  canon-user-chat --workspace <abs-path> [--state-dir <abs-path>] --message <text> [--to <role>]\n\
  canon-user-chat --workspace <abs-path> [--state-dir <abs-path>] --message-file <path> [--to <role>]\n\
  canon-user-chat --workspace <abs-path> [--state-dir <abs-path>] --read-reply\n\
\n\
Defaults:\n\
  --to solo\n\
\n\
Behavior:\n\
  - writes agent_state/last_message_to_<role>.json\n\
  - writes agent_state/wakeup_<role>.flag\n\
  - when --read-reply is used, prints agent_state/last_message_to_user.json if present\n"
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

fn resolve_state_dir(workspace: &Path, state_dir_flag: Option<String>) -> Result<PathBuf> {
    let path = state_dir_flag
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("agent_state"));
    if !path.is_absolute() {
        bail!(
            "--state-dir must be an absolute path, got: {}",
            path.display()
        );
    }
    Ok(path)
}

fn read_message(args: &[String]) -> Result<String> {
    if let Some(message) = take_flag_value(args, "--message") {
        let trimmed = message.trim().to_string();
        if trimmed.is_empty() {
            bail!("--message cannot be empty");
        }
        return Ok(trimmed);
    }
    if let Some(path) = take_flag_value(args, "--message-file") {
        let text =
            fs::read_to_string(&path).with_context(|| format!("read --message-file {}", path))?;
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            bail!("--message-file contained only whitespace");
        }
        return Ok(trimmed);
    }
    bail!("missing --message or --message-file")
}

fn build_user_handoff_action_text(to_key: &str, message: &str) -> Result<String> {
    serde_json::to_string_pretty(&json!({
        "action": "message",
        "from": "user",
        "to": to_key,
        "type": "handoff",
        "status": "ready",
        "observation": "External user message injected into the runtime.",
        "rationale": "Process the request under canonical law and current system policy rather than replying outside the runtime.",
        "payload": {
            "summary": message,
            "user_message": message,
            "reply_to": "user"
        }
    }))
    .map_err(Into::into)
}

fn write_user_message_projections(
    workspace: &Path,
    state_dir: &Path,
    to_key: &str,
    action_text: &str,
) -> Result<PathBuf> {
    let msg_path = state_dir.join(format!("last_message_to_{}.json", to_key));
    write_projection_with_artifact_effects(
        workspace,
        &msg_path,
        &format!("agent_state/last_message_to_{}.json", to_key),
        "write",
        "external_user_handoff_projection",
        action_text,
    )?;

    let wake_path = state_dir.join(format!("wakeup_{}.flag", to_key));
    write_projection_with_artifact_effects(
        workspace,
        &wake_path,
        &format!("agent_state/wakeup_{}.flag", to_key),
        "write",
        "external_user_handoff_wakeup",
        "user_message",
    )?;

    Ok(msg_path)
}

fn write_user_message(
    workspace: &Path,
    state_dir: &Path,
    to_role: &str,
    message: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create state dir {}", state_dir.display()))?;
    let to_key = sanitize_role(to_role);
    let action_text = build_user_handoff_action_text(&to_key, message)?;
    let signature = artifact_write_signature(&[
        "inbound_message",
        "user",
        &to_key,
        &action_text.len().to_string(),
        action_text.as_str(),
    ]);
    record_effect_for_workspace(
        workspace,
        EffectEvent::InboundMessageRecorded {
            from_role: "user".to_string(),
            to_role: to_key.clone(),
            message: action_text.clone(),
            signature,
        },
    )?;
    write_user_message_projections(workspace, state_dir, &to_key, &action_text)
}

fn read_user_reply(state_dir: &Path) -> Result<Option<String>> {
    let reply_path = state_dir.join("last_message_to_user.json");
    match fs::read_to_string(&reply_path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", reply_path.display())),
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if has_flag(&args, "--help") || has_flag(&args, "-h") {
        print!("{}", usage());
        return Ok(());
    }

    let workspace = take_flag_value(&args, "--workspace").context("missing --workspace")?;
    let workspace = PathBuf::from(workspace);
    if !workspace.is_absolute() {
        bail!(
            "--workspace must be an absolute path, got: {}",
            workspace.display()
        );
    }
    let state_dir = resolve_state_dir(&workspace, take_flag_value(&args, "--state-dir"))?;

    canon_mini_agent::set_workspace(workspace.display().to_string());
    canon_mini_agent::set_agent_state_dir(state_dir.display().to_string());

    if has_flag(&args, "--read-reply") {
        match read_user_reply(&state_dir)? {
            Some(reply) => {
                println!("{}", reply);
            }
            None => {
                println!("{{}}")
            }
        }
        return Ok(());
    }

    let to_role = take_flag_value(&args, "--to").unwrap_or_else(|| "solo".to_string());
    let msg_path = write_user_message(&workspace, &state_dir, &to_role, &read_message(&args)?)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "delivered_to": sanitize_role(&to_role),
            "message_path": msg_path,
            "wakeup_flag": state_dir.join(format!("wakeup_{}.flag", sanitize_role(&to_role))),
        }))?
    );
    Ok(())
}
