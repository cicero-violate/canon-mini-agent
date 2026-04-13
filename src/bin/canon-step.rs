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
    let raw = read_stdin_string()?;
    serde_json::from_str(&raw).context("stdin is not valid JSON")
}

fn read_stdin_string() -> Result<String> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).context("read stdin")?;
    Ok(raw)
}

fn predicted_next_actions(action: &Value) -> Vec<Value> {
    forward_prediction((existing_predicted_next_actions(action), action), |(existing, action)| {
        existing.unwrap_or_else(|| predicted_next_actions_from_kind(action))
    })
}

fn forward_prediction<T, U>(input: T, build: impl FnOnce(T) -> U) -> U {
    build(input)
}

fn predicted_next_actions_from_kind(action: &Value) -> Vec<Value> {
    forward_prediction((action_kind(action), action), |(kind, action)| {
        predicted_next_actions_for_kind(kind, action)
    })
}

fn action_kind(action: &Value) -> &str {
    action.get("action").and_then(|v| v.as_str()).unwrap_or("")
}

fn predicted_next_actions_for_kind(kind: &str, action: &Value) -> Vec<Value> {
    simple_prediction_for_kind(kind)
        .or_else(|| file_prediction_for_kind(kind, action))
        .unwrap_or_default()
}

fn simple_prediction_for_kind(kind: &str) -> Option<Vec<Value>> {
    simple_prediction_spec(kind).map(|spec| {
        forward_prediction(spec, |(action, intent)| {
            single_prediction(simple_action_prediction_value(action, intent))
        })
    })
}

fn simple_prediction_spec(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "apply_patch" => Some(("cargo_test", "Verify the patch compiles and tests pass.")),
        "rename_symbol" => Some((
            "cargo_test",
            "Run tests after rename to ensure no regressions.",
        )),
        "run_command" => Some(("message", "Summarize command output and decide next step.")),
        "read_file" => Some((
            "message",
            "Summarize findings and choose the next concrete action.",
        )),
        _ => None,
    }
}

fn file_prediction_for_kind(kind: &str, action: &Value) -> Option<Vec<Value>> {
    forward_prediction((file_prediction_spec(kind), action), |(spec, action)| {
        spec.map(|spec| prediction_from_file_spec(action, spec))
    })
}

fn prediction_from_file_spec(
    action: &Value,
    spec: (&'static str, &'static str),
) -> Vec<Value> {
    let (default, intent) = spec;
    read_file_prediction_for_output(action, default, intent)
}

fn file_prediction_spec(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "symbols_index" => Some((
            "state/symbols.json",
            "Inspect generated symbols inventory.",
        )),
        "symbols_rename_candidates" => Some((
            "state/rename_candidates.json",
            "Inspect generated rename candidates.",
        )),
        "symbols_prepare_rename" => Some((
            "state/next_rename_action.json",
            "Inspect prepared rename action JSON.",
        )),
        _ => None,
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
    let path = out_or_default(action, default);
    single_prediction(json!({"action": "read_file", "path": path, "intent": intent}))
}

fn single_prediction(prediction: Value) -> Vec<Value> {
    vec![prediction]
}

fn simple_action_prediction(action: &str, intent: &str) -> Vec<Value> {
    forward_prediction((action, intent), |(action, intent)| {
        single_prediction(simple_action_prediction_value(action, intent))
    })
}

fn simple_action_prediction_value(action: &str, intent: &str) -> Value {
    json!({"action": action, "intent": intent})
}

// build_read_file_prediction removed: logic now inlined into read_file_prediction_for_output

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

fn write_prediction_output(output: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(output)?);
    Ok(())
}

fn emit_prediction_from_stdin() -> Result<()> {
    let input = read_action_input()?;
    let output = prediction_output(&input);
    write_prediction_output(&output)
}

fn run_with_args(args: &[String]) -> Result<()> {
    if maybe_print_usage(args) {
        return Ok(());
    }
    emit_prediction_from_stdin()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    run_with_args(&args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicted_next_actions_preserves_existing_fast_path() {
        let input = json!({
            "predicted_next_actions": [
                {"action": "message", "intent": "precomputed"}
            ]
        });

        let result = predicted_next_actions(&input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["action"], "message");
        assert_eq!(result[0]["intent"], "precomputed");
    }

    #[test]
    fn action_kind_defaults_to_empty_string() {
        let input = json!({"out": "unused"});
        assert_eq!(action_kind(&input), "");
    }

    #[test]
    fn predicted_next_actions_from_kind_preserves_apply_patch_mapping() {
        let input = json!({"action": "apply_patch"});
        let result = predicted_next_actions_from_kind(&input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["action"], "cargo_test");
        assert_eq!(
            result[0]["intent"],
            "Verify the patch compiles and tests pass."
        );
    }

    #[test]
    fn simple_prediction_spec_preserves_apply_patch_mapping() {
        assert_eq!(
            simple_prediction_spec("apply_patch"),
            Some(("cargo_test", "Verify the patch compiles and tests pass."))
        );
    }

    #[test]
    fn write_prediction_output_serializes_pretty_json() {
        let output = json!({"predicted_next_actions": []});
        let encoded = serde_json::to_string_pretty(&output).unwrap();
        assert!(encoded.contains("predicted_next_actions"));
    }

    #[test]
    fn file_prediction_spec_preserves_symbols_index_mapping() {
        assert_eq!(
            file_prediction_spec("symbols_index"),
            Some((
                "state/symbols.json",
                "Inspect generated symbols inventory.",
            ))
        );
    }

    #[test]
    fn file_prediction_for_kind_preserves_symbols_index_output() {
        let input = json!({"out": "custom/path.json"});
        let prediction = file_prediction_for_kind("symbols_index", &input).unwrap();
        assert_eq!(prediction.len(), 1);
        assert_eq!(prediction[0]["action"], "read_file");
        assert_eq!(prediction[0]["path"], "custom/path.json");
        assert_eq!(prediction[0]["intent"], "Inspect generated symbols inventory.");
    }

    #[test]
    fn predicted_next_actions_falls_back_to_file_prediction_for_symbols_index() {
        let input = json!({"action": "symbols_index", "out": "custom/path.json"});
        let result = predicted_next_actions(&input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["action"], "read_file");
        assert_eq!(result[0]["path"], "custom/path.json");
        assert_eq!(result[0]["intent"], "Inspect generated symbols inventory.");
    }

    #[test]
    fn simple_action_prediction_preserves_action_and_intent() {
        let prediction = simple_action_prediction("message", "keep behavior");
        assert_eq!(prediction.len(), 1);
        assert_eq!(prediction[0]["action"], "message");
        assert_eq!(prediction[0]["intent"], "keep behavior");
    }

    #[test]
    fn simple_action_prediction_value_preserves_action_and_intent() {
        let prediction = simple_action_prediction_value("cargo_test", "verify");
        assert_eq!(prediction["action"], "cargo_test");
        assert_eq!(prediction["intent"], "verify");
    }
}
