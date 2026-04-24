use crate::llm_runtime::llm_domains::{is_chatgpt_gg_url, is_chatgpt_url, is_gemini_url};
use serde_json::Value;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteType {
    ChatGptPrivate,
    ChatGptGroup,
    Gemini,
    Unknown,
}
impl SiteType {
    pub fn from_url(url: &str) -> Self {
        if is_chatgpt_gg_url(url) {
            return SiteType::ChatGptGroup;
        }
        if is_chatgpt_url(url) {
            return SiteType::ChatGptPrivate;
        }
        if is_gemini_url(url) {
            return SiteType::Gemini;
        }
        SiteType::Unknown
    }
}
/// The result of parsing one inbound frame.
#[derive(Debug)]
pub enum FrameResult {
    ExecutionDelta(String),
    Snapshot(String),
    Done,
    Ignore,
}
pub fn classify_frame(site: SiteType, raw: &str) -> FrameResult {
    match site {
        SiteType::ChatGptPrivate => classify_chatgpt_private(raw),
        SiteType::ChatGptGroup => classify_chatgpt_group(raw),
        SiteType::Gemini => classify_gemini(raw),
        SiteType::Unknown => FrameResult::Ignore,
    }
}
pub struct FrameAssembler {
    site: SiteType,
    deltas: Vec<String>,
    raw: String,
}
impl FrameAssembler {
    pub fn new(site: SiteType) -> Self {
        Self {
            site,
            deltas: Vec::new(),
            raw: String::new(),
        }
    }
    /// Intent: canonical_write
    /// Resource: error
    /// Inputs: &mut llm_runtime::parsers::FrameAssembler, llm_runtime::parsers::SiteType
    /// Outputs: ()
    /// Effects: error
    /// Forbidden: error
    /// Invariants: error
    /// Failure: error
    /// Provenance: rustc:facts + rustc:docstring
    pub fn set_site(&mut self, site: SiteType) {
        self.site = site;
    }
    pub fn reset(&mut self) {
        self.deltas.clear();
        self.raw.clear();
    }
    pub fn push(&mut self, payload: &str) -> Option<String> {
        match self.site {
            SiteType::Gemini => match classify_frame(self.site, payload) {
                FrameResult::Snapshot(text) => {
                    self.raw.clear();
                    self.raw.push_str(&text);
                    if let Some(fenced) = try_extract_complete_fenced_json(&self.raw) {
                        self.reset();
                        return Some(fenced);
                    }
                    None
                }
                FrameResult::ExecutionDelta(text) => {
                    self.raw.push_str(&text);
                    if let Some(fenced) = try_extract_complete_fenced_json(&self.raw) {
                        self.reset();
                        return Some(fenced);
                    }
                    None
                }
                _ => None,
            },
            _ => match classify_frame(self.site, payload) {
                FrameResult::ExecutionDelta(text) => {
                    self.deltas.push(text);
                    None
                }
                FrameResult::Snapshot(text) => {
                    self.reset();
                    Some(text)
                }
                FrameResult::Done => {
                    let assembled = self.deltas.join("");
                    self.reset();
                    if assembled.is_empty() {
                        None
                    } else {
                        Some(assembled)
                    }
                }
                FrameResult::Ignore => None,
            },
        }
    }
}
fn classify_chatgpt_private(raw: &str) -> FrameResult {
    let data = raw.strip_prefix("data: ").unwrap_or(raw).trim();
    if data.is_empty() {
        return FrameResult::Ignore;
    }
    if data == "[DONE]" {
        return FrameResult::Done;
    }
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return fallback_fenced_snapshot_or_ignore(data),
    };
    if let Some(fenced) = find_fenced_json_in_value(&v) {
        return FrameResult::Snapshot(fenced);
    }
    let obj = match v.as_object() {
        Some(o) => o,
        None => return FrameResult::Ignore,
    };
    if chatgpt_private_is_done(obj) {
        return FrameResult::Done;
    }
    if let Some(text) = chatgpt_private_snapshot(obj) {
        return FrameResult::Snapshot(text);
    }
    if let Some(delta) = chatgpt_private_delta(obj) {
        return FrameResult::ExecutionDelta(delta);
    }
    FrameResult::Ignore
}

fn fallback_fenced_snapshot_or_ignore(data: &str) -> FrameResult {
    extract_fenced_json(data)
        .map(FrameResult::Snapshot)
        .unwrap_or(FrameResult::Ignore)
}

fn chatgpt_private_is_done(obj: &serde_json::Map<String, Value>) -> bool {
    obj.get("type").and_then(|t| t.as_str()) == Some("message_stream_complete")
}

fn chatgpt_private_snapshot(obj: &serde_json::Map<String, Value>) -> Option<String> {
    if obj.get("type").and_then(|t| t.as_str()) != Some("message") {
        return None;
    }
    obj.get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

fn chatgpt_private_delta(obj: &serde_json::Map<String, Value>) -> Option<String> {
    chatgpt_completion_chunk_delta(obj)
        .or_else(|| non_empty_json_str(obj.get("delta")))
        .or_else(|| chatgpt_operation_delta(obj))
        .or_else(|| non_empty_json_str(obj.get("v")))
        .or_else(|| chatgpt_append_array_delta(obj.get("v")?))
}

fn chatgpt_completion_chunk_delta(obj: &serde_json::Map<String, Value>) -> Option<String> {
    if obj.get("object").and_then(|v| v.as_str()) != Some("chat.completion.chunk") {
        return None;
    }
    obj.get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("delta"))
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
        .map(ToString::to_string)
}

fn chatgpt_operation_delta(obj: &serde_json::Map<String, Value>) -> Option<String> {
    match obj.get("o").and_then(|o| o.as_str()) {
        Some("append") => chatgpt_append_delta(obj),
        Some("patch") => chatgpt_append_array_delta(obj.get("v")?),
        _ => None,
    }
}

fn chatgpt_append_delta(obj: &serde_json::Map<String, Value>) -> Option<String> {
    obj.get("p")
        .and_then(Value::as_str)
        .filter(|path| path.contains("parts"))?;
    non_empty_json_str(obj.get("v"))
}

fn chatgpt_append_array_delta(value: &Value) -> Option<String> {
    let mut out = String::new();
    for item in value.as_array()? {
        if item.get("o").and_then(|o| o.as_str()) != Some("append") {
            continue;
        }
        let path = item.get("p").and_then(|p| p.as_str()).unwrap_or("");
        if !path.contains("parts") {
            continue;
        }
        if let Some(fragment) = item.get("v").and_then(Value::as_str) {
            out.push_str(fragment);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn non_empty_json_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

fn classify_chatgpt_group(raw: &str) -> FrameResult {
    let data = raw.strip_prefix("data: ").unwrap_or(raw).trim();
    if data.is_empty() {
        return FrameResult::Ignore;
    }
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return FrameResult::Ignore,
    };
    classify_calpico_value(&v)
}
fn classify_gemini(raw: &str) -> FrameResult {
    let data = raw.strip_prefix("data: ").unwrap_or(raw).trim();
    if data.is_empty() {
        return FrameResult::Ignore;
    }
    let data = strip_leading_length_line(data);
    if let Ok(v) = serde_json::from_str::<Value>(data) {
        let mut out = String::new();
        collect_gemini_fragments(&v, &mut out, 0);
        if !out.is_empty() {
            if out.contains("```json") {
                return FrameResult::Snapshot(out);
            }
            return FrameResult::ExecutionDelta(out);
        }
    } else if let Some(fenced) = extract_fenced_json(data) {
        return FrameResult::Snapshot(fenced);
    }
    FrameResult::Ignore
}
fn strip_leading_length_line(input: &str) -> &str {
    let digit_prefix_len: usize = input
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .map(char::len_utf8)
        .sum();
    if digit_prefix_len == 0 {
        return input;
    }
    let rest = &input[digit_prefix_len..];
    if rest.starts_with('\n') || rest.starts_with('\r') {
        return rest.trim_start_matches(['\r', '\n']);
    }
    input
}
fn collect_gemini_fragments(v: &Value, out: &mut String, depth: usize) {
    let mut stack: Vec<(Value, usize)> = vec![(v.clone(), depth)];
    while let Some((current, depth)) = stack.pop() {
        if depth > 12 {
            continue;
        }
        match current {
            Value::String(s) => {
                if s.starts_with('{') || s.starts_with('[') {
                    if let Ok(inner) = serde_json::from_str::<Value>(&s) {
                        stack.push((inner, depth + 1));
                        continue;
                    }
                }
                if s.contains("```json") {
                    out.push_str(&s);
                }
            }
            Value::Array(arr) => {
                for item in arr.into_iter().rev() {
                    stack.push((item, depth + 1));
                }
            }
            Value::Object(map) => {
                for (_, val) in map {
                    stack.push((val, depth + 1));
                }
            }
            _ => {}
        }
    }
}
fn classify_chunk_object(obj: &serde_json::Map<String, Value>) -> Option<FrameResult> {
    obj.get("chunk")
        .and_then(|c| c.as_str())
        .map(classify_chatgpt_group)
}

fn is_assistant_raw_message(obj: &serde_json::Map<String, Value>) -> bool {
    obj.get("role").and_then(|r| r.as_str()) == Some("assistant")
        && obj.get("raw_messages").is_some()
}

fn classify_calpico_object(obj: &serde_json::Map<String, Value>, value: &Value) -> FrameResult {
    if let Some(result) = classify_chunk_object(obj) {
        return result;
    }
    if let Some(result) = classify_calpico_items_object(obj) {
        return result;
    }
    if calpico_object_is_message_envelope(obj) {
        return classify_calpico_envelope(value);
    }
    if is_assistant_raw_message(obj) {
        return classify_calpico_message(value);
    }
    FrameResult::Ignore
}

fn classify_calpico_items_object(obj: &serde_json::Map<String, Value>) -> Option<FrameResult> {
    let items = obj.get("items")?.as_array()?;
    match classify_calpico_items(items) {
        FrameResult::Ignore => None,
        result => Some(result),
    }
}

fn calpico_object_is_message_envelope(obj: &serde_json::Map<String, Value>) -> bool {
    obj.get("type").and_then(|t| t.as_str()) == Some("message")
}

fn classify_calpico_value(v: &Value) -> FrameResult {
    if let Some(arr) = v.as_array() {
        return classify_calpico_array(arr);
    }
    let Some(obj) = v.as_object() else {
        return FrameResult::Ignore;
    };
    classify_calpico_object(obj, v)
}

fn classify_calpico_array(arr: &[Value]) -> FrameResult {
    for envelope in arr {
        let result = classify_calpico_envelope(envelope);
        if !matches!(result, FrameResult::Ignore) {
            return result;
        }
    }
    FrameResult::Ignore
}

fn classify_calpico_items(items: &[Value]) -> FrameResult {
    for item in items {
        let result = classify_calpico_envelope(item);
        if !matches!(result, FrameResult::Ignore) {
            return result;
        }
        if let Some(text) = assistant_item_text(item) {
            return FrameResult::Snapshot(text.to_string());
        }
    }
    FrameResult::Ignore
}

fn assistant_item_text(item: &Value) -> Option<&str> {
    if item.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return None;
    }
    let text = item
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())?;
    if text.is_empty() {
        return None;
    }
    Some(text)
}

fn calpico_message_from_payload(payload: &Value) -> Option<&Value> {
    payload.get("payload")?.get("message")
}

fn classify_calpico_update_payload(payload: &Value) -> Option<FrameResult> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("calpico-message-update") {
        return None;
    }
    let msg = calpico_message_from_payload(payload)?;
    let assistant_reaction = msg
        .get("reactions")
        .and_then(|r| r.get("assistant"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if assistant_reaction.is_empty() {
        return Some(FrameResult::Ignore);
    }
    Some(FrameResult::Snapshot(format!(
        "assistant reaction-only terminal frame: {}",
        assistant_reaction
    )))
}

fn classify_calpico_add_payload(payload: &Value) -> Option<FrameResult> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("calpico-message-add") {
        return None;
    }
    Some(classify_calpico_message(calpico_message_from_payload(
        payload,
    )?))
}

fn classify_calpico_envelope(envelope: &Value) -> FrameResult {
    if envelope.get("type").and_then(|t| t.as_str()) != Some("message") {
        return FrameResult::Ignore;
    }
    let payload = match envelope.get("payload") {
        Some(p) => p,
        None => return FrameResult::Ignore,
    };
    if let Some(result) = classify_calpico_update_payload(payload) {
        return result;
    }
    if let Some(result) = classify_calpico_add_payload(payload) {
        return result;
    }
    FrameResult::Ignore
}

fn assistant_final_message_text(raw_msg: &Value) -> Option<Option<&str>> {
    let author_role = raw_msg
        .get("author")
        .and_then(|a| a.get("role"))
        .and_then(|r| r.as_str())?;
    if author_role != "assistant" {
        return None;
    }
    let channel = raw_msg
        .get("channel")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if channel != "final" {
        return Some(None);
    }
    let text = raw_msg
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Some(Some(text))
}

fn classify_calpico_message(msg: &Value) -> FrameResult {
    if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return FrameResult::Ignore;
    }
    let mut saw_assistant = false;
    let mut saw_empty = false;
    let raw_messages = match msg.get("raw_messages").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return FrameResult::Ignore,
    };
    for raw_msg in raw_messages {
        match assistant_final_message_text(raw_msg) {
            Some(Some(text)) if !text.is_empty() => {
                return FrameResult::Snapshot(text.to_string());
            }
            Some(Some(_)) => {
                saw_assistant = true;
                saw_empty = true;
            }
            Some(None) => {
                saw_assistant = true;
            }
            None => {}
        }
    }
    if saw_assistant && saw_empty {
        return FrameResult::Snapshot("LLM error: empty assistant response body".to_string());
    }
    FrameResult::Ignore
}
/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn find_fenced_json_in_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => extract_fenced_json(s),
        Value::Array(arr) => arr.iter().find_map(find_fenced_json_in_value),
        Value::Object(map) => map.values().find_map(find_fenced_json_in_value),
        _ => None,
    }
}
/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_fenced_json(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let rest = &text[start..];
    let end = rest.rfind("```")?;
    if end <= 6 {
        return None;
    }
    Some(rest[..end + 3].to_string())
}
pub fn try_extract_complete_fenced_json(raw: &str) -> Option<String> {
    let start = raw.find("```json")?;
    let after_fence = &raw[start + 7..];
    let end = after_fence.rfind("```")?;
    if end == 0 {
        return None;
    }
    let inner = after_fence[..end].trim();
    if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
        return Some(format!("```json\n{}\n```", inner));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{classify_frame, FrameResult, SiteType};

    #[test]
    fn chatgpt_group_parses_calpico_message_add_array() {
        let raw = r#"[{"type":"message","payload":{"type":"calpico-message-add","payload":{"message":{"role":"assistant","raw_messages":[{"author":{"role":"assistant"},"channel":"final","content":{"parts":["```json\n{\"action\":\"issue\"}\n```"]}}]}}}}]"#;
        match classify_frame(SiteType::ChatGptGroup, raw) {
            FrameResult::Snapshot(text) => assert!(text.contains(r#""action":"issue""#)),
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[test]
    fn chatgpt_group_parses_wrapped_chunk_payload() {
        let raw = r#"{"turn_id":22,"chunk":"[{\"type\":\"message\",\"payload\":{\"type\":\"calpico-message-add\",\"payload\":{\"message\":{\"role\":\"assistant\",\"raw_messages\":[{\"author\":{\"role\":\"assistant\"},\"channel\":\"final\",\"content\":{\"parts\":[\"```json\\n{\\\"action\\\":\\\"apply_patch\\\"}\\n```\"]}}]}}}}]"}"#;
        match classify_frame(SiteType::ChatGptGroup, raw) {
            FrameResult::Snapshot(text) => assert!(text.contains(r#""action":"apply_patch""#)),
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[test]
    fn chatgpt_group_parses_direct_assistant_message_object() {
        let raw = r#"{"id":"msg","role":"assistant","raw_messages":[{"author":{"role":"assistant"},"channel":"final","content":{"parts":["```json\n{\"action\":\"message\"}\n```"]}}]}"#;
        match classify_frame(SiteType::ChatGptGroup, raw) {
            FrameResult::Snapshot(text) => assert!(text.contains(r#""action":"message""#)),
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[test]
    fn chatgpt_group_parses_room_read_assistant_snapshot_items() {
        let raw = r#"{"items":[{"id":"msg","role":"assistant","content":{"text":"```json\n{\"action\":\"python\"}\n```"}}]}"#;
        match classify_frame(SiteType::ChatGptGroup, raw) {
            FrameResult::Snapshot(text) => assert!(text.contains(r#""action":"python""#)),
            other => panic!("expected snapshot, got {other:?}"),
        }
    }
}
