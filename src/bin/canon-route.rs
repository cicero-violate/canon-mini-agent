use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Read;

fn take_flag_value(args: &[String], name: &str) -> Option<String> {
    let mut i = 0usize;
    while i + 1 < args.len() {
        if args[i] == name {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn usage() -> &'static str {
    "canon-route: wrap a result/summary into a message action (stdin JSON -> stdout JSON)\n\
\n\
Usage:\n\
  canon-route --role <role> [--to <role>] [--type <msg_type>] [--status <status>]\n\
\n\
Input (stdin): JSON object. Common forms:\n\
  1) {\"summary\":\"...\",\"payload\":{...}}\n\
  2) {\"ok\":true,\"done\":false,\"output\":\"...\"}\n\
  3) any tool action or wrapper; it will be embedded into payload\n\
\n\
Output (stdout): a tool action JSON:\n\
  {\"action\":\"message\",\"from\":\"...\",\"to\":\"...\",\"type\":\"...\",\"status\":\"...\",\"payload\":{...}}\n"
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if handle_help(&args) {
        return Ok(());
    }

    let role = take_flag_value(&args, "--role").context("missing --role")?;
    let input = read_input_json()?;

    let (from, to, msg_type, status) = resolve_route_fields(&args, &role);

    let payload = build_route_payload(input);

    emit_message(from, to, msg_type, status, payload)?;
    Ok(())
}

fn handle_help(args: &[String]) -> bool {
    if has_flag(args, "--help") || has_flag(args, "-h") {
        eprint!("{}", usage());
        return true;
    }
    false
}

fn read_input_json() -> Result<Value> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).context("read stdin")?;
    let input: Value = serde_json::from_str(&raw).context("stdin is not valid JSON")?;
    Ok(input)
}

fn resolve_route_fields(args: &[String], role: &str) -> (String, String, String, String) {
    let (from, default_to, default_type, default_status) =
        canon_mini_agent::invalid_action::default_message_route(role);

    let to = take_flag_value(args, "--to").unwrap_or_else(|| default_to.to_string());
    let msg_type = take_flag_value(args, "--type").unwrap_or_else(|| default_type.to_string());
    let status = take_flag_value(args, "--status").unwrap_or_else(|| default_status.to_string());

    (from.to_string(), to, msg_type, status)
}

fn emit_message(
    from: String,
    to: String,
    msg_type: String,
    status: String,
    payload: Value,
) -> Result<()> {
    let msg = json!({
        "action": "message",
        "from": from,
        "to": to,
        "type": msg_type,
        "status": status,
        "payload": payload
    });

    println!("{}", serde_json::to_string_pretty(&msg)?);
    Ok(())
}

fn build_route_payload(input: Value) -> Value {
    let summary = input
        .get("summary")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            input.get("output")
                .and_then(|v| v.as_str())
                .map(|s| s.lines().next().unwrap_or("").trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pipeline message".to_string());

    match input.get("payload") {
        Some(p) if p.is_object() => p.clone(),
        _ => json!({ "summary": summary, "input": input }),
    }
}
