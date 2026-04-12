use serde_json::{json, Value};

pub fn example_predicted_next_actions() -> Value {
    Value::Array(vec![
        predicted_next_action(
            "read_file",
            "Inspect the relevant source before making changes.",
        ),
        predicted_next_action(
            "run_command",
            "Verify the current workspace state after the read.",
        ),
    ])
}

fn predicted_next_action(action: &str, intent: &str) -> Value {
    json!({"action": action, "intent": intent})
}

fn run_command_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "run_command",
        "cmd": "rg -n \"pattern\" src/",
        "observation": "Search for the relevant code.",
        "rationale": "Locate the target before patching.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn read_file_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "read_file",
        "path": "src/lib.rs",
        "observation": "Read the file for context.",
        "rationale": "Need context before editing.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn symbols_index_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "symbols_index",
        "path": "src",
        "out": "state/symbols.json",
        "observation": "Build deterministic symbol inventory.",
        "rationale": "Need a unique sorted symbols catalog before rename/refactor planning.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn symbols_rename_candidates_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "symbols_rename_candidates",
        "symbols_path": "state/symbols.json",
        "out": "state/rename_candidates.json",
        "observation": "Derive deterministic rename candidates from symbol inventory.",
        "rationale": "Prioritize naming cleanup before direct symbol mutation.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn symbols_prepare_rename_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "symbols_prepare_rename",
        "candidates_path": "state/rename_candidates.json",
        "index": 0,
        "out": "state/next_rename_action.json",
        "observation": "Select the top rename candidate.",
        "rationale": "Prepare a concrete rename_symbol payload before mutation.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn rename_symbol_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "rename_symbol",
        "path": "src/tools.rs",
        "line": 2230,
        "column": 8,
        "old_name": "handle_plan_action",
        "new_name": "handle_master_plan_action",
        "question": "Is this exact symbol-at-position the one that should be renamed without changing behavior?",
        "observation": "Target identifier located at the given position.",
        "rationale": "Perform a deterministic symbol rename.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn list_dir_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "list_dir",
        "path": ".",
        "observation": "List workspace files.",
        "rationale": "Locate the target before acting.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn apply_patch_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "apply_patch",
        "patch": "*** Begin Patch\n*** Update File: path/to/file.rs\n@@\n- old\n+ new\n*** End Patch",
        "observation": "Apply the requested change.",
        "rationale": "Implement the edit directly.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn python_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "python",
        "code": "print('analysis')",
        "observation": "Run structured analysis.",
        "rationale": "Use Python for parsing tasks.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn cargo_test_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "cargo_test",
        "crate": "canon-mini-agent",
        "test": "optional_test_name",
        "observation": "Run the targeted test.",
        "rationale": "Verify the change.",
        "predicted_next_actions": predicted_next_actions
    })
}

fn plan_example_action(predicted_next_actions: &Value) -> Value {
    json!({
        "action": "plan",
        "op": "create_task",
        "task": {
            "id": "T4",
            "title": "Add plan DAG",
            "status": "todo",
            "priority": 3
        },
        "observation": "Planning update needed.",
        "rationale": "Track work in PLAN.json via plan tool.",
        "predicted_next_actions": predicted_next_actions
    })
}

pub fn non_message_example_action(kind: &str) -> Option<Value> {
    let predicted_next_actions = example_predicted_next_actions();
    match kind {
        "run_command" => Some(run_command_example_action(&predicted_next_actions)),
        "read_file" => Some(read_file_example_action(&predicted_next_actions)),
        "symbols_index" => Some(symbols_index_example_action(&predicted_next_actions)),
        "symbols_rename_candidates" => {
            Some(symbols_rename_candidates_example_action(&predicted_next_actions))
        }
        "symbols_prepare_rename" => {
            Some(symbols_prepare_rename_example_action(&predicted_next_actions))
        }
        "rename_symbol" => Some(rename_symbol_example_action(&predicted_next_actions)),
        "list_dir" => Some(list_dir_example_action(&predicted_next_actions)),
        "apply_patch" => Some(apply_patch_example_action(&predicted_next_actions)),
        "python" => Some(python_example_action(&predicted_next_actions)),
        "cargo_test" => Some(cargo_test_example_action(&predicted_next_actions)),
        "plan" => Some(plan_example_action(&predicted_next_actions)),
        _ => None,
    }
}
