# Tool action syntax examples

## `message` — send inter-agent protocol message

Examples:
  {"action":"message","from":"executor","to":"verifier","type":"handoff","status":"complete","observation":"Summarize what happened.","rationale":"Execution work is complete and the verifier now has enough evidence to judge it.","payload":{"summary":"brief evidence summary","artifacts":["path/to/file.rs"]}}
  {"action":"message","from":"executor","to":"planner","type":"blocker","status":"blocked","observation":"Describe the blocker.","rationale":"Explain why progress is impossible.","payload":{"summary":"Short blocker summary","blocker":"Root cause","evidence":"Concrete error text","required_action":"What must be done to unblock","severity":"error"}}
Allowed roles: executor|planner|verifier|diagnostics|solo. Allowed types: handoff|result|verification|failure|blocker|plan|diagnostics. Allowed status: complete|in_progress|failed|verified|ready|blocked.
⚠ message with status=complete is REJECTED if build or tests fail — fix all errors first.

## `list_dir` — inspect directory contents

Example:
  {"action":"list_dir","path":".","rationale":"Inspect the workspace before making assumptions."}

## `read_file` — read a file; output is line-numbered

Examples:
  {"action":"read_file","path":"src/app.rs","rationale":"Read the file before editing it."}
  {"action":"read_file","path":"src/app.rs","line":120,"rationale":"Read the relevant section before editing it."}
With "line":N the output starts at line N and shows up to 1000 lines.
⚠ Always read a file before patching it. Never patch from memory.
⚠ Paths may be relative to WORKSPACE or absolute under WORKSPACE.
⚠ read_file output is prefixed with line numbers ("42: code here"). Strip the "N: " prefix when writing patch lines.
WRONG:  -42: fn old() {}   RIGHT:  -fn old() {}

## `symbols_index` — build deterministic symbols index JSON from Rust sources

Example:
  {"action":"symbols_index","path":"src","out":"state/symbols.json","rationale":"Build a deterministic unique symbol catalog for planning and rename work.","predicted_next_actions":[{"action":"read_file","intent":"Inspect generated symbols.json and choose target symbols."},{"action":"rename_symbol","intent":"Apply a precise rename for one selected symbol."}]}
Notes:
- `path` defaults to workspace root.
- `out` defaults to `state/symbols.json`.

## `symbols_rename_candidates` — derive deterministic rename candidates from symbols.json using naming heuristics

Example:
  {"action":"symbols_rename_candidates","symbols_path":"state/symbols.json","out":"state/rename_candidates.json","rationale":"Surface high-value rename candidates before mutating code.","predicted_next_actions":[{"action":"read_file","intent":"Inspect rename candidates and choose one."},{"action":"rename_symbol","intent":"Apply a precise rename for the selected candidate."}]}
Notes:
- `symbols_path` defaults to `state/symbols.json`.
- `out` defaults to `state/rename_candidates.json`.

## `symbols_prepare_rename` — pick a rename candidate and emit a ready-to-run rename_symbol action JSON skeleton

Example:
  {"action":"symbols_prepare_rename","candidates_path":"state/rename_candidates.json","index":0,"out":"state/next_rename_action.json","rationale":"Pick the top candidate and prepare a deterministic rename action payload.","predicted_next_actions":[{"action":"read_file","intent":"Inspect prepared rename action JSON for correctness."},{"action":"rename_symbol","intent":"Execute the prepared rename action."}]}
Notes:
- `candidates_path` defaults to `state/rename_candidates.json`.
- `index` defaults to 0.
- `out` defaults to `state/next_rename_action.json`.

## `rename_symbol` — rename a Rust identifier at an exact line/column using rust-analyzer syntax APIs (file-scoped in v1)

Example:
  {"action":"rename_symbol","path":"src/tools.rs","line":2230,"column":8,"old_name":"handle_plan_action","new_name":"handle_master_plan_action","question":"Is this exact symbol-at-position the one that should be renamed without changing behavior?","rationale":"Apply a deterministic symbol rename at the precise declaration/reference location.","predicted_next_actions":[{"action":"cargo_test","intent":"Run focused tests covering the renamed symbol path."},{"action":"run_command","intent":"Run cargo check to verify no compile regressions."}]}
Notes:
- `line` and `column` are 1-based.
- v1 is file-scoped and only supports `.rs` files.

## `issue` — record/update discovered issues in ISSUES.json for later attention

Examples:
  {"action":"issue","op":"read","rationale":"Check open issues before starting work."}
  {"action":"issue","op":"create","issue":{"id":"ISS-001","title":"Retry loop does not fire for submit-only turns","status":"open","priority":"high","kind":"bug","description":"...","location":"src/ws_server.rs:554","evidence":["frames/inbound.jsonl fc=91 only presence frames after fc=76 heartbeat"],"discovered_by":"solo"},"rationale":"Record the stall bug for later fix."}
  {"action":"issue","op":"set_status","issue_id":"ISS-001","status":"resolved","rationale":"Issue was fixed by removing the pending check."}
  {"action":"issue","op":"update","issue_id":"ISS-001","updates":{"priority":"medium","description":"Updated description"},"rationale":"Revise issue details."}
Allowed status: open | in_progress | resolved | wontfix
Allowed priority: high | medium | low
Allowed kind: bug | logic | invariant_violation | performance | stale_state

## `objectives` — read/update objectives in PLANS/OBJECTIVES.json

Examples:
  {"action":"objectives","op":"read","rationale":"Load only non-completed objectives for planning/verification."}
  {"action":"objectives","op":"read","include_done":true,"rationale":"Load all objectives, including completed."}
  {"action":"objectives","op":"create_objective","objective":{"id":"obj_new","title":"New objective","status":"active","scope":"...","authority_files":["src/foo.rs"],"category":"quality","level":"low","description":"...","requirement":[],"verification":[],"success_criteria":[]},"rationale":"Record a new objective."}
  {"action":"objectives","op":"set_status","objective_id":"obj_new","status":"done","rationale":"Mark objective complete."}
  {"action":"objectives","op":"update_objective","objective_id":"obj_new","updates":{"scope":"updated scope"},"rationale":"Update objective fields."}
  {"action":"objectives","op":"delete_objective","objective_id":"obj_new","rationale":"Remove obsolete objective."}
  {"action":"objectives","op":"replace_objectives","objectives":[],"rationale":"Replace objectives list."}
  {"action":"objectives","op":"sorted_view","rationale":"View objectives sorted by status."}

## `apply_patch` — create or update files using unified patch syntax

Examples:
  {"action":"apply_patch","patch":"*** Begin Patch\n*** Add File: path/to/new.rs\n+line one\n+line two\n*** End Patch","rationale":"Apply the concrete code change after reading the target context."}
  {"action":"apply_patch","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n fn before_before() {}\n fn before() {}\n fn target() {\n-    old_body();\n+    new_body();\n }\n fn after() {}\n*** End Patch","rationale":"Update the file using exact surrounding context from the read."}
  {"action":"apply_patch","patch":"*** Begin Patch\n*** Delete File: PLANS/executor-b.json\n*** Add File: PLANS/executor-b.json\n+# new content\n+line two\n*** End Patch","rationale":"Full-file replacement is safer than a giant hunk with many - lines."}
Rules:
- Every @@ hunk must have AT LEAST 3 unchanged context lines around the edit.
- Never use @@ with only 1 context line.
- ALL - lines must be copied character-for-character from read_file output (minus the "N: " prefix).
- If replacing more than ~10 lines, use *** Delete File + *** Add File instead of a large @@ hunk.
- NEVER use absolute paths inside the patch string.

## `run_command` — run shell commands for discovery or verification

Examples:
  {"action":"run_command","cmd":"cargo check -p canon-mini-agent","cwd":"/workspace/ai_sandbox/canon-mini-agent","rationale":"Validate the target crate after a change."}
  {"action":"run_command","cmd":"rg -n 'fn foo' src","cwd":"/workspace/ai_sandbox/canon-mini-agent","rationale":"Search the codebase for the relevant symbol before editing."}
⚠ cwd may be relative to WORKSPACE or absolute under WORKSPACE.

## `python` — run Python analysis inside the workspace

Example:
  {"action":"python","code":"from pathlib import Path\nprint(len(list(Path('src').glob('**/*.rs'))))","cwd":"/workspace/ai_sandbox/canon-mini-agent","rationale":"Use Python for structured workspace analysis."}
⚠ cwd may be relative to WORKSPACE or absolute under WORKSPACE.

## `cargo_test` — run a targeted cargo test (harness-style)

Example:
  {"action":"cargo_test","crate":"canon-runtime","test":"some_test_name","rationale":"Run the exact failing test using the harness-style command."}

## `plan` — create/update/delete tasks and DAG edges in PLAN.json

Examples:
  {"action":"plan","op":"set_task_status","rationale":"Update a single task status in PLAN.json.","status":"in_progress","task_id":"T1"}
  {"action":"plan","op":"set_plan_status","rationale":"Update top-level PLAN.json status.","status":"in_progress"}
  {"action":"plan","op":"sorted_view","rationale":"View the current plan in DAG order (read-only)."}

## `rustc_hir` — emit HIR for analysis

```json
{
  "properties": {
    "action": {
      "enum": [
        "rustc_hir"
      ],
      "type": "string"
    },
    "crate": {
      "type": "string"
    },
    "extra": {
      "type": [
        "string",
        "null"
      ]
    },
    "mode": {
      "type": "string"
    },
    "observation": {
      "type": [
        "string",
        "null"
      ]
    },
    "predicted_next_actions": {
      "items": {
        "$ref": "#/definitions/PredictedNextAction"
      },
      "type": "array"
    },
    "rationale": {
      "minLength": 1,
      "type": "string"
    }
  },
  "required": [
    "action",
    "crate",
    "mode",
    "predicted_next_actions",
    "rationale"
  ],
  "type": "object"
}
```

## `rustc_mir` — emit MIR for analysis

```json
{
  "properties": {
    "action": {
      "enum": [
        "rustc_mir"
      ],
      "type": "string"
    },
    "crate": {
      "type": "string"
    },
    "extra": {
      "type": [
        "string",
        "null"
      ]
    },
    "mode": {
      "type": "string"
    },
    "observation": {
      "type": [
        "string",
        "null"
      ]
    },
    "predicted_next_actions": {
      "items": {
        "$ref": "#/definitions/PredictedNextAction"
      },
      "type": "array"
    },
    "rationale": {
      "minLength": 1,
      "type": "string"
    }
  },
  "required": [
    "action",
    "crate",
    "mode",
    "predicted_next_actions",
    "rationale"
  ],
  "type": "object"
}
```

## `graph_call` — emit call graph CSVs

```json
{
  "properties": {
    "action": {
      "enum": [
        "graph_call"
      ],
      "type": "string"
    },
    "crate": {
      "type": "string"
    },
    "observation": {
      "type": [
        "string",
        "null"
      ]
    },
    "out_dir": {
      "type": [
        "string",
        "null"
      ]
    },
    "predicted_next_actions": {
      "items": {
        "$ref": "#/definitions/PredictedNextAction"
      },
      "type": "array"
    },
    "rationale": {
      "minLength": 1,
      "type": "string"
    }
  },
  "required": [
    "action",
    "crate",
    "predicted_next_actions",
    "rationale"
  ],
  "type": "object"
}
```

## `graph_cfg` — emit CFG CSVs

```json
{
  "properties": {
    "action": {
      "enum": [
        "graph_cfg"
      ],
      "type": "string"
    },
    "crate": {
      "type": "string"
    },
    "observation": {
      "type": [
        "string",
        "null"
      ]
    },
    "out_dir": {
      "type": [
        "string",
        "null"
      ]
    },
    "predicted_next_actions": {
      "items": {
        "$ref": "#/definitions/PredictedNextAction"
      },
      "type": "array"
    },
    "rationale": {
      "minLength": 1,
      "type": "string"
    }
  },
  "required": [
    "action",
    "crate",
    "predicted_next_actions",
    "rationale"
  ],
  "type": "object"
}
```

## `graph_dataflow` — emit dataflow reports

```json
{
  "properties": {
    "action": {
      "enum": [
        "graph_dataflow"
      ],
      "type": "string"
    },
    "crate": {
      "type": "string"
    },
    "observation": {
      "type": [
        "string",
        "null"
      ]
    },
    "out_dir": {
      "type": [
        "string",
        "null"
      ]
    },
    "predicted_next_actions": {
      "items": {
        "$ref": "#/definitions/PredictedNextAction"
      },
      "type": "array"
    },
    "rationale": {
      "minLength": 1,
      "type": "string"
    },
    "tlog": {
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "action",
    "crate",
    "predicted_next_actions",
    "rationale"
  ],
  "type": "object"
}
```

## `graph_reachability` — emit reachability reports

```json
{
  "properties": {
    "action": {
      "enum": [
        "graph_reachability"
      ],
      "type": "string"
    },
    "crate": {
      "type": "string"
    },
    "observation": {
      "type": [
        "string",
        "null"
      ]
    },
    "out_dir": {
      "type": [
        "string",
        "null"
      ]
    },
    "predicted_next_actions": {
      "items": {
        "$ref": "#/definitions/PredictedNextAction"
      },
      "type": "array"
    },
    "rationale": {
      "minLength": 1,
      "type": "string"
    },
    "tlog": {
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "action",
    "crate",
    "predicted_next_actions",
    "rationale"
  ],
  "type": "object"
}
```

## `stage_graph` — emit a synthetic OODA-style stage graph (written to state/orchestrator/stage_graph.json by default)

Example:
  {"action":"stage_graph","rationale":"Generate the current stage graph for agent branching and introspection.","predicted_next_actions":[{"action":"read_file","intent":"Inspect the generated stage graph JSON."},{"action":"semantic_map","intent":"Jump from a stage anchor to code symbols."}]}
Notes:
- `out` defaults to `state/orchestrator/stage_graph.json`.

## `semantic_map` — rustc-backed repomap: symbol outline by file (kind, name, signature); set expand_bodies:true (with filter) to inline all bodies in a module

Examples:
  {"action":"semantic_map","crate":"canon_mini_agent","rationale":"Get a compiler-backed symbol outline before exploring files."}
  {"action":"semantic_map","crate":"canon_mini_agent","filter":"tools","rationale":"Restrict to the tools module."}
  {"action":"semantic_map","crate":"canon_mini_agent","filter":"tools","expand_bodies":true,"rationale":"Read every symbol body in the tools module in one pass."}
Notes: symbol paths are module-relative (e.g. `tools::my_fn`). Crate-qualified prefixes like `canon_mini_agent::tools` or `crate::tools` are accepted and stripped. `filter` is an optional path prefix; use `expand_bodies` with `filter` to avoid oversized output.

## `symbol_window` — extract the full definition body of a symbol (byte-precise, via def span)

Example:
  {"action":"symbol_window","crate":"canon_mini_agent","symbol":"tools::execute_logged_action","rationale":"Read the exact body of a function before editing it."}
Notes: accepts short unambiguous suffix if the full module path is unknown.

## `symbol_refs` — list all reference sites for a symbol; set expand_bodies:true to also show each enclosing function/struct/trait body (like symbol_window)

Example (sites only):
  {"action":"symbol_refs","crate":"canon_mini_agent","symbol":"tools::execute_logged_action","rationale":"Find all call sites before changing a signature."}
Example (with bodies):
  {"action":"symbol_refs","crate":"canon_mini_agent","symbol":"app::run_agent","expand_bodies":true,"rationale":"Read every caller body to understand the call contract before refactoring."}
Notes: covers every identifier span recorded by the HIR visitor during compilation. expand_bodies finds the tightest enclosing symbol in the graph and inlines its source.

## `symbol_path` — BFS shortest call-graph path between two symbols; set expand_bodies:true to inline the source body of each hop

Example:
  {"action":"symbol_path","crate":"canon_mini_agent","from":"app::run_agent","to":"tools::handle_apply_patch_action","rationale":"Trace how a high-level entry point reaches a specific handler."}
Example (with bodies):
  {"action":"symbol_path","crate":"canon_mini_agent","from":"app::run_agent","to":"tools::handle_apply_patch_action","expand_bodies":true,"rationale":"Read every function along the call chain before changing a handler signature."}
Notes: uses static call edges only; returns path with file:line annotations.

## `symbol_neighborhood` — immediate callers and callees of a symbol; set expand_bodies:true to inline the source body of each caller and callee

Example:
  {"action":"symbol_neighborhood","crate":"canon_mini_agent","symbol":"tools::execute_logged_action","rationale":"Understand the blast radius of a function before modifying it."}
Example (with bodies):
  {"action":"symbol_neighborhood","crate":"canon_mini_agent","symbol":"tools::execute_logged_action","expand_bodies":true,"rationale":"Read every caller and callee body before refactoring."}
Notes: returns all direct callers and callees from the static call graph.

## `batch` — execute up to 8 non-mutating actions in one turn; results returned as labeled sections

Example (read multiple files before patching):
  {"action":"batch","rationale":"Gather all context needed before forming a patch.","predicted_next_actions":[{"action":"apply_patch","intent":"Apply the fix after reading all relevant code."},{"action":"cargo_test","intent":"Confirm fix compiles and tests pass."}],"actions":[{"action":"read_file","path":"src/app.rs","line":1800},{"action":"symbol_window","crate":"canon_mini_agent","symbol":"app::apply_wake_flags"},{"action":"symbol_neighborhood","crate":"canon_mini_agent","symbol":"app::apply_wake_flags"}]}
Example (survey multiple modules):
  {"action":"batch","rationale":"Map the relevant modules before a cross-cutting change.","predicted_next_actions":[{"action":"semantic_map","intent":"Drill into a specific module after surveying."}],"actions":[{"action":"semantic_map","crate":"canon_mini_agent","filter":"tools"},{"action":"semantic_map","crate":"canon_mini_agent","filter":"app"},{"action":"list_dir","path":"state"}]}
Rules:
- Max 8 items per batch.
- Mutating actions (apply_patch, rename_symbol, message, run_command, python, cargo_test) are rejected.
- For plan: only op=sorted_view is accepted.
- For objectives: only op=read or op=sorted_view.
- For issue: only op=read.
- Items must omit rationale, predicted_next_actions, and observation.
- On per-item error the item is labeled [batch N/M: ERROR] and execution continues.

