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

fn predicted_next_actions(action: &Value) -> Vec<Value> {
    if let Some(arr) = action.get("predicted_next_actions").and_then(|v| v.as_array()) {
        return arr.iter().cloned().collect();
    }
    let kind = action.get("action").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "apply_patch" => cargo_test_prediction("Verify the patch compiles and tests pass."),
        "symbols_index" => {
            let out = out_or_default(action, "state/symbols.json");
            build_read_file_prediction(out, "Inspect generated symbols inventory.")
        }
        "symbols_rename_candidates" => {
            let out = out_or_default(action, "state/rename_candidates.json");
            build_read_file_prediction(out, "Inspect generated rename candidates.")
        }
        "symbols_prepare_rename" => {
            let out = out_or_default(action, "state/next_rename_action.json");
            build_read_file_prediction(out, "Inspect prepared rename action JSON.")
        }
        "rename_symbol" => cargo_test_prediction("Run tests after rename to ensure no regressions."),
        "run_command" => message_prediction("Summarize command output and decide next step."),
        "read_file" => message_prediction("Summarize findings and choose the next concrete action."),
        _ => Vec::new(),
    }
}

fn out_or_default<'a>(action: &'a Value, default: &'a str) -> &'a str {
    action.get("out").and_then(|v| v.as_str()).unwrap_or(default)
}

fn cargo_test_prediction(intent: &str) -> Vec<Value> {
    vec![json!({"action": "cargo_test", "intent": intent})]
}

fn message_prediction(intent: &str) -> Vec<Value> {
    vec![json!({"action": "message", "intent": intent})]
}

fn build_read_file_prediction(path: &str, intent: &str) -> Vec<Value> {
    vec![json!({"action": "read_file", "path": path, "intent": intent})]
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if has_flag(&args, "--help") || has_flag(&args, "-h") {
        eprint!("{}", usage());
        return Ok(());
    }

    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).context("read stdin")?;
    let input: Value = serde_json::from_str(&raw).context("stdin is not valid JSON")?;

    let action = input.get("action").unwrap_or(&input);
    let out = json!({
        "predicted_next_actions": predicted_next_actions(action),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
