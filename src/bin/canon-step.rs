use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Read;

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn usage() -> &'static str {
    "canon-step: lightweight next-action predictor (stdin JSON -> stdout JSON)\n\
\n\
Usage:\n\
  canon-step\n\
\n\
Input (stdin): JSON object. Supported shapes:\n\
  1) a tool action object (may include \"predicted_next_actions\")\n\
  2) a result wrapper: {\"action\":{...},\"output\":\"...\"}\n\
\n\
Output (stdout): {\"predicted_next_actions\":[...]} where entries are action stubs.\n"
}

fn read_action_input() -> Result<Value> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).context("read stdin")?;
    serde_json::from_str(&raw).context("stdin is not valid JSON")
}

fn predicted_next_actions(action: &Value) -> Vec<Value> {
    if let Some(existing) = existing_predicted_next_actions(action) {
        return existing;
    }
    let kind = action.get("action").and_then(|v| v.as_str()).unwrap_or("");
    predicted_next_actions_for_kind(kind, action)
}

fn predicted_next_actions_for_kind(kind: &str, action: &Value) -> Vec<Value> {
    match kind {
        "apply_patch" => {
            simple_action_prediction("cargo_test", "Verify the patch compiles and tests pass.")
        }
        "symbols_index" => read_file_prediction_for_output(
            action,
            "state/symbols.json",
            "Inspect generated symbols inventory.",
        ),
        "symbols_rename_candidates" => read_file_prediction_for_output(
            action,
            "state/rename_candidates.json",
            "Inspect generated rename candidates.",
        ),
        "symbols_prepare_rename" => read_file_prediction_for_output(
            action,
            "state/next_rename_action.json",
            "Inspect prepared rename action JSON.",
        ),
        "rename_symbol" => {
            simple_action_prediction("cargo_test", "Run tests after rename to ensure no regressions.")
        }
        "run_command" => {
            simple_action_prediction("message", "Summarize command output and decide next step.")
        }
        "read_file" => {
            simple_action_prediction("message", "Summarize findings and choose the next concrete action.")
        }
        _ => Vec::new(),
    }
}

fn existing_predicted_next_actions(action: &Value) -> Option<Vec<Value>> {
    action
        .get("predicted_next_actions")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().cloned().collect())
}

fn out_or_default<'a>(action: &'a Value, default: &'a str) -> &'a str {
    action.get("out").and_then(|v| v.as_str()).unwrap_or(default)
}

fn read_file_prediction_for_output(action: &Value, default: &str, intent: &str) -> Vec<Value> {
    let out = out_or_default(action, default);
    build_read_file_prediction(out, intent)
}

fn simple_action_prediction(action: &str, intent: &str) -> Vec<Value> {
    vec![json!({"action": action, "intent": intent})]
}

fn build_read_file_prediction(path: &str, intent: &str) -> Vec<Value> {
    vec![json!({"action": "read_file", "path": path, "intent": intent})]
}

fn maybe_print_usage(args: &[String]) -> bool {
    if has_flag(args, "--help") || has_flag(args, "-h") {
        eprint!("{}", usage());
        return true;
    }
    false
}

fn prediction_output(input: &Value) -> Value {
    let action = input.get("action").unwrap_or(input);
    json!({
        "predicted_next_actions": predicted_next_actions(action),
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if maybe_print_usage(&args) {
        return Ok(());
    }

    let input = read_action_input()?;
    let out = prediction_output(&input);
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
