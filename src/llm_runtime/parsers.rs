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
        Err(_) => {
            if let Some(fenced) = extract_fenced_json(data) {
                return FrameResult::Snapshot(fenced);
            }
            return FrameResult::Ignore;
        }
    };
    if let Some(fenced) = find_fenced_json_in_value(&v) {
        return FrameResult::Snapshot(fenced);
    }
    let obj = match v.as_object() {
        Some(o) => o,
        None => return FrameResult::Ignore,
    };
    if obj.get("type").and_then(|t| t.as_str()) == Some("message_stream_complete") {
        return FrameResult::Done;
    }
    if obj.get("type").and_then(|t| t.as_str()) == Some("message") {
        let text = obj
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !text.is_empty() {
            return FrameResult::Snapshot(text.to_string());
        }
    }
    if obj.get("object").and_then(|v| v.as_str()) == Some("chat.completion.chunk") {
        let content = obj
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|v| v.as_str());
        return match content {
            Some(s) if !s.is_empty() => FrameResult::ExecutionDelta(s.to_string()),
            _ => FrameResult::Ignore,
        };
    }
    if let Some(delta) = obj.get("delta").and_then(|d| d.as_str()) {
        if !delta.is_empty() {
            return FrameResult::ExecutionDelta(delta.to_string());
        }
    }
    if let Some(op) = obj.get("o").and_then(|o| o.as_str()) {
        match op {
            "append" => {
                let p = obj.get("p").and_then(|p| p.as_str()).unwrap_or("");
                if p.contains("parts") {
                    if let Some(s) = obj.get("v").and_then(|v| v.as_str()) {
                        if !s.is_empty() {
                            return FrameResult::ExecutionDelta(s.to_string());
                        }
                    }
                }
            }
            "patch" => {
                if let Some(arr) = obj.get("v").and_then(|v| v.as_array()) {
                    let mut out = String::new();
                    for item in arr {
                        if item.get("o").and_then(|o| o.as_str()) != Some("append") {
                            continue;
                        }
                        let p = item.get("p").and_then(|p| p.as_str()).unwrap_or("");
                        if p.contains("parts") {
                            if let Some(s) = item.get("v").and_then(|v| v.as_str()) {
                                out.push_str(s);
                            }
                        }
                    }
                    if !out.is_empty() {
                        return FrameResult::ExecutionDelta(out);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(text) = obj.get("v").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            return FrameResult::ExecutionDelta(text.to_string());
        }
    }
    if let Some(arr) = obj.get("v").and_then(|v| v.as_array()) {
        let mut out = String::new();
        for item in arr {
            if item.get("o").and_then(|o| o.as_str()) != Some("append") {
                continue;
            }
            let p = item.get("p").and_then(|p| p.as_str()).unwrap_or("");
            if p.contains("parts") {
                if let Some(s) = item.get("v").and_then(|v| v.as_str()) {
                    out.push_str(s);
                }
            }
        }
        if !out.is_empty() {
            return FrameResult::ExecutionDelta(out);
        }
    }
    FrameResult::Ignore
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
    enum GeminiNode<'a> {
        Borrowed(&'a Value),
        Owned(Value),
    }
    let mut stack: Vec<(GeminiNode<'_>, usize)> = vec![(GeminiNode::Borrowed(v), depth)];
    while let Some((node, depth)) = stack.pop() {
        if depth > 12 {
            continue;
        }
        match node {
            GeminiNode::Borrowed(current) => match current {
                Value::String(s) => {
                    if s.starts_with('{') || s.starts_with('[') {
                        if let Ok(inner) = serde_json::from_str::<Value>(s) {
                            stack.push((GeminiNode::Owned(inner), depth + 1));
                            continue;
                        }
                    }
                    if s.contains("```json") {
                        out.push_str(s);
                    }
                }
                Value::Array(arr) => {
                    for item in arr.iter().rev() {
                        stack.push((GeminiNode::Borrowed(item), depth + 1));
                    }
                }
                Value::Object(map) => {
                    for val in map.values() {
                        stack.push((GeminiNode::Borrowed(val), depth + 1));
                    }
                }
                _ => {}
            },
            GeminiNode::Owned(current) => match &current {
                Value::String(s) => {
                    if s.starts_with('{') || s.starts_with('[') {
                        if let Ok(inner) = serde_json::from_str::<Value>(s) {
                            stack.push((GeminiNode::Owned(inner), depth + 1));
                            continue;
                        }
                    }
                    if s.contains("```json") {
                        out.push_str(s);
                    }
                }
                Value::Array(arr) => {
                    for item in arr.iter().rev() {
                        stack.push((GeminiNode::Owned(item.clone()), depth + 1));
                    }
                }
                Value::Object(map) => {
                    for val in map.values() {
                        stack.push((GeminiNode::Owned(val.clone()), depth + 1));
                    }
                }
                _ => {}
            },
        }
    }
}
fn classify_calpico_value(v: &Value) -> FrameResult {
    if let Some(arr) = v.as_array() {
        return classify_calpico_array(arr);
    }
    let Some(obj) = v.as_object() else {
        return FrameResult::Ignore;
    };
    if let Some(chunk) = obj.get("chunk").and_then(|c| c.as_str()) {
        return classify_chatgpt_group(chunk);
    }
    if let Some(items) = obj.get("items").and_then(|i| i.as_array()) {
        let result = classify_calpico_items(items);
        if !matches!(result, FrameResult::Ignore) {
            return result;
        }
    }
    if obj.get("type").and_then(|t| t.as_str()) == Some("message") {
        return classify_calpico_envelope(v);
    }
    if obj.get("role").and_then(|r| r.as_str()) == Some("assistant")
        && obj.get("raw_messages").is_some()
    {
        return classify_calpico_message(v);
    }
    FrameResult::Ignore
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

        let Some(role) = item.get("role").and_then(|r| r.as_str()) else {
            continue;
        };
        if role != "assistant" {
            continue;
        }

        let text = item
            .get("content")
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if !text.is_empty() {
            return FrameResult::Snapshot(text.to_string());
        }
    }
    FrameResult::Ignore
}

fn classify_calpico_envelope(envelope: &Value) -> FrameResult {
    if envelope.get("type").and_then(|t| t.as_str()) != Some("message") {
        return FrameResult::Ignore;
    }
    let payload = match envelope.get("payload") {
        Some(p) => p,
        None => return FrameResult::Ignore,
    };
    if payload.get("type").and_then(|t| t.as_str()) == Some("calpico-message-update") {
        let msg = match payload.get("payload").and_then(|p| p.get("message")) {
            Some(m) => m,
            None => return FrameResult::Ignore,
        };
        let assistant_reaction = msg
            .get("reactions")
            .and_then(|r| r.get("assistant"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !assistant_reaction.is_empty() {
            return FrameResult::Snapshot(format!(
                "assistant reaction-only terminal frame: {}",
                assistant_reaction
            ));
        }
        return FrameResult::Ignore;
    }
    if payload.get("type").and_then(|t| t.as_str()) != Some("calpico-message-add") {
        return FrameResult::Ignore;
    }
    let msg = match payload.get("payload").and_then(|p| p.get("message")) {
        Some(m) => m,
        None => return FrameResult::Ignore,
    };
    classify_calpico_message(msg)
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
        let author_role = raw_msg
            .get("author")
            .and_then(|a| a.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if author_role != "assistant" {
            continue;
        }
        saw_assistant = true;
        let channel = raw_msg
            .get("channel")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if channel != "final" {
            continue;
        }
        let text = raw_msg
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !text.is_empty() {
            return FrameResult::Snapshot(text.to_string());
        }
        saw_empty = true;
    }
    if saw_assistant && saw_empty {
        return FrameResult::Snapshot("LLM error: empty assistant response body".to_string());
    }
    FrameResult::Ignore
}
fn find_fenced_json_in_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => extract_fenced_json(s),
        Value::Array(arr) => arr.iter().find_map(find_fenced_json_in_value),
        Value::Object(map) => map.values().find_map(find_fenced_json_in_value),
        _ => None,
    }
}
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
