//! ACP `session/update` to AgentMessage (TS `AcpMessageHandler.ts`).
use super::{
    filters::{is_internal_event_json, parse_rate_limit_text, RateLimit},
    tool_names::{
        canonicalize_diff_tool_input, derive_tool_name_with_source, is_placeholder_tool_name,
        DerivedToolName, ToolNameSource,
    },
    types::{AgentMessage, ToolCall},
};
use crate::rpc::handlers::GeneratedImages;
use base64::Engine;
use serde_json::{json, Map, Value};
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

const REASONING_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(250);
const TEXT_IDLE_FLUSH_MS: u64 = 1000;
const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextChunkMode {
    #[default]
    Dedupe,
    Delta,
}

#[derive(Clone, Debug, Default)]
pub struct MessageHandlerOptions {
    pub text_chunk_mode: TextChunkMode,
    pub flavor: Option<String>,
    /// `None`: 1000 ms; `Some(0)` disables the idle flush.
    pub text_idle_flush_ms: Option<u64>,
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}
fn nonempty<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    str_of(v, key).filter(|s| !s.is_empty())
}

fn normalize_status(status: &Value) -> String {
    match status.as_str() {
        Some(s @ ("in_progress" | "completed" | "failed")) => s.into(),
        _ => "pending".into(),
    }
}

/// `rawInput: {}` (OpenCode) carries no usable arguments.
fn usable(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Object(m)) => !m.is_empty(),
        Some(_) => true,
    }
}

fn normalize_tool_kind(kind: Option<&str>) -> Option<&'static str> {
    let k = kind?.trim().to_lowercase();
    Some(match k.as_str() {
        "read" | "read_file" | "file_read" | "view" => "read",
        "execute" | "shell" | "bash" | "run" | "run_shell" | "run_shell_command" | "cmd"
        | "terminal" => "execute",
        "search" | "grep" | "find" | "glob" => "search",
        "edit" | "write" | "write_file" | "replace" | "file_edit" | "modify" => "edit",
        "think" | "thought" | "reasoning" => "think",
        _ => return None,
    })
}

/// `Category: argument` titles (Kimi) keep only the argument when the label
/// names the same tool kind.
fn extract_title_argument<'a>(title: &'a str, kind: Option<&str>) -> &'a str {
    let Some((label, rest)) = title.split_once(':') else {
        return title;
    };
    let mut chars = label.chars();
    let label_ok = label.len() <= 32
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphabetic() || matches!(c, ' ' | '_' | '-'));
    if !label_ok || !rest.starts_with(char::is_whitespace) {
        return title;
    }
    let arg = rest.trim_start();
    if arg.is_empty() || arg.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return title;
    }
    match normalize_tool_kind(Some(label)) {
        Some(l) if Some(l) == normalize_tool_kind(kind) => arg,
        _ => title,
    }
}

fn derive_input_from_kind_and_title(
    kind: Option<&str>,
    title: Option<&str>,
    locations: Option<&Value>,
) -> Option<Value> {
    let normalized = normalize_tool_kind(kind);
    if normalized == Some("edit") {
        let path = locations
            .and_then(|l| l.get(0))
            .and_then(|l| str_of(l, "path"))?;
        return Some(json!({ "file_path": path }));
    }
    let arg = extract_title_argument(title.filter(|t| !t.is_empty())?, kind);
    match normalized {
        Some("read") => Some(json!({ "file_path": arg })),
        Some("execute") => Some(json!({ "command": arg })),
        Some("search") => Some(json!({ "pattern": arg })),
        _ => None,
    }
}

/// Kimi streams tool arguments as JSON text in a single content block.
fn extract_json_input_from_content(content: Option<&Value>) -> Option<Value> {
    let blocks = content?.as_array().filter(|a| a.len() == 1)?;
    let block = &blocks[0];
    if block["type"] != "content" || block["content"]["type"] != "text" {
        return None;
    }
    let text = block["content"]["text"].as_str()?.trim();
    if !text.starts_with('{') && !text.starts_with('[') {
        return None;
    }
    crate::jsjson::parse::<Value>(text.as_bytes())
        .ok()
        .filter(|v| v.is_object() || v.is_array())
}

fn resolve_input_fallbacks(update: &Value, title: Option<&str>) -> Option<Value> {
    let kind = str_of(update, "kind");
    derive_input_from_kind_and_title(kind, title, update.get("locations"))
        .or_else(|| extract_json_input_from_content(update.get("content")))
}

fn meta_kind_from_content(content: Option<&Value>) -> Option<&str> {
    let first = content?.as_array()?.first()?;
    if first["type"] != "diff" {
        return None;
    }
    first["_meta"]["kind"].as_str()
}

fn derive_name_from_update(update: &Value) -> DerivedToolName {
    derive_tool_name_with_source(
        str_of(update, "title"),
        str_of(update, "kind"),
        update.get("rawInput"),
        meta_kind_from_content(update.get("content")),
    )
}

fn is_stale_derived_input(existing: &Value, title: Option<&str>, kind: Option<&str>) -> bool {
    let Some(title) = title.filter(|t| !t.is_empty()) else {
        return false;
    };
    let arg = extract_title_argument(title, kind);
    if arg == title {
        return false;
    }
    let Some(object) = existing.as_object() else {
        return false;
    };
    !object
        .values()
        .any(|v| v.as_str().is_some_and(|s| s.trim() == arg))
}

fn hoist_diff_content(content: Option<&Value>) -> Option<(String, Value)> {
    let first = content?.as_array()?.first()?;
    if first["type"] != "diff" {
        return None;
    }
    let path = first["path"].as_str()?;
    let text = |k: &str| first[k].as_str().unwrap_or("").to_owned();
    match first["_meta"]["kind"].as_str() {
        Some("add") => Some((
            "Write".into(),
            json!({"file_path":path,"content":text("newText")}),
        )),
        Some("modify") => Some((
            "Edit".into(),
            json!({"file_path":path,"old_string":text("oldText"),"new_string":text("newText")}),
        )),
        _ => None,
    }
}

fn extract_audience(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => out.extend(a.iter().filter_map(Value::as_str).map(str::to_owned)),
        _ => {}
    }
}

fn explicit_audience(annotations: &Value) -> Vec<String> {
    fn object(entry: &Value, out: &mut Vec<String>) {
        extract_audience(&entry["audience"], out);
        if entry["value"].is_object() {
            extract_audience(&entry["value"]["audience"], out);
        }
    }
    let mut out = Vec::new();
    match annotations {
        Value::Array(entries) => {
            for entry in entries {
                match entry {
                    Value::String(s) => out.push(s.clone()),
                    Value::Object(_) => object(entry, &mut out),
                    _ => {}
                }
            }
        }
        Value::Object(_) => object(annotations, &mut out),
        _ => {}
    }
    out
}

fn extract_text_content(block: &Value) -> Option<&str> {
    if block["type"] != "text" {
        return None;
    }
    let audience = explicit_audience(&block["annotations"]);
    if !audience.is_empty() && !audience.iter().any(|a| a == "assistant") {
        return None;
    }
    block["text"].as_str()
}

/// ACP tool content (`content` / `diff` blocks) collapsed to a string or a
/// single diff object; `None` when it cannot be collapsed safely.
fn normalize_tool_content(content: Option<&Value>) -> Option<Value> {
    let blocks = content?.as_array()?;
    if blocks.is_empty() {
        return Some(json!(""));
    }
    let (mut diff, mut texts) = (None, Vec::new());
    for block in blocks {
        if !block.is_object() {
            return None;
        }
        if block["type"] == "diff" {
            if diff.is_some() {
                return None;
            }
            let mut d = Map::new();
            for key in ["path", "oldText", "newText"] {
                if let Some(s) = block[key].as_str() {
                    d.insert(key.into(), json!(s));
                }
            }
            if let Some(k) = block["_meta"]["kind"].as_str() {
                d.insert("kind".into(), json!(k));
            }
            diff = Some(Value::Object(d));
        } else if block["type"] == "content" && block["content"].is_object() {
            let inner = &block["content"];
            texts.push(inner["text"].as_str().filter(|_| inner["type"] == "text")?);
        } else {
            return None;
        }
    }
    if diff.is_some() && !texts.is_empty() {
        return None;
    }
    Some(diff.unwrap_or_else(|| json!(texts.concat())))
}

fn resolve_tool_result_output(update: &Value) -> Value {
    let content = update.get("content");
    if let Some(raw) = update.get("rawOutput") {
        let metadata_only = raw
            .as_object()
            .is_some_and(|m| m.len() == 1 && m.contains_key("metadata"));
        if metadata_only {
            if let Some(normalized) = normalize_tool_content(content) {
                let mut body = match normalized {
                    Value::String(s) => json!({ "output": s }),
                    other => other,
                };
                body["metadata"] = raw["metadata"].clone();
                return body;
            }
        }
        return raw.clone();
    }
    normalize_tool_content(content)
        .or_else(|| content.cloned())
        .unwrap_or(Value::Null)
}

fn normalize_plan_entries(entries: &Value) -> Vec<Value> {
    entries
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let content = e["content"].as_str().filter(|s| !s.is_empty())?;
            let priority = e["priority"]
                .as_str()
                .filter(|p| matches!(*p, "high" | "medium" | "low"))?;
            let status = e["status"]
                .as_str()
                .filter(|s| matches!(*s, "pending" | "in_progress" | "completed"))?;
            Some(json!({"content":content,"priority":priority,"status":status}))
        })
        .collect()
}

fn suffix_prefix_overlap(base: &str, next: &str) -> usize {
    next.char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .filter(|&len| len <= base.len())
        .rev()
        .find(|&len| base.ends_with(&next[..len]))
        .unwrap_or(0)
}

const CONTROL_KEYS: &[&str] = &["data", "rate_limit_info", "type"];
const CONTROL_TYPES: &[&str] = &["output", "rate_limit_event"];

fn ident(s: &str, first: impl Fn(char) -> bool, rest: impl Fn(char) -> bool) -> usize {
    let mut len = 0;
    for (i, c) in s.char_indices() {
        if !(if i == 0 { first(c) } else { rest(c) }) {
            break;
        }
        len = i + c.len_utf8();
    }
    len
}

/// A partial JSON object that may still become a control envelope.
pub fn may_be_unfinished_control_envelope(text: &str) -> bool {
    if crate::jsjson::parse::<Value>(text.as_bytes()).is_ok() {
        return false;
    }
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(after) = compact.strip_prefix("{\"") else {
        return compact.starts_with('{');
    };
    let len = ident(
        after,
        |c| c.is_ascii_alphabetic() || c == '_',
        |c| c.is_ascii_alphanumeric() || c == '_' || c == '-',
    );
    if len == 0 {
        return true;
    }
    let key = &after[..len];
    let Some(after_key) = after[len..].strip_prefix('"') else {
        return CONTROL_KEYS.iter().any(|k| k.starts_with(key));
    };
    if !CONTROL_KEYS.contains(&key) {
        return false;
    }
    if key != "type" {
        return true;
    }
    let Some(value) = after_key.strip_prefix(":\"") else {
        return true;
    };
    let len = ident(
        value,
        |c| c.is_ascii_alphabetic() || c == '_',
        |c| c.is_ascii_alphabetic() || c == '_',
    );
    let v = &value[..len];
    if value[len..].starts_with('"') {
        CONTROL_TYPES.contains(&v)
    } else {
        CONTROL_TYPES.iter().any(|t| t.starts_with(v))
    }
}

fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn safe_file_name(uri: Option<&str>) -> Option<String> {
    let uri = uri?;
    let path = if let Some(p) = uri.strip_prefix("file://") {
        p
    } else if let Some(rest) = uri
        .strip_prefix("https://")
        .or_else(|| uri.strip_prefix("http://"))
    {
        rest.split(['?', '#']).next().unwrap_or("")
    } else if !uri.contains(':') {
        uri
    } else {
        return None;
    };
    let name: String = path.rsplit(['/', '\\']).next()?.chars().take(255).collect();
    (!name.is_empty()).then_some(name)
}

pub struct MessageHandler {
    sink: Box<dyn FnMut(AgentMessage) + Send>,
    opts: MessageHandlerOptions,
    images: Option<GeneratedImages>,
    accepting: bool,
    tool_calls: HashMap<String, (String, Value)>,
    native_tools: HashMap<String, (String, Option<Value>)>,
    text: String,
    text_id: Option<String>,
    text_deadline: Option<Instant>,
    reasoning: String,
    reasoning_id: Option<String>,
    reasoning_at: Option<Instant>,
    reasoning_last: String,
    reasoning_emitted: bool,
}

impl MessageHandler {
    pub fn new(sink: Box<dyn FnMut(AgentMessage) + Send>, opts: MessageHandlerOptions) -> Self {
        Self {
            sink,
            opts,
            images: None,
            accepting: true,
            tool_calls: HashMap::new(),
            native_tools: HashMap::new(),
            text: String::new(),
            text_id: None,
            text_deadline: None,
            reasoning: String::new(),
            reasoning_id: None,
            reasoning_at: None,
            reasoning_last: String::new(),
            reasoning_emitted: false,
        }
    }
    /// Where inline ACP image blocks are registered; without it they are dropped.
    pub fn set_images(&mut self, images: GeneratedImages) {
        self.images = Some(images);
    }
    fn emit(&mut self, message: AgentMessage) {
        if self.accepting {
            (self.sink)(message);
        }
    }
    /// Drops late updates of a cancelled prompt.
    pub fn deactivate(&mut self) {
        self.accepting = false;
        self.text.clear();
        self.text_id = None;
        self.text_deadline = None;
        self.reset_reasoning();
    }
    pub fn discard_buffers(&mut self) {
        self.text.clear();
        self.text_id = None;
        self.text_deadline = None;
        self.reset_reasoning();
    }
    /// Reasoning first, then text: the Reasoning block renders above the answer.
    pub fn drain_buffers(&mut self) {
        self.flush_reasoning();
        self.flush_text();
    }
    pub fn idle_deadline(&self) -> Option<Instant> {
        self.text_deadline
    }
    /// The idle text flush: a live snapshot under a stable stream id.
    pub fn flush_idle(&mut self) {
        self.text_deadline = None;
        if self.text.is_empty()
            || !self.reasoning.is_empty()
            || may_be_unfinished_control_envelope(&self.text)
            || is_internal_event_json(&self.text)
        {
            return;
        }
        let id = self
            .text_id
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        let text = self.text.clone();
        self.emit(AgentMessage::Text {
            text,
            id: Some(id),
            live: true,
            stream_snapshot: true,
        });
    }
    fn flush_text(&mut self) {
        let text = std::mem::take(&mut self.text);
        let id = self.text_id.take();
        self.text_deadline = None;
        if text.is_empty() || is_internal_event_json(&text) {
            return;
        }
        let stream_snapshot = id.is_some();
        self.emit(AgentMessage::Text {
            text,
            id,
            live: false,
            stream_snapshot,
        });
    }
    fn flush_reasoning(&mut self) {
        if self.reasoning.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.reasoning);
        let id = self.reasoning_id.take().filter(|_| self.reasoning_emitted);
        self.reset_reasoning();
        if text.trim().is_empty() {
            return;
        }
        self.emit(AgentMessage::Reasoning {
            text,
            id,
            live: false,
        });
    }
    fn reset_reasoning(&mut self) {
        self.reasoning.clear();
        self.reasoning_id = None;
        self.reasoning_at = None;
        self.reasoning_last.clear();
        self.reasoning_emitted = false;
    }
    fn refresh_idle_timer(&mut self) {
        let ms = self.opts.text_idle_flush_ms.unwrap_or(TEXT_IDLE_FLUSH_MS);
        self.text_deadline =
            (ms > 0 && !self.text.is_empty()).then(|| Instant::now() + Duration::from_millis(ms));
    }
    fn append_text(&mut self, text: &str) {
        let buffered = &self.text;
        if self.opts.text_chunk_mode == TextChunkMode::Delta || buffered.is_empty() {
            self.text.push_str(text);
        } else if text.starts_with(buffered.as_str()) || text.ends_with(buffered.as_str()) {
            self.text = text.to_owned();
        } else if !(buffered.starts_with(text) || buffered.ends_with(text)) {
            let overlap = suffix_prefix_overlap(buffered, text);
            self.text.push_str(&text[overlap..]);
        }
        self.refresh_idle_timer();
    }
    fn append_reasoning(&mut self, text: &str) {
        self.reasoning.push_str(text);
        let id = self
            .reasoning_id
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        let now = Instant::now();
        let Some(last) = self.reasoning_at else {
            self.reasoning_at = Some(now);
            return;
        };
        if now.duration_since(last) < REASONING_SNAPSHOT_INTERVAL {
            return;
        }
        self.reasoning_at = Some(now);
        if self.reasoning.trim().is_empty() || self.reasoning == self.reasoning_last {
            return;
        }
        self.reasoning_last = self.reasoning.clone();
        self.reasoning_emitted = true;
        let text = self.reasoning.clone();
        self.emit(AgentMessage::Reasoning {
            text,
            id: Some(id),
            live: true,
        });
    }
    pub fn handle_update(&mut self, update: &Value) {
        let Some(kind) = str_of(update, "sessionUpdate") else {
            return;
        };
        self.refresh_idle_timer();
        let content = &update["content"];
        match kind {
            "agent_thought_chunk" => {
                if let Some(t) = content["text"]
                    .as_str()
                    .filter(|t| !t.is_empty() && content["type"] == "text")
                {
                    self.append_reasoning(t);
                }
            }
            "agent_message_chunk" => {
                if content["type"] == "image" {
                    self.drain_buffers();
                    self.emit_image(content);
                    return;
                }
                let Some(text) = extract_text_content(content).filter(|t| !t.is_empty()) else {
                    return;
                };
                let prefixed = !self.text.is_empty() && text.starts_with(self.text.as_str());
                if let Some(limit) = parse_rate_limit_text(text) {
                    if prefixed {
                        self.text.clear();
                    }
                    if let RateLimit::Message(m) = limit {
                        self.drain_buffers();
                        self.emit(m);
                    }
                    return;
                }
                if is_internal_event_json(text) {
                    if prefixed {
                        self.text.clear();
                    }
                    return;
                }
                self.flush_reasoning();
                self.append_text(text);
            }
            "tool_call" => {
                self.drain_buffers();
                self.handle_tool_call(update);
            }
            "tool_call_update" => {
                self.flush_reasoning();
                self.handle_tool_call_update(update);
            }
            "plan" => {
                self.drain_buffers();
                let items = normalize_plan_entries(&update["entries"]);
                if !items.is_empty() {
                    self.emit(AgentMessage::Plan(items));
                }
            }
            _ => {}
        }
    }
    fn emit_image(&mut self, block: &Value) {
        let Some(images) = self.images.clone() else {
            return;
        };
        let Some(data) = block["data"].as_str() else {
            return;
        };
        if data.len() > MAX_IMAGE_BYTES / 3 * 4 + 8 {
            return;
        }
        let compact: String = data.chars().filter(|c| !c.is_whitespace()).collect();
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(compact) else {
            return;
        };
        let Some(mime) = sniff_image(&bytes).filter(|_| bytes.len() <= MAX_IMAGE_BYTES) else {
            return;
        };
        let declared = block["mimeType"]
            .as_str()
            .or_else(|| block["mime_type"].as_str());
        if declared.is_some_and(|d| d != mime) {
            return;
        }
        let uri = block["uri"].as_str().or_else(|| block["url"].as_str());
        let file_name = safe_file_name(uri)
            .unwrap_or_else(|| format!("generated-{}.png", uuid::Uuid::new_v4()));
        let id = uuid::Uuid::new_v4().to_string();
        if images
            .insert(id.clone(), bytes, mime.into(), file_name.clone())
            .is_err()
        {
            return;
        }
        let mut source = json!({"ingress":"acp"});
        if let Some(f) = &self.opts.flavor {
            source["flavor"] = json!(f);
        }
        self.emit(AgentMessage::GeneratedImage {
            image_id: id,
            file_name,
            mime_type: mime.into(),
            source: Some(source),
        });
    }
    fn native_fields(&mut self, id: &str, update: &Value) -> (Option<String>, Option<Value>) {
        if let Some(name) = nonempty(update, "name") {
            let args = update
                .get("args")
                .or_else(|| update.get("rawInput"))
                .cloned();
            self.native_tools.insert(id.into(), (name.into(), args));
        }
        match self.native_tools.get(id) {
            Some((n, a)) => (Some(n.clone()), a.clone()),
            None => (None, None),
        }
    }
    fn tool_call(&mut self, id: &str, name: String, input: Value, status: &str, update: &Value) {
        let (native_name, native_args) = self.native_fields(id, update);
        self.emit(AgentMessage::ToolCall(ToolCall {
            id: id.into(),
            name,
            input,
            status: status.into(),
            title: nonempty(update, "title").map(str::to_owned),
            kind: nonempty(update, "kind").map(str::to_owned),
            progress: None,
            native_name,
            native_args,
        }));
    }
    fn handle_tool_call(&mut self, update: &Value) {
        let Some(id) = str_of(update, "toolCallId") else {
            return;
        };
        let (title, kind) = (str_of(update, "title"), str_of(update, "kind"));
        let raw = update.get("rawInput");
        let derived = derive_tool_name_with_source(title, kind, raw, None);
        let candidate = if usable(raw) {
            raw.cloned()
        } else {
            resolve_input_fallbacks(update, title)
        };
        let mut input = candidate.filter(|c| usable(Some(c))).unwrap_or(Value::Null);
        let mut name = derived.name.clone();
        if !input.is_null() {
            if let Some((n, i)) =
                canonicalize_diff_tool_input(&input, Some(kind.unwrap_or(&derived.name)))
            {
                name = n;
                input = i;
            }
        }
        self.tool_calls
            .insert(id.into(), (name.clone(), input.clone()));
        let status = normalize_status(&update["status"]);
        self.tool_call(id, name, input, &status, update);
    }
    fn handle_tool_call_update(&mut self, update: &Value) {
        let Some(id) = str_of(update, "toolCallId") else {
            return;
        };
        let status = normalize_status(&update["status"]);
        let existing = self.tool_calls.get(id).cloned();
        let kind = str_of(update, "kind");
        let raw = update.get("rawInput");
        if usable(raw) {
            let derived = derive_name_from_update(update);
            let mut name = select_name(existing.as_ref().map(|e| e.0.as_str()), &derived);
            let mut input = raw.cloned().unwrap_or(Value::Null);
            if let Some((n, i)) =
                canonicalize_diff_tool_input(&input, Some(kind.unwrap_or(&derived.name)))
            {
                name = n;
                input = i;
            }
            self.tool_calls
                .insert(id.into(), (name.clone(), input.clone()));
            self.tool_call(id, name, input, &status, update);
        } else if let Some((old_name, old_input)) = &existing {
            let (mut name, mut input) = (old_name.clone(), old_input.clone());
            let mut rederived = false;
            let title = str_of(update, "title");
            if !usable(Some(&input)) || is_stale_derived_input(&input, title, kind) {
                if let Some(fallback) =
                    resolve_input_fallbacks(update, title).filter(|f| usable(Some(f)))
                {
                    if let Some((n, i)) =
                        canonicalize_diff_tool_input(&fallback, Some(old_name.as_str()))
                    {
                        name = n;
                        input = i;
                    } else {
                        name =
                            select_name(Some(old_name.as_str()), &derive_name_from_update(update));
                        input = fallback;
                    }
                    self.tool_calls
                        .insert(id.into(), (name.clone(), input.clone()));
                    rederived = true;
                }
            }
            let enriched = (!usable(Some(old_input)) && usable(Some(&input))) || rederived;
            if matches!(status.as_str(), "in_progress" | "pending") || enriched {
                self.tool_call(id, name, input, &status, update);
            }
        }
        if matches!(status.as_str(), "completed" | "failed") {
            if status == "completed" && !usable(raw) && existing.is_some() {
                if let Some((name, input)) = hoist_diff_content(update.get("content")) {
                    self.tool_calls
                        .insert(id.into(), (name.clone(), input.clone()));
                    self.tool_call(id, name, input, &status, update);
                }
            }
            let output = resolve_tool_result_output(update);
            self.emit(AgentMessage::ToolResult {
                id: id.into(),
                output,
                status,
            });
        }
    }
}

fn select_name(existing: Option<&str>, derived: &DerivedToolName) -> String {
    match existing {
        Some(e)
            if !matches!(
                derived.source,
                ToolNameSource::Title | ToolNameSource::RawInputName | ToolNameSource::RawInputTool
            ) && !is_placeholder_tool_name(e) =>
        {
            e.to_owned()
        }
        _ => derived.name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Out = Arc<Mutex<Vec<AgentMessage>>>;

    fn handler(mode: TextChunkMode) -> (MessageHandler, Out) {
        let out: Out = Arc::default();
        let sink = out.clone();
        let h = MessageHandler::new(
            Box::new(move |m| sink.lock().unwrap().push(m)),
            MessageHandlerOptions {
                text_chunk_mode: mode,
                ..Default::default()
            },
        );
        (h, out)
    }
    fn chunk(text: &str) -> Value {
        json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}})
    }
    fn thought(text: &str) -> Value {
        json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":text}})
    }
    fn texts(out: &Out) -> Vec<String> {
        out.lock()
            .unwrap()
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn delta_mode_keeps_repeated_boundary_characters() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&chunk("73-8"));
        h.handle_update(&chunk("800"));
        h.drain_buffers();
        assert_eq!(texts(&out), vec!["73-8800"]);
    }

    #[test]
    fn dedupe_mode_merges_snapshots_and_overlaps() {
        let (mut h, out) = handler(TextChunkMode::Dedupe);
        for c in ["Hel", "Hello", "Hello", "lo world", "Hel"] {
            h.handle_update(&chunk(c));
        }
        h.drain_buffers();
        assert_eq!(texts(&out), vec!["Hello world"]);
    }

    #[test]
    fn audience_filters_user_only_text() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x","annotations":{"audience":["user"]}}}));
        h.handle_update(&json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"y","annotations":{"audience":["assistant"]}}}));
        h.drain_buffers();
        assert_eq!(texts(&out), vec!["y"]);
    }

    #[test]
    fn reasoning_flushes_before_text_and_tool_boundaries() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&thought("think"));
        h.handle_update(&chunk("answer"));
        h.handle_update(&json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"ls","kind":"execute","status":"pending"}));
        let out = out.lock().unwrap();
        assert!(
            matches!(&out[0], AgentMessage::Reasoning { text, id: None, live: false } if text == "think")
        );
        assert!(matches!(&out[1], AgentMessage::Text { text, id: None, .. } if text == "answer"));
        match &out[2] {
            AgentMessage::ToolCall(c) => {
                assert_eq!(c.name, "ls");
                assert_eq!(c.input, json!({"command":"ls"}));
                assert_eq!(c.status, "pending");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_flush_streams_snapshot_then_settles_with_same_id() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&chunk("partial"));
        let deadline = h.idle_deadline().unwrap();
        tokio::time::sleep_until(deadline).await;
        h.flush_idle();
        h.handle_update(&chunk(" more"));
        h.drain_buffers();
        let out = out.lock().unwrap();
        let (
            AgentMessage::Text {
                id: a,
                live: true,
                stream_snapshot: true,
                ..
            },
            AgentMessage::Text {
                text,
                id: b,
                live: false,
                stream_snapshot: true,
            },
        ) = (&out[0], &out[1])
        else {
            panic!("{out:?}");
        };
        assert_eq!(a, b);
        assert_eq!(text, "partial more");
    }

    #[tokio::test(start_paused = true)]
    async fn reasoning_snapshots_are_throttled() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&thought("a"));
        tokio::time::advance(Duration::from_millis(300)).await;
        h.handle_update(&thought("b"));
        h.drain_buffers();
        let out = out.lock().unwrap();
        assert!(
            matches!(&out[0], AgentMessage::Reasoning { text, live: true, id: Some(_) } if text == "ab")
        );
        assert!(
            matches!(&out[1], AgentMessage::Reasoning { text, live: false, id: Some(_) } if text == "ab")
        );
    }

    #[test]
    fn internal_and_rate_limit_text_is_filtered() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&chunk(
            r#"{"type":"output","data":{"parentUuid":null,"sessionId":"s","userType":"external"}}"#,
        ));
        h.handle_update(&chunk(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
        ));
        h.drain_buffers();
        assert!(out.lock().unwrap().is_empty());
        assert!(may_be_unfinished_control_envelope("{\"ty"));
        assert!(may_be_unfinished_control_envelope("{ \"type\": \"out"));
        assert!(!may_be_unfinished_control_envelope("{\"answer\": 4"));
        assert!(!may_be_unfinished_control_envelope("{\"type\":\"x\""));
        assert!(!may_be_unfinished_control_envelope("{\"a\":1}"));
    }

    #[test]
    fn tool_update_results_and_native_fields() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"Run run_command","status":"pending","name":"run_command","args":{"CommandLine":"ls"}}));
        h.handle_update(
            &json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"in_progress"}),
        );
        h.handle_update(&json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed","content":[{"type":"content","content":{"type":"text","text":"a"}},{"type":"content","content":{"type":"text","text":"b"}}]}));
        let out = out.lock().unwrap();
        assert_eq!(out.len(), 3);
        let AgentMessage::ToolCall(c) = &out[1] else {
            panic!()
        };
        assert_eq!(c.native_name.as_deref(), Some("run_command"));
        assert_eq!(c.native_args, Some(json!({"CommandLine":"ls"})));
        assert_eq!(
            out[2],
            AgentMessage::ToolResult {
                id: "t".into(),
                output: json!("ab"),
                status: "completed".into()
            }
        );
    }

    #[test]
    fn diff_hoisting_and_metadata_outputs() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&json!({"sessionUpdate":"tool_call","toolCallId":"w","title":"Writing","kind":"edit","status":"pending","locations":[{"path":"/f"}]}));
        h.handle_update(&json!({"sessionUpdate":"tool_call_update","toolCallId":"w","status":"completed","content":[{"type":"diff","path":"/f","newText":"n","_meta":{"kind":"add"}}]}));
        let out = out.lock().unwrap();
        let AgentMessage::ToolCall(c) = &out[1] else {
            panic!("{out:?}")
        };
        assert_eq!(c.name, "Write");
        assert_eq!(c.input, json!({"file_path":"/f","content":"n"}));
        assert!(
            matches!(&out[2], AgentMessage::ToolResult { output, .. } if output["kind"] == "add")
        );
        let update = json!({"rawOutput":{"metadata":{"exit":0}},"content":[{"type":"content","content":{"type":"text","text":"o"}}]});
        assert_eq!(
            resolve_tool_result_output(&update),
            json!({"output":"o","metadata":{"exit":0}})
        );
    }

    #[test]
    fn plans_and_title_arguments() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&json!({"sessionUpdate":"plan","entries":[{"content":"a","priority":"high","status":"pending"},{"content":"b","priority":"x","status":"pending"}]}));
        assert_eq!(
            out.lock().unwrap()[0],
            AgentMessage::Plan(vec![
                json!({"content":"a","priority":"high","status":"pending"})
            ])
        );
        assert_eq!(
            extract_title_argument("Shell: free -h", Some("execute")),
            "free -h"
        );
        assert_eq!(
            extract_title_argument("curl http://x", Some("execute")),
            "curl http://x"
        );
        assert_eq!(
            extract_title_argument("git commit -m \"feat: x\"", Some("execute")),
            "git commit -m \"feat: x\""
        );
    }

    #[test]
    fn deactivated_handler_emits_nothing() {
        let (mut h, out) = handler(TextChunkMode::Delta);
        h.handle_update(&chunk("x"));
        h.deactivate();
        h.handle_update(&chunk("y"));
        h.drain_buffers();
        assert!(out.lock().unwrap().is_empty());
    }
}
