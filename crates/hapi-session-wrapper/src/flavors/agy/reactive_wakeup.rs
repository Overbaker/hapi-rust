//! Reactive wakeups from agy's `transcript.jsonl` (TS `cli/src/agy/agyReactiveWakeup.ts`).
use hapi_wrapper_core::{
    acp::{AgentMessage, ToolCall, Usage},
    jsjson::parse as parse_json,
    session::parse_iso_timestamp_ms,
};
use indexmap::IndexMap;
use regex_lite::Regex;
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};

/// Internal prompt sent when a reactive wakeup parks on a tool approval or question.
pub const AGY_WAKEUP_CONTINUE_PROMPT: &str = "[System note: background wakeup reattached — continue your in-progress response to the background notification above without mentioning this note.]";

/// Tools that commonly park in `SetPendingToolPermission` when no `session/prompt` is open.
const AGY_GATED_TOOLS: &[&str] = &["run_command", "edit_file", "write_file", "call_mcp_tool"];
/// Tools that park the agent and end its turn immediately once their action step completes.
const AGY_TURN_TERMINATING_TOOLS: &[&str] = &["swarm_wait"];

/// Shown for a command agy moved to the background (TS `AGY_BACKGROUND_COMMAND_RESULT`).
pub fn background_command_result() -> Value {
    json!({"stdout":"Running in the background. The result appears when agy reports it."})
}

fn add_optional(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    if a.is_none() && b.is_none() {
        None
    } else {
        Some(a.unwrap_or(0.0) + b.unwrap_or(0.0))
    }
}

/// Sums token counters across multiple model steps in a turn or wakeup.
pub fn add_agy_usage(total: Option<Usage>, next: Usage) -> Usage {
    let Some(t) = total else {
        return next;
    };
    Usage {
        input_tokens: t.input_tokens + next.input_tokens,
        output_tokens: t.output_tokens + next.output_tokens,
        total_tokens: add_optional(t.total_tokens, next.total_tokens),
        thought_tokens: add_optional(t.thought_tokens, next.thought_tokens),
        cache_read_tokens: add_optional(t.cache_read_tokens, next.cache_read_tokens),
        cache_creation_tokens: add_optional(t.cache_creation_tokens, next.cache_creation_tokens),
        ..Default::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgyTranscriptToolCall {
    pub name: String,
    pub args: Map<String, Value>,
}

/// `agy acp` streams reactive wakeup updates live from 1.5.10 on
/// (google-antigravity/antigravity-cli#1461); an unknown or unparseable
/// version keeps the transcript-rendered behavior.
pub fn supports_agy_live_idle_wakeups(version: Option<&str>) -> bool {
    let Some(v) = version else { return false };
    let (core, build_ok) = match v.split_once('+') {
        Some((c, b)) => (c, valid_ident(b)),
        None => (v, true),
    };
    let (core, pre) = match core.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (core, None),
    };
    if !build_ok || pre.is_some_and(|p| !valid_ident(p)) {
        return false;
    }
    let parts: Vec<&str> = core.split('.').collect();
    let nums: Vec<u64> = parts
        .iter()
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|p| p.parse().ok())
        .collect();
    let [major, minor, patch] = nums[..] else {
        return false;
    };
    if parts.len() != 3 {
        return false;
    }
    // 1.5.10-beta sorts below 1.5.10.
    if major != 1 {
        major > 1
    } else if minor != 5 {
        minor > 5
    } else {
        patch > 10 || (patch == 10 && pre.is_none())
    }
}
fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// One `transcript.jsonl` row. `kind` is the TS `type` field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgyTranscriptStep {
    pub step_index: i64,
    pub source: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub created_at: Option<String>,
    pub content: Option<String>,
    pub thinking: Option<String>,
    pub tool_calls: Vec<AgyTranscriptToolCall>,
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub cache_read_tokens: Option<f64>,
    pub thought_tokens: Option<f64>,
    /// `truncated_fields` is a non-empty array.
    pub truncated: bool,
}

impl AgyTranscriptStep {
    /// Parses a row; `None` unless `step_index` is a number.
    pub fn from_value(v: &Value) -> Option<Self> {
        let step_index = v.get("step_index")?.as_f64()? as i64;
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let n = |k: &str| v.get(k).and_then(Value::as_f64).filter(|f| f.is_finite());
        let tool_calls = v
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .map(|c| AgyTranscriptToolCall {
                        name: c["name"].as_str().unwrap_or("").to_owned(),
                        args: c["args"].as_object().cloned().unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            step_index,
            source: s("source"),
            kind: s("type"),
            status: s("status"),
            created_at: s("created_at"),
            content: s("content"),
            thinking: s("thinking"),
            tool_calls,
            input_tokens: n("input_tokens"),
            output_tokens: n("output_tokens"),
            cache_read_tokens: n("cache_read_tokens"),
            thought_tokens: n("thought_tokens"),
            truncated: v
                .get("truncated_fields")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty()),
        })
    }
    pub fn is(&self, source: &str, kind: &str) -> bool {
        self.source.as_deref() == Some(source) && self.kind.as_deref() == Some(kind)
    }
    pub fn is_system_message(&self) -> bool {
        self.is("SYSTEM", "SYSTEM_MESSAGE")
    }
    pub fn is_planner(&self) -> bool {
        self.is("MODEL", "PLANNER_RESPONSE")
    }
    fn is_model_action(&self) -> bool {
        self.source.as_deref() == Some("MODEL") && self.kind.as_deref() != Some("PLANNER_RESPONSE")
    }
    fn status_is(&self, s: &str) -> bool {
        self.status.as_deref() == Some(s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgyReactiveTimingConfig {
    /// Quiet period before treating a no-tool PLANNER_RESPONSE or ask_question as settled.
    pub settle_ms: u64,
    /// Grace window before treating a gated tool call as parked on ACP approval.
    pub tool_park_ms: u64,
    /// Longer grace window for non-gated / external network tools.
    pub slow_tool_park_ms: u64,
    /// Safety bound if agy stops writing mid-wakeup.
    pub max_idle_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgyOpenBackgroundCommand {
    pub call_id: String,
    pub task_id: Option<String>,
    pub after_step_index: Option<i64>,
    pub ordinal_in_turn: Option<usize>,
    pub max_step_index: Option<i64>,
    pub step_index: Option<i64>,
    pub synthetic: bool,
    pub command_line: Option<String>,
    pub native_args: Option<Map<String, Value>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgyLiveBackgroundCompletionLookup {
    pub live_call_id: Option<String>,
    pub command_line: Option<String>,
    pub native_args: Option<Map<String, Value>>,
    pub call_status: Option<String>,
    /// `Some` when the TS `resultOutput` is defined (it may be `null`).
    pub result_output: Option<Value>,
    pub result_status: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum AgySyntheticCommandMatch {
    /// `index` points into the slice passed to the matcher.
    Matched {
        index: usize,
        entry: AgyOpenBackgroundCommand,
    },
    Ambiguous,
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgyReactiveInspection {
    None,
    Running {
        latest_step_index: i64,
    },
    Completed {
        steps: Vec<AgyTranscriptStep>,
        latest_step_index: i64,
    },
    ParkedOnTool {
        latest_step_index: i64,
    },
    TimedOut {
        steps: Vec<AgyTranscriptStep>,
        latest_step_index: i64,
    },
}
impl AgyReactiveInspection {
    pub fn latest_step_index(&self) -> Option<i64> {
        match self {
            Self::None => None,
            Self::Running { latest_step_index }
            | Self::ParkedOnTool { latest_step_index }
            | Self::Completed {
                latest_step_index, ..
            }
            | Self::TimedOut {
                latest_step_index, ..
            } => Some(*latest_step_index),
        }
    }
}

pub fn default_agy_brain_dir() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_default()
        .join(".gemini")
        .join("antigravity-cli")
        .join("brain")
}

fn is_safe_session_segment(id: &str) -> bool {
    !id.is_empty()
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains("..")
        && !id.contains('\0')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgyTranscriptPaths {
    pub logs_dir: PathBuf,
    pub compact_path: PathBuf,
    pub full_path: PathBuf,
}

pub fn agy_transcript_paths(brain_dir: &Path, session_id: &str) -> Option<AgyTranscriptPaths> {
    if !is_safe_session_segment(session_id) {
        return None;
    }
    let logs_dir = brain_dir
        .join(session_id)
        .join(".system_generated")
        .join("logs");
    Some(AgyTranscriptPaths {
        compact_path: logs_dir.join("transcript.jsonl"),
        full_path: logs_dir.join("transcript_full.jsonl"),
        logs_dir,
    })
}

struct CacheEntry {
    ino: u64,
    mtime: Option<SystemTime>,
    size: u64,
    parsed_offset: u64,
    head: Vec<u8>,
    steps: Vec<AgyTranscriptStep>,
}
const JSONL_CACHE_LIMIT: usize = 8;
const JSONL_HEAD_CHECK_BYTES: u64 = 128;
static JSONL_CACHE: LazyLock<Mutex<IndexMap<PathBuf, CacheEntry>>> =
    LazyLock::new(Default::default);

fn inode(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

fn parse_jsonl_chunk(raw: &str, out: &mut Vec<AgyTranscriptStep>) {
    for line in raw.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(step) = parse_json::<Value>(line.as_bytes())
            .ok()
            .as_ref()
            .and_then(AgyTranscriptStep::from_value)
        {
            out.push(step);
        }
    }
}

fn read_range(file: &mut fs::File, offset: u64, len: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    if file.seek(SeekFrom::Start(offset)).is_ok() {
        let _ = file.take(len).read_to_end(&mut buf);
    }
    buf
}

/// Incrementally reads newline-terminated JSONL steps from `path`: an unchanged
/// file costs one stat, an appended file parses only the new tail.
fn read_complete_jsonl_steps(path: &Path) -> Vec<AgyTranscriptStep> {
    let mut cache = JSONL_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let meta = match fs::metadata(path) {
        Ok(m) if m.len() > 0 => m,
        _ => {
            cache.shift_remove(path);
            return Vec::new();
        }
    };
    let (ino, size, mtime) = (inode(&meta), meta.len(), meta.modified().ok());
    if let Some(c) = cache.get(path) {
        if c.ino == ino && c.size == size && c.mtime == mtime {
            return c.steps.clone();
        }
    }
    let Ok(mut file) = fs::File::open(path) else {
        return cache.get(path).map(|c| c.steps.clone()).unwrap_or_default();
    };
    let cached = cache.get(path);
    let can_append = cached.is_some_and(|c| {
        c.ino == ino
            && size >= c.parsed_offset
            && !c.head.is_empty()
            && read_range(&mut file, 0, c.head.len() as u64) == c.head
    });
    let (start, mut steps, mut head) = match cached {
        Some(c) if can_append => (c.parsed_offset, c.steps.clone(), c.head.clone()),
        _ => (0, Vec::new(), Vec::new()),
    };
    let mut parsed_offset = start;
    if size > start {
        let tail = read_range(&mut file, start, size - start);
        if let Some(last) = tail.iter().rposition(|b| *b == b'\n') {
            parse_jsonl_chunk(&String::from_utf8_lossy(&tail[..last]), &mut steps);
            parsed_offset = start + last as u64 + 1;
        }
    }
    if head.is_empty() && parsed_offset > 0 {
        head = read_range(&mut file, 0, JSONL_HEAD_CHECK_BYTES.min(parsed_offset));
    }
    if !cache.contains_key(path) && cache.len() >= JSONL_CACHE_LIMIT {
        cache.shift_remove_index(0);
    }
    cache.insert(
        path.to_owned(),
        CacheEntry {
            ino,
            mtime,
            size,
            parsed_offset,
            head,
            steps: steps.clone(),
        },
    );
    steps
}

/// Highest `step_index` persisted in `transcript.jsonl` (-1 when absent).
pub fn read_latest_agy_transcript_step_index(brain_dir: &Path, session_id: &str) -> i64 {
    let Some(paths) = agy_transcript_paths(brain_dir, session_id) else {
        return -1;
    };
    read_complete_jsonl_steps(&paths.compact_path)
        .iter()
        .map(|s| s.step_index)
        .fold(-1, i64::max)
}

/// Steps with `step_index > after`, upgrading truncated rows from `transcript_full.jsonl`.
pub fn read_agy_transcript_steps_after(
    brain_dir: &Path,
    session_id: &str,
    after: i64,
) -> Vec<AgyTranscriptStep> {
    let Some(paths) = agy_transcript_paths(brain_dir, session_id) else {
        return Vec::new();
    };
    let compact: Vec<_> = read_complete_jsonl_steps(&paths.compact_path)
        .into_iter()
        .filter(|s| s.step_index > after)
        .collect();
    if compact.is_empty() || !compact.iter().any(|s| s.truncated) {
        return compact;
    }
    let full = read_complete_jsonl_steps(&paths.full_path);
    if full.is_empty() {
        return compact;
    }
    let mut by_index = HashMap::new();
    for s in full.into_iter().filter(|s| s.step_index > after) {
        by_index.insert(s.step_index, s);
    }
    compact
        .into_iter()
        .map(|s| by_index.get(&s.step_index).cloned().unwrap_or(s))
        .collect()
}

fn is_outside_workspace(file: &Value, workspace: &str) -> bool {
    let Some(file) = file.as_str().filter(|f| !f.is_empty()) else {
        return false;
    };
    if workspace.is_empty() {
        return false;
    }
    let root = if workspace.ends_with('/') {
        workspace.to_owned()
    } else {
        format!("{workspace}/")
    };
    file != workspace && !file.starts_with(&root)
}

/// Decodes `tool_calls[].args`, where agy stores each value JSON-serialized.
pub fn decode_agy_transcript_args(raw: &Map<String, Value>) -> Map<String, Value> {
    static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-?\d+(?:\.\d+)?$").unwrap());
    raw.iter()
        .map(|(k, v)| {
            let Some(s) = v.as_str() else {
                return (k.clone(), v.clone());
            };
            let t = s.trim();
            let encoded = (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
                || (t.starts_with('{') && t.ends_with('}'))
                || (t.starts_with('[') && t.ends_with(']'))
                || matches!(t, "true" | "false" | "null")
                || NUMBER.is_match(t);
            let decoded = encoded
                .then(|| parse_json::<Value>(t.as_bytes()).ok())
                .flatten()
                .unwrap_or_else(|| v.clone());
            (k.clone(), decoded)
        })
        .collect()
}

fn required_park_ms(
    calls: &[&AgyTranscriptToolCall],
    workspace: &str,
    t: &AgyReactiveTimingConfig,
) -> Option<u64> {
    let mut wait: Option<u64> = None;
    let mut lower = |c: u64| wait = Some(wait.map_or(c, |w| w.min(c)));
    for call in calls {
        let name = call.name.as_str();
        if name == "ask_question" {
            lower(t.settle_ms);
        } else if AGY_GATED_TOOLS.contains(&name) {
            lower(t.tool_park_ms);
        } else if name == "read_file" {
            let args = decode_agy_transcript_args(&call.args);
            lower(
                if is_outside_workspace(args.get("Path").unwrap_or(&Value::Null), workspace) {
                    t.tool_park_ms
                } else {
                    t.slow_tool_park_ms
                },
            );
        }
        // Other agy tools run internally and never park on an ACP permission.
    }
    wait
}

/// Classifies steps written after a user turn finished; only autonomous
/// `SYSTEM_MESSAGE` entries start a reactive wakeup.
pub fn inspect_agy_reactive_steps(
    steps_after_turn: &[AgyTranscriptStep],
    workspace: &str,
    now_ms: i64,
    last_progress_at_ms: i64,
    t: &AgyReactiveTimingConfig,
) -> AgyReactiveInspection {
    let Some(first) = steps_after_turn.iter().position(|s| s.is_system_message()) else {
        return AgyReactiveInspection::None;
    };
    let reactive = &steps_after_turn[first..];
    let last = reactive.last().unwrap();
    let latest = last.step_index;
    let idle = (now_ms - last_progress_at_ms).max(0) as u64;
    let completed = || AgyReactiveInspection::Completed {
        steps: reactive.to_vec(),
        latest_step_index: latest,
    };
    let running = AgyReactiveInspection::Running {
        latest_step_index: latest,
    };
    let last_system = reactive.iter().rposition(|s| s.is_system_message());
    let last_planner = reactive.iter().rposition(|s| s.is_planner());
    if let Some(pi) = last_planner.filter(|p| Some(*p) > last_system) {
        let planner = &reactive[pi];
        if planner.status_is("DONE") {
            let calls = &planner.tool_calls;
            if calls.is_empty() && pi == reactive.len() - 1 {
                return if idle >= t.settle_ms {
                    completed()
                } else {
                    running
                };
            }
            if !calls.is_empty() {
                let trailing: Vec<&AgyTranscriptStep> = reactive[pi + 1..]
                    .iter()
                    .filter(|s| s.is_model_action())
                    .collect();
                let outstanding: Vec<&AgyTranscriptToolCall> = calls
                    .iter()
                    .enumerate()
                    .filter(|(i, call)| match trailing.get(*i) {
                        None => true,
                        Some(a) => {
                            a.status_is("PENDING")
                                || a.status_is("WAITING")
                                || (call.name == "ask_question"
                                    && !a.status_is("DONE")
                                    && !a.status_is("ERROR"))
                        }
                    })
                    .map(|(_, c)| c)
                    .collect();
                if !outstanding.is_empty() {
                    if required_park_ms(&outstanding, workspace, t).is_some_and(|p| idle >= p) {
                        return AgyReactiveInspection::ParkedOnTool {
                            latest_step_index: latest,
                        };
                    }
                } else if calls
                    .iter()
                    .any(|c| AGY_TURN_TERMINATING_TOOLS.contains(&c.name.as_str()))
                    && trailing.len() == calls.len()
                    && calls
                        .iter()
                        .zip(&trailing)
                        .all(|(c, a)| is_completed_or_backgrounded_batch_action(c, Some(*a)))
                    && std::ptr::eq(last, trailing[calls.len() - 1])
                {
                    return if idle >= t.settle_ms {
                        completed()
                    } else {
                        running
                    };
                }
            }
        }
    }
    if idle >= t.max_idle_ms {
        return AgyReactiveInspection::TimedOut {
            steps: reactive.to_vec(),
            latest_step_index: latest,
        };
    }
    running
}

fn strip_agy_step_timestamps(content: &str) -> String {
    static TS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:Created At: [^\n]*\n(?:Completed At: [^\n]*\n)?)+").unwrap()
    });
    TS.replace(content, "").into_owned()
}

pub fn extract_agy_background_task_id(content: Option<&str>) -> Option<String> {
    static TASK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"Tool is running as a background task with task id:\s*([^\s"'<>]+/task-\d+)"#)
            .unwrap()
    });
    TASK.captures(content?).map(|c| c[1].to_owned())
}

fn is_completed_or_backgrounded_batch_action(
    call: &AgyTranscriptToolCall,
    action: Option<&AgyTranscriptStep>,
) -> bool {
    let Some(action) = action.filter(|a| a.is_model_action()) else {
        return false;
    };
    if action.status_is("DONE") || action.status_is("ERROR") {
        return true;
    }
    call.name == "run_command"
        && action.status_is("RUNNING")
        && extract_agy_background_task_id(action.content.as_deref()).is_some()
}

/// Action steps following the planner at `i` (one per tool call at most).
fn action_steps(steps: &[AgyTranscriptStep], i: usize, calls: usize) -> &[AgyTranscriptStep] {
    let mut end = i + 1;
    while end < steps.len() && end - i - 1 < calls && steps[end].is_model_action() {
        end += 1;
    }
    &steps[i + 1..end]
}

/// Task id (or `None`) for every `run_command` call in `steps`, in call order.
pub fn extract_agy_background_task_ids(steps: &[AgyTranscriptStep]) -> Vec<Option<String>> {
    let mut ids = Vec::new();
    let mut i = 0;
    while i < steps.len() {
        let step = &steps[i];
        if !step.is_planner() || step.tool_calls.is_empty() {
            i += 1;
            continue;
        }
        let actions = action_steps(steps, i, step.tool_calls.len());
        for (idx, call) in step.tool_calls.iter().enumerate() {
            if call.name == "run_command" {
                ids.push(extract_agy_background_task_id(
                    actions.get(idx).and_then(|a| a.content.as_deref()),
                ));
            }
        }
        i += actions.len() + 1;
    }
    ids
}

fn resolve_entries<'a>(
    brain_dir: &Path,
    session_id: &str,
    entries: impl Iterator<Item = &'a mut AgyOpenBackgroundCommand>,
    upper: Option<i64>,
) {
    let mut windows: HashMap<(i64, Option<i64>), Vec<Option<String>>> = HashMap::new();
    for entry in entries.filter(|e| e.task_id.is_none()) {
        if let Some(step_index) = entry.step_index {
            let steps = read_agy_transcript_steps_after(brain_dir, session_id, step_index - 1);
            let exact = steps.iter().find(|s| s.step_index == step_index);
            if let Some(id) =
                extract_agy_background_task_id(exact.and_then(|s| s.content.as_deref()))
            {
                entry.task_id = Some(id);
            }
            continue;
        }
        let (Some(after), Some(ordinal)) = (entry.after_step_index, entry.ordinal_in_turn) else {
            continue;
        };
        let max = match (entry.max_step_index, upper) {
            (Some(m), Some(u)) => Some(m.min(u)),
            (m, u) => m.or(u),
        };
        let ids = windows.entry((after, max)).or_insert_with(|| {
            let steps: Vec<_> = read_agy_transcript_steps_after(brain_dir, session_id, after)
                .into_iter()
                .filter(|s| max.is_none_or(|m| s.step_index <= m))
                .collect();
            extract_agy_background_task_ids(&steps)
        });
        if let Some(Some(id)) = ids.get(ordinal) {
            entry.task_id = Some(id.clone());
        }
    }
}

/// Resolves `task_id` for open background commands whose `RUN_COMMAND` step
/// had not flushed yet when the turn closed.
pub fn resolve_agy_background_task_ids(
    brain_dir: &Path,
    session_id: &str,
    commands: &mut [AgyOpenBackgroundCommand],
    upper_step_index: Option<i64>,
) {
    resolve_entries(brain_dir, session_id, commands.iter_mut(), upper_step_index);
}

/// Advances the cursor at the end of a prompt turn while preserving any
/// autonomous `SYSTEM_MESSAGE` wakeup slice written after agy closed the turn.
pub fn advance_agy_turn_transcript_cursor(
    steps: &[AgyTranscriptStep],
    current_cursor: i64,
    streamed_text_counts: &HashMap<String, usize>,
) -> i64 {
    let mut cursor = current_cursor;
    let mut remaining = streamed_text_counts.clone();
    let consume = |map: &mut HashMap<String, usize>, step: &AgyTranscriptStep| {
        if let Some(text) = step
            .content
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            if let Some(n) = map.get_mut(text).filter(|n| **n > 0) {
                *n -= 1;
            }
        }
    };
    let is_terminal_planner =
        |s: &AgyTranscriptStep| s.is_planner() && s.status_is("DONE") && s.tool_calls.is_empty();
    let terminal_turn_end = |pi: usize| -> Option<usize> {
        let step = &steps[pi];
        if !step.is_planner() || !step.status_is("DONE") {
            return None;
        }
        let calls = &step.tool_calls;
        if calls.is_empty() {
            return Some(pi);
        }
        if !calls
            .iter()
            .any(|c| AGY_TURN_TERMINATING_TOOLS.contains(&c.name.as_str()))
        {
            return None;
        }
        let end = pi + calls.len();
        if end >= steps.len() {
            return None;
        }
        (pi + 1..=end)
            .all(|k| is_completed_or_backgrounded_batch_action(&calls[k - pi - 1], Some(&steps[k])))
            .then_some(end)
    };
    let last_user = steps
        .iter()
        .rposition(|s| s.is("USER_EXPLICIT", "USER_INPUT"));
    let mut start = 0;
    if let Some(lu) = last_user {
        for step in &steps[..=lu] {
            if step.is_planner() && step.status_is("DONE") {
                consume(&mut remaining, step);
            }
            cursor = cursor.max(step.step_index);
        }
        start = lu + 1;
    }
    let mut saw_terminal = false;
    let mut pending_terminal_action: Option<usize> = None;
    for (i, step) in steps.iter().enumerate().skip(start) {
        if step.is_system_message() {
            if saw_terminal {
                break;
            }
            let mut terminal = None;
            let mut unstreamed = false;
            let mut probe = remaining.clone();
            for (j, ahead) in steps.iter().enumerate().skip(i + 1) {
                if !ahead.is_planner() {
                    continue;
                }
                if !ahead.status_is("DONE") {
                    unstreamed = true;
                    break;
                }
                if let Some(text) = ahead
                    .content
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    match probe.get_mut(text).filter(|n| **n > 0) {
                        Some(n) => *n -= 1,
                        None => {
                            unstreamed = true;
                            break;
                        }
                    }
                }
                if let Some(end) = terminal_turn_end(j) {
                    terminal = Some(end);
                    break;
                }
            }
            if unstreamed || terminal.is_none() {
                break;
            }
            cursor = cursor.max(step.step_index);
            continue;
        }
        if step.is_planner() && step.status_is("DONE") {
            consume(&mut remaining, step);
            if is_terminal_planner(step) {
                saw_terminal = true;
            } else if let Some(end) = terminal_turn_end(i) {
                pending_terminal_action = Some(end);
            }
        }
        if pending_terminal_action == Some(i) {
            saw_terminal = true;
        }
        cursor = cursor.max(step.step_index);
    }
    cursor
}

struct CompletedTask {
    task_id: String,
    body: String,
    exit_code: Option<i64>,
}

static EXIT_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Command exited with code (-?\d+)\.").unwrap());

fn exit_code_of(text: &str) -> Option<i64> {
    EXIT_CODE.captures(text).and_then(|c| c[1].parse().ok())
}

fn parse_completed_background_task(
    content: Option<&str>,
    session_id: &str,
) -> Option<CompletedTask> {
    let pattern = format!(
        r#"Task id "({}/task-\d+)" finished with result:\n([\s\S]*?)(?:\nLog: file:///[^\n]*)?\s*</SYSTEM_MESSAGE>"#,
        regex_lite::escape(session_id)
    );
    let c = Regex::new(&pattern).ok()?.captures(content?)?;
    let body = c[2].trim_end().to_owned();
    Some(CompletedTask {
        task_id: c[1].to_owned(),
        exit_code: exit_code_of(&body),
        body,
    })
}

fn normalize_agy_command_body(text: &str) -> (String, Option<i64>) {
    static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^Command exited with code -?\d+\.\s*\n\s*Command output:\n?").unwrap()
    });
    let normalized = text.replace("\r\n", "\n");
    let exit = exit_code_of(&normalized);
    (PREFIX.replace(&normalized, "").trim_end().to_owned(), exit)
}

fn integral(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
}

/// `(stdout, exit_code)` of a live completion, `None` when it carries neither.
fn parse_live_command_completion_result(v: &Value) -> Option<(Option<String>, Option<i64>)> {
    if let Some(s) = v.as_str() {
        let (stdout, exit) = normalize_agy_command_body(s);
        return Some((Some(stdout), exit));
    }
    let record = v.as_object()?;
    let raw_exit = record.get("exit_code").and_then(integral);
    if let Some(out) = record.get("output").and_then(Value::as_str) {
        let (stdout, exit) = normalize_agy_command_body(out);
        return Some((Some(stdout), raw_exit.or(exit)));
    }
    if let Some(out) = record.get("stdout").and_then(Value::as_str) {
        return Some((
            Some(out.replace("\r\n", "\n").trim_end().to_owned()),
            raw_exit,
        ));
    }
    raw_exit.map(|e| (None, Some(e)))
}

/// JS `===` for JSON values (numbers compare by value).
fn js_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if a.is_number() && b.is_number() => x == y,
        _ => a == b,
    }
}
fn is_primitive(v: &Value) -> bool {
    v.is_string() || v.is_number() || v.is_boolean()
}
fn args_compatible(stored: &Map<String, Value>, live: &Map<String, Value>) -> bool {
    live.iter()
        .filter(|(_, v)| is_primitive(v))
        .all(|(k, v)| stored.get(k).is_none_or(|s| js_eq(s, v)))
}

fn are_command_args_compatible(
    candidate: &AgyOpenBackgroundCommand,
    live_args: Option<&Map<String, Value>>,
    live_command_line: Option<&str>,
) -> bool {
    if let (Some(live), Some(stored)) = (live_command_line, candidate.command_line.as_deref()) {
        if live != stored {
            return false;
        }
    }
    match (live_args, candidate.native_args.as_ref()) {
        (Some(live), Some(stored)) => args_compatible(stored, live),
        _ => true,
    }
}

/// Whether a live ACP background-command update maps uniquely to an open
/// synthetic `agy-wakeup-*` entry, is ambiguous, or matches none.
pub fn inspect_synthetic_background_command_match(
    brain_dir: &Path,
    session_id: &str,
    commands: &mut [AgyOpenBackgroundCommand],
    turn_start_step_index: i64,
    lookup: &AgyLiveBackgroundCompletionLookup,
) -> AgySyntheticCommandMatch {
    let candidates: Vec<usize> = (0..commands.len())
        .filter(|i| commands[*i].synthetic)
        .collect();
    if candidates.is_empty() {
        return AgySyntheticCommandMatch::None;
    }
    resolve_entries(
        brain_dir,
        session_id,
        commands.iter_mut().filter(|e| e.synthetic),
        None,
    );
    let matched = |i: usize| AgySyntheticCommandMatch::Matched {
        index: i,
        entry: commands[i].clone(),
    };
    let live_args = lookup.native_args.as_ref();
    let live_cmd = lookup.command_line.as_deref().or_else(|| {
        live_args
            .and_then(|a| a.get("CommandLine"))
            .and_then(Value::as_str)
    });
    if let Some(live_id) = lookup.live_call_id.as_deref() {
        let exact: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|i| {
                commands[*i].task_id.as_deref().is_some_and(|task| {
                    let short = task.rsplit('/').next().unwrap_or(task);
                    live_id == task || live_id == short
                })
            })
            .collect();
        if exact.len() == 1 {
            return matched(exact[0]);
        }
    }
    let compatible: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|i| are_command_args_compatible(&commands[*i], live_args, live_cmd))
        .collect();
    if compatible.is_empty() {
        return AgySyntheticCommandMatch::None;
    }
    let earliest = compatible
        .iter()
        .filter_map(|i| commands[*i].step_index)
        .fold(turn_start_step_index, i64::min);
    let recent = read_agy_transcript_steps_after(brain_dir, session_id, (earliest - 1).max(-1));
    let mut completed_tasks: HashMap<String, (String, Option<i64>)> = HashMap::new();
    for step in recent.iter().filter(|s| s.is_system_message()) {
        if let Some(task) = parse_completed_background_task(step.content.as_deref(), session_id) {
            let (stdout, exit) = normalize_agy_command_body(&task.body);
            completed_tasks.insert(task.task_id, (stdout, task.exit_code.or(exit)));
        }
    }
    let live_result = lookup
        .result_output
        .as_ref()
        .and_then(parse_live_command_completion_result);
    let has_result_signal = live_result.is_some() || lookup.result_status.is_some();
    let done = |i: &usize| {
        commands[*i]
            .task_id
            .as_ref()
            .is_some_and(|t| completed_tasks.contains_key(t))
    };
    let completed_candidates: Vec<usize> = compatible.iter().copied().filter(|i| done(i)).collect();
    let unpersisted = compatible.iter().filter(|i| !done(i)).count();
    // A result-only update names no command, so any planned call would "match" it.
    let planned_compatible = (live_cmd.is_some() || live_args.is_some())
        && recent.iter().any(|step| {
            step.step_index > turn_start_step_index
                && step.is_planner()
                && step.tool_calls.iter().any(|call| {
                    if call.name != "run_command" {
                        return false;
                    }
                    let args = decode_agy_transcript_args(&call.args);
                    let planned = args.get("CommandLine").and_then(Value::as_str);
                    if live_cmd.is_some() && planned != live_cmd {
                        return false;
                    }
                    live_args.is_none_or(|live| args_compatible(&args, live))
                })
        });
    if !has_result_signal {
        return if !completed_candidates.is_empty()
            || (!planned_compatible && unpersisted > 0)
            || matches!(lookup.call_status.as_deref(), Some("completed" | "failed"))
        {
            AgySyntheticCommandMatch::Ambiguous
        } else {
            AgySyntheticCommandMatch::None
        };
    }
    let (live_stdout, live_exit) = live_result.clone().unwrap_or((None, None));
    let task_of = |i: usize| &completed_tasks[commands[i].task_id.as_ref().unwrap()];
    if !completed_candidates.is_empty() {
        let filtered: Vec<usize> = completed_candidates
            .iter()
            .copied()
            .filter(|i| {
                let (stdout, exit) = task_of(*i);
                if let (Some(l), Some(t)) = (live_exit, *exit) {
                    if l != t {
                        return false;
                    }
                }
                match lookup.result_status.as_deref() {
                    Some("failed") if *exit == Some(0) => return false,
                    Some("completed") if exit.is_some_and(|e| e != 0) => return false,
                    _ => {}
                }
                live_stdout.as_ref().is_none_or(|l| l == stdout)
            })
            .collect();
        if filtered.len() == 1 {
            let established = task_of(filtered[0]);
            if live_stdout.is_some() && (unpersisted == 0 || established.1.is_some()) {
                return matched(filtered[0]);
            }
            return AgySyntheticCommandMatch::Ambiguous;
        }
        if filtered.len() > 1 {
            if unpersisted == 0 && live_stdout.is_some() {
                let first = task_of(filtered[0]);
                if filtered.iter().all(|i| task_of(*i) == first) {
                    return matched(filtered[0]);
                }
            }
            return AgySyntheticCommandMatch::Ambiguous;
        }
        if !planned_compatible && unpersisted > 0 {
            return AgySyntheticCommandMatch::Ambiguous;
        }
        return AgySyntheticCommandMatch::None;
    }
    if planned_compatible {
        return AgySyntheticCommandMatch::None;
    }
    if unpersisted > 0 {
        return AgySyntheticCommandMatch::Ambiguous;
    }
    AgySyntheticCommandMatch::None
}

/// The matched synthetic entry, if any (TS `findCompletedSyntheticBackgroundCommand`).
pub fn find_completed_synthetic_background_command(
    brain_dir: &Path,
    session_id: &str,
    commands: &mut [AgyOpenBackgroundCommand],
    turn_start_step_index: i64,
    lookup: &AgyLiveBackgroundCompletionLookup,
) -> Option<AgyOpenBackgroundCommand> {
    match inspect_synthetic_background_command_match(
        brain_dir,
        session_id,
        commands,
        turn_start_step_index,
        lookup,
    ) {
        AgySyntheticCommandMatch::Matched { entry, .. } => Some(entry),
        _ => None,
    }
}

/// `(output, status, backgrounded)` of a tool action step.
fn build_generic_tool_output(tool: &str, step: &AgyTranscriptStep) -> (Value, &'static str, bool) {
    let raw = strip_agy_step_timestamps(step.content.as_deref().unwrap_or(""));
    if tool == "run_command" && step.status_is("RUNNING") {
        return (background_command_result(), "completed", true);
    }
    if tool == "run_command" {
        let exit = exit_code_of(&raw);
        let failed = step.status_is("ERROR") || exit.is_some_and(|e| e != 0);
        let mut output = json!({"output":raw});
        if let Some(e) = exit {
            output["exit_code"] = json!(e);
        }
        return (output, if failed { "failed" } else { "completed" }, false);
    }
    let status = if step.status_is("ERROR") {
        "failed"
    } else {
        "completed"
    };
    (Value::String(raw), status, false)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgyReactiveConversion {
    pub messages: Vec<AgentMessage>,
    pub usage: Option<Usage>,
    pub duration_ms: Option<i64>,
}

/// Converts a completed wakeup slice into agent messages and summed usage.
/// Matched background entries are removed from `commands`; newly backgrounded
/// commands are appended as synthetic entries.
pub fn convert_agy_reactive_steps(
    session_id: &str,
    steps: &[AgyTranscriptStep],
    commands: &mut Vec<AgyOpenBackgroundCommand>,
) -> AgyReactiveConversion {
    let mut out = AgyReactiveConversion::default();
    let times: Vec<i64> = steps
        .iter()
        .filter_map(|s| s.created_at.as_deref().and_then(parse_iso_timestamp_ms))
        .collect();
    if let (Some(first), Some(last)) = (times.iter().min(), times.iter().max()) {
        out.duration_ms = Some((last - first).max(0));
    }
    let mut i = 0;
    while i < steps.len() {
        let step = &steps[i];
        i += 1;
        if step.is_system_message() {
            let Some(done) = parse_completed_background_task(step.content.as_deref(), session_id)
            else {
                continue;
            };
            if let Some(idx) = commands
                .iter()
                .position(|e| e.task_id.as_deref() == Some(done.task_id.as_str()))
            {
                let matched = commands.remove(idx);
                let mut output = json!({"output":done.body});
                if let Some(e) = done.exit_code {
                    output["exit_code"] = json!(e);
                }
                out.messages.push(AgentMessage::ToolResult {
                    id: matched.call_id,
                    output,
                    status: if done.exit_code.is_some_and(|e| e != 0) {
                        "failed"
                    } else {
                        "completed"
                    }
                    .into(),
                });
            }
            continue;
        }
        if !step.is_planner() {
            continue;
        }
        if step.input_tokens.is_some() || step.output_tokens.is_some() {
            let input = step.input_tokens.unwrap_or(0.0);
            let output = step.output_tokens.unwrap_or(0.0);
            let total = input
                + output
                + step.thought_tokens.unwrap_or(0.0)
                + step.cache_read_tokens.unwrap_or(0.0);
            out.usage = Some(add_agy_usage(
                out.usage.take(),
                Usage {
                    input_tokens: input,
                    output_tokens: output,
                    total_tokens: Some(total),
                    thought_tokens: step.thought_tokens,
                    cache_read_tokens: step.cache_read_tokens,
                    ..Default::default()
                },
            ));
        }
        if let Some(thinking) = step
            .thinking
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            out.messages.push(AgentMessage::Reasoning {
                text: thinking.into(),
                id: None,
                live: false,
            });
        }
        if let Some(content) = step.content.as_deref().filter(|c| !c.trim().is_empty()) {
            out.messages.push(AgentMessage::text(content));
        }
        if step.tool_calls.is_empty() {
            continue;
        }
        let actions = action_steps(steps, i - 1, step.tool_calls.len());
        i += actions.len();
        for (idx, call) in step.tool_calls.iter().enumerate() {
            let action = actions.get(idx);
            let decoded = Value::Object(decode_agy_transcript_args(&call.args));
            let id = format!("agy-wakeup-{session_id}-{}-{idx}", step.step_index);
            let (output, status, backgrounded) = action.map_or_else(
                || (Value::String(String::new()), "completed", false),
                |a| build_generic_tool_output(&call.name, a),
            );
            let task_id = action
                .filter(|_| backgrounded)
                .and_then(|a| extract_agy_background_task_id(a.content.as_deref()));
            // A live turn already streamed and tracks this task.
            if task_id
                .as_ref()
                .is_some_and(|t| commands.iter().any(|e| e.task_id.as_ref() == Some(t)))
            {
                continue;
            }
            out.messages.push(AgentMessage::ToolCall(ToolCall {
                id: id.clone(),
                name: call.name.clone(),
                input: decoded.clone(),
                status: status.into(),
                title: Some(call.name.clone()),
                kind: None,
                progress: None,
                native_name: Some(call.name.clone()),
                native_args: Some(decoded.clone()),
            }));
            let Some(action) = action else {
                continue;
            };
            out.messages.push(AgentMessage::ToolResult {
                id: id.clone(),
                output,
                status: status.into(),
            });
            if backgrounded {
                commands.push(AgyOpenBackgroundCommand {
                    call_id: id,
                    step_index: Some(action.step_index),
                    synthetic: true,
                    command_line: decoded["CommandLine"].as_str().map(str::to_owned),
                    native_args: decoded.as_object().cloned(),
                    task_id,
                    ..Default::default()
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_idle_wakeup_version_gate() {
        for (version, live) in [
            ("1.5.10", true),
            ("1.5.13", true),
            ("1.6.0-beta.1", true),
            ("2.0.0", true),
            ("1.5.9", false),
            ("1.5.10-beta", false),
            ("1.5.10-rc.1", false),
            ("1.5.11-rc.1", true),
            ("1.5.10+build.7", true),
            ("1.5.10foo", false),
            ("v1.5.13", false),
            ("1.4.99", false),
            ("0.9.15", false),
            ("", false),
            ("unknown", false),
        ] {
            assert_eq!(
                supports_agy_live_idle_wakeups(Some(version)),
                live,
                "{version}"
            );
        }
        assert!(!supports_agy_live_idle_wakeups(None));
    }

    const T: AgyReactiveTimingConfig = AgyReactiveTimingConfig {
        settle_ms: 50,
        tool_park_ms: 200,
        slow_tool_park_ms: 1_000,
        max_idle_ms: 5_000,
    };
    const WS: &str = "/srv/work/projects";

    fn steps(v: Value) -> Vec<AgyTranscriptStep> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| AgyTranscriptStep::from_value(s).unwrap())
            .collect()
    }
    fn write(brain: &Path, sid: &str, compact: &str, full: &[Value]) -> AgyTranscriptPaths {
        let p = agy_transcript_paths(brain, sid).unwrap();
        fs::create_dir_all(&p.logs_dir).unwrap();
        fs::write(&p.compact_path, compact).unwrap();
        if !full.is_empty() {
            let text: String = full.iter().map(|v| format!("{v}\n")).collect();
            fs::write(&p.full_path, text).unwrap();
        }
        p
    }
    fn lines(rows: &[Value]) -> String {
        rows.iter().map(|v| format!("{v}\n")).collect()
    }
    fn bg(call: &str, task: Option<&str>) -> AgyOpenBackgroundCommand {
        AgyOpenBackgroundCommand {
            call_id: call.into(),
            task_id: task.map(str::to_owned),
            ..Default::default()
        }
    }
    fn running(i: i64) -> AgyReactiveInspection {
        AgyReactiveInspection::Running {
            latest_step_index: i,
        }
    }
    fn parked(i: i64) -> AgyReactiveInspection {
        AgyReactiveInspection::ParkedOnTool {
            latest_step_index: i,
        }
    }
    fn is_completed(i: &AgyReactiveInspection, latest: i64) -> bool {
        matches!(i, AgyReactiveInspection::Completed { latest_step_index, .. } if *latest_step_index == latest)
    }
    fn result(id: &str, status: &str, output: Value) -> AgentMessage {
        AgentMessage::ToolResult {
            id: id.into(),
            output,
            status: status.into(),
        }
    }
    fn system(i: i64, content: &str) -> Value {
        json!({"step_index":i,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":content})
    }

    #[test]
    fn rejects_unsafe_segments_and_decodes_args() {
        let dir = tempfile::tempdir().unwrap();
        assert!(agy_transcript_paths(dir.path(), "../escape").is_none());
        assert!(agy_transcript_paths(dir.path(), "a/b").is_none());
        let raw = json!({"Path":"\"/srv/work/projects/a.ts\"","StartLine":"10","Overwrite":"false","Plain":"raw-string"});
        assert_eq!(
            Value::Object(decode_agy_transcript_args(raw.as_object().unwrap())),
            json!({"Path":"/srv/work/projects/a.ts","StartLine":10,"Overwrite":false,"Plain":"raw-string"})
        );
    }

    #[test]
    fn ignores_incomplete_lines_and_upgrades_truncated_steps() {
        let dir = tempfile::tempdir().unwrap();
        let compact = lines(&[
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"hi"}),
            json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"short...","truncated_fields":["content"]}),
        ]) + r#"{"step_index":2,"source":"SYSTEM","type":"SYSTEM_MESSAGE""#;
        write(
            dir.path(),
            "conv-1",
            &compact,
            &[
                json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"full untruncated response text"}),
            ],
        );
        assert_eq!(
            read_latest_agy_transcript_step_index(dir.path(), "conv-1"),
            1
        );
        let s = read_agy_transcript_steps_after(dir.path(), "conv-1", 0);
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].content.as_deref(),
            Some("full untruncated response text")
        );
    }

    #[test]
    fn incremental_reader_appends_and_reparses_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let row = |i: i64| json!({"step_index":i,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE"});
        let p = write(dir.path(), "conv-inc", &lines(&[row(0), row(1)]), &[]);
        assert_eq!(
            read_latest_agy_transcript_step_index(dir.path(), "conv-inc"),
            1
        );
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&p.compact_path)
            .unwrap();
        std::io::Write::write_all(&mut f, lines(&[row(2)]).as_bytes()).unwrap();
        drop(f);
        assert_eq!(
            read_agy_transcript_steps_after(dir.path(), "conv-inc", -1).len(),
            3
        );
        fs::write(&p.compact_path, lines(&[row(7)])).unwrap();
        let s = read_agy_transcript_steps_after(dir.path(), "conv-inc", -1);
        assert_eq!(s.iter().map(|s| s.step_index).collect::<Vec<_>>(), [7]);
        fs::remove_file(&p.compact_path).unwrap();
        assert_eq!(
            read_latest_agy_transcript_step_index(dir.path(), "conv-inc"),
            -1
        );
    }

    #[test]
    fn classifies_completed_parked_and_read_file_waits() {
        let sys = system(4, "<SYSTEM_MESSAGE>Subagent done</SYSTEM_MESSAGE>");
        let planner = |calls: Value| json!({"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":calls});
        assert_eq!(
            inspect_agy_reactive_steps(
                &steps(
                    json!([{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE"}])
                ),
                WS,
                1_000,
                0,
                &T
            ),
            AgyReactiveInspection::None
        );
        let final_text = steps(
            json!([sys, {"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"All done."}]),
        );
        assert_eq!(
            inspect_agy_reactive_steps(&final_text, WS, 120, 100, &T),
            running(5)
        );
        assert!(is_completed(
            &inspect_agy_reactive_steps(&final_text, WS, 160, 100, &T),
            5
        ));

        let gated = steps(json!([
            sys,
            planner(json!([{"name":"run_command","args":{"CommandLine":"\"git status\""}}]))
        ]));
        assert_eq!(
            inspect_agy_reactive_steps(&gated, WS, 200, 100, &T),
            running(5)
        );
        assert_eq!(
            inspect_agy_reactive_steps(&gated, WS, 310, 100, &T),
            parked(5)
        );

        let inside = steps(json!([
            sys,
            planner(
                json!([{"name":"read_file","args":{"Path":"\"/srv/work/projects/README.md\""}}])
            )
        ]));
        let outside = steps(json!([
            sys,
            planner(json!([{"name":"read_file","args":{"Path":"\"/etc/hosts\""}}]))
        ]));
        assert_eq!(
            inspect_agy_reactive_steps(&inside, WS, 400, 100, &T),
            running(5)
        );
        assert_eq!(
            inspect_agy_reactive_steps(&outside, WS, 400, 100, &T),
            parked(5)
        );

        let mixed = steps(json!([sys,
            planner(json!([{"name":"read_file","args":{"Path":"\"/srv/work/projects/README.md\""}},{"name":"run_command","args":{"CommandLine":"\"git status\""}}])),
            {"step_index":6,"source":"MODEL","type":"READ_FILE","status":"DONE","content":"1: # readme"}]));
        assert_eq!(
            inspect_agy_reactive_steps(&mixed, WS, 200, 100, &T),
            running(6)
        );
        assert_eq!(
            inspect_agy_reactive_steps(&mixed, WS, 310, 100, &T),
            parked(6)
        );

        let internal = steps(json!([
            sys,
            planner(json!([{"name":"read_board","args":{"since":"1"}}]))
        ]));
        assert_eq!(
            inspect_agy_reactive_steps(&internal, WS, 2_000, 100, &T),
            running(5)
        );
        assert!(matches!(
            inspect_agy_reactive_steps(&internal, WS, 6_000, 100, &T),
            AgyReactiveInspection::TimedOut {
                latest_step_index: 5,
                ..
            }
        ));

        let swarm = steps(json!([sys,
            planner(json!([{"name":"post_message","args":{"text":"\"@worker1 next step\""}},{"name":"swarm_wait","args":{"since":"2"}}])),
            {"step_index":6,"source":"MODEL","type":"GENERIC","status":"DONE","content":"{\"status\":\"posted\",\"seq\":3}"},
            {"step_index":7,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Waiting for a message that calls on you by name, or on @all posted after #2."}]));
        assert_eq!(
            inspect_agy_reactive_steps(&swarm, WS, 120, 100, &T),
            running(7)
        );
        assert!(is_completed(
            &inspect_agy_reactive_steps(&swarm, WS, 160, 100, &T),
            7
        ));
    }

    #[test]
    fn converts_completion_tool_calls_reasoning_text_and_usage() {
        let mut cmds = vec![bg("call-bg-42", Some("conv-xyz/task-7"))];
        let converted = convert_agy_reactive_steps(
            "conv-xyz",
            &steps(json!([
                {"step_index":10,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-11T12:00:00Z",
                 "content":"<SYSTEM_MESSAGE>\nTask id \"conv-xyz/task-7\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nBUILD_OK\nLog: file:///tmp/task-7.log\n</SYSTEM_MESSAGE>"},
                {"step_index":11,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-11T12:00:02Z",
                 "thinking":"Checking output file","input_tokens":100,"output_tokens":30,"thought_tokens":10,
                 "tool_calls":[{"name":"read_file","args":{"Path":"\"/srv/work/projects/out.txt\""}}]},
                {"step_index":12,"source":"MODEL","type":"READ_FILE","status":"DONE","created_at":"2026-11-11T12:00:02Z",
                 "content":"Created At: 2026-11-11T12:00:02Z\nCompleted At: 2026-11-11T12:00:02Z\n1: hello"},
                {"step_index":13,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-11T12:00:04Z",
                 "content":"Background build succeeded.","input_tokens":150,"output_tokens":20}
            ])),
            &mut cmds,
        );
        assert!(cmds.is_empty());
        assert_eq!(converted.duration_ms, Some(4_000));
        assert_eq!(
            converted.usage,
            Some(Usage {
                input_tokens: 250.0,
                output_tokens: 50.0,
                thought_tokens: Some(10.0),
                total_tokens: Some(310.0),
                ..Default::default()
            })
        );
        let args = json!({"Path":"/srv/work/projects/out.txt"});
        assert_eq!(
            converted.messages,
            vec![
                result(
                    "call-bg-42",
                    "completed",
                    json!({"output":"Command exited with code 0.\n\nCommand output:\nBUILD_OK","exit_code":0})
                ),
                AgentMessage::Reasoning {
                    text: "Checking output file".into(),
                    id: None,
                    live: false
                },
                AgentMessage::ToolCall(ToolCall {
                    id: "agy-wakeup-conv-xyz-11-0".into(),
                    name: "read_file".into(),
                    input: args.clone(),
                    status: "completed".into(),
                    title: Some("read_file".into()),
                    kind: None,
                    progress: None,
                    native_name: Some("read_file".into()),
                    native_args: Some(args),
                }),
                result("agy-wakeup-conv-xyz-11-0", "completed", json!("1: hello")),
                AgentMessage::text("Background build succeeded."),
            ]
        );
    }

    #[test]
    fn replayed_background_command_tracked_by_live_turn_is_not_registered_twice() {
        let mut cmds = vec![bg("call-live-37", Some("conv-dup/task-38"))];
        let replay = convert_agy_reactive_steps(
            "conv-dup",
            &steps(json!([
                system(36, "<SYSTEM_MESSAGE>\nTask id \"conv-dup/task-31\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nEARLIER\n</SYSTEM_MESSAGE>"),
                {"step_index":37,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"python3 scan.py\""}}]},
                {"step_index":38,"source":"MODEL","type":"RUN_COMMAND","status":"RUNNING","content":"Created At: 2026-11-17T22:37:38-05:00\nTool is running as a background task with task id: conv-dup/task-38"},
                {"step_index":39,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Comparing the folders."},
                {"step_index":40,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"python3 move.py\""}}]},
                {"step_index":41,"source":"MODEL","type":"RUN_COMMAND","status":"RUNNING","content":"Created At: 2026-11-17T22:39:06-05:00\nTool is running as a background task with task id: conv-dup/task-42"}
            ])),
            &mut cmds,
        );
        let calls: Vec<_> = replay
            .messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolCall(c) => Some(c.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, ["agy-wakeup-conv-dup-40-0"]);
        assert!(!replay
            .messages
            .iter()
            .any(|m| m.tool_id() == Some("agy-wakeup-conv-dup-37-0")));
        assert_eq!(
            cmds.iter()
                .map(|e| (e.call_id.as_str(), e.task_id.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("call-live-37", Some("conv-dup/task-38")),
                ("agy-wakeup-conv-dup-40-0", Some("conv-dup/task-42"))
            ]
        );
        assert!(cmds[1].synthetic && cmds[1].command_line.as_deref() == Some("python3 move.py"));
        let completion = convert_agy_reactive_steps("conv-dup", &steps(json!([
            system(42, "<SYSTEM_MESSAGE>\nTask id \"conv-dup/task-38\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nSCANNED\n</SYSTEM_MESSAGE>")
        ])), &mut cmds);
        assert_eq!(
            completion.messages,
            vec![result(
                "call-live-37",
                "completed",
                json!({"output":"Command exited with code 0.\n\nCommand output:\nSCANNED","exit_code":0})
            )]
        );
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].call_id, "agy-wakeup-conv-dup-40-0");
    }

    #[test]
    fn matches_reverse_order_completions_and_never_fifo_attributes() {
        let mut cmds = vec![
            bg("call-first", Some("conv-rev/task-1")),
            bg("call-second", Some("conv-rev/task-2")),
        ];
        let first = convert_agy_reactive_steps("conv-rev", &steps(json!([
            system(20, "<SYSTEM_MESSAGE>\nTask id \"conv-rev/task-99\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nUNRELATED\n</SYSTEM_MESSAGE>"),
            system(21, "<SYSTEM_MESSAGE>\nTask id \"conv-rev/task-2\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nSECOND_OK\n</SYSTEM_MESSAGE>")
        ])), &mut cmds);
        assert_eq!(
            first.messages,
            vec![result(
                "call-second",
                "completed",
                json!({"output":"Command exited with code 0.\n\nCommand output:\nSECOND_OK","exit_code":0})
            )]
        );
        assert_eq!(cmds, vec![bg("call-first", Some("conv-rev/task-1"))]);
        let second = convert_agy_reactive_steps("conv-rev", &steps(json!([
            system(22, "<SYSTEM_MESSAGE>\nTask id \"conv-rev/task-1\" finished with result:\nCommand exited with code 2.\n\nCommand output:\nFIRST_FAILED\n</SYSTEM_MESSAGE>")
        ])), &mut cmds);
        assert_eq!(
            second.messages,
            vec![result(
                "call-first",
                "failed",
                json!({"output":"Command exited with code 2.\n\nCommand output:\nFIRST_FAILED","exit_code":2})
            )]
        );
        assert!(cmds.is_empty());

        let mut unknown = vec![bg("call-unknown-1", None), bg("call-unknown-2", None)];
        let slice = convert_agy_reactive_steps("conv-unknown", &steps(json!([
            system(10, "<SYSTEM_MESSAGE>\nTask id \"conv-unknown/task-2\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nSECOND_ONLY\n</SYSTEM_MESSAGE>")
        ])), &mut unknown);
        assert!(slice.messages.is_empty());
        assert_eq!(unknown.len(), 2);
    }

    #[test]
    fn cursor_preserves_autonomous_wakeups_and_skips_intra_turn_folds() {
        let counts = |pairs: &[(&str, usize)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect::<HashMap<_, _>>()
        };
        let post_terminal = steps(json!([
            {"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"start"},
            {"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Turn finished."},
            system(2, "<SYSTEM_MESSAGE>Wakeup</SYSTEM_MESSAGE>")
        ]));
        assert_eq!(
            advance_agy_turn_transcript_cursor(
                &post_terminal,
                -1,
                &counts(&[("Turn finished.", 1)])
            ),
            1
        );
        let folded = steps(json!([
            {"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"start"},
            {"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"invoke_subagent","args":{}}]},
            system(2, "<SYSTEM_MESSAGE>Fast subagent</SYSTEM_MESSAGE>"),
            {"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Handled inside turn."}
        ]));
        assert_eq!(
            advance_agy_turn_transcript_cursor(
                &folded,
                -1,
                &counts(&[("Handled inside turn.", 1)])
            ),
            3
        );
        assert_eq!(
            advance_agy_turn_transcript_cursor(&folded, -1, &HashMap::new()),
            1
        );
        let swarm = steps(json!([
            {"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"/swarm start"},
            {"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"sleep 10\""}},{"name":"swarm_wait","args":{"since":"1"}}]},
            {"step_index":2,"source":"MODEL","type":"RUN_COMMAND","status":"RUNNING","content":"Tool is running as a background task with task id: conv-1/task-1"},
            {"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Waiting for a message that calls on you by name, or on @all posted after #1."},
            system(4, "<SYSTEM_MESSAGE>Board message #2</SYSTEM_MESSAGE>"),
            {"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"post_message","args":{"text":"\"@worker ack\""}},{"name":"swarm_wait","args":{"since":"2"}}]},
            {"step_index":6,"source":"MODEL","type":"GENERIC","status":"DONE","content":"{\"status\":\"posted\",\"seq\":3}"},
            {"step_index":7,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Waiting for a message that calls on you by name, or on @all posted after #2."}
        ]));
        assert_eq!(
            advance_agy_turn_transcript_cursor(&swarm, -1, &HashMap::new()),
            3
        );
    }

    #[test]
    fn resolves_late_flushed_task_ids_by_call_ordinal() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "conv-t",
            &lines(&[
                json!({"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[
                {"name":"run_command","args":{"CommandLine":"\"a\""}},{"name":"read_file","args":{}},{"name":"run_command","args":{"CommandLine":"\"b\""}}]}),
                json!({"step_index":4,"source":"MODEL","type":"RUN_COMMAND","status":"RUNNING","content":"Tool is running as a background task with task id: conv-t/task-4"}),
                json!({"step_index":5,"source":"MODEL","type":"READ_FILE","status":"DONE","content":"x"}),
                json!({"step_index":6,"source":"MODEL","type":"RUN_COMMAND","status":"RUNNING","content":"Tool is running as a background task with task id: conv-t/task-6"}),
            ]),
            &[],
        );
        let mut cmds = vec![
            AgyOpenBackgroundCommand {
                call_id: "c2".into(),
                after_step_index: Some(2),
                ordinal_in_turn: Some(1),
                ..Default::default()
            },
            AgyOpenBackgroundCommand {
                call_id: "c1".into(),
                after_step_index: Some(2),
                ordinal_in_turn: Some(0),
                max_step_index: Some(4),
                ..Default::default()
            },
            AgyOpenBackgroundCommand {
                call_id: "s".into(),
                step_index: Some(6),
                synthetic: true,
                ..Default::default()
            },
        ];
        resolve_agy_background_task_ids(dir.path(), "conv-t", &mut cmds, None);
        assert_eq!(
            cmds.iter()
                .map(|e| e.task_id.as_deref())
                .collect::<Vec<_>>(),
            [
                Some("conv-t/task-6"),
                Some("conv-t/task-4"),
                Some("conv-t/task-6")
            ]
        );
    }

    #[test]
    fn synthetic_match_needs_result_evidence() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "c", &lines(&[
            json!({"step_index":6,"source":"MODEL","type":"RUN_COMMAND","status":"RUNNING","content":"Tool is running as a background task with task id: c/task-1"}),
            system(8, "<SYSTEM_MESSAGE>\nTask id \"c/task-1\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nOK\n</SYSTEM_MESSAGE>"),
        ]), &[]);
        let mut cmds = vec![
            bg("live-other", Some("c/task-9")),
            AgyOpenBackgroundCommand {
                call_id: "agy-wakeup-c-5-0".into(),
                step_index: Some(6),
                synthetic: true,
                command_line: Some("make".into()),
                ..Default::default()
            },
        ];
        let no_result = AgyLiveBackgroundCompletionLookup {
            live_call_id: Some("live-1".into()),
            command_line: Some("make".into()),
            call_status: Some("in_progress".into()),
            ..Default::default()
        };
        assert_eq!(
            inspect_synthetic_background_command_match(dir.path(), "c", &mut cmds, 7, &no_result),
            AgySyntheticCommandMatch::Ambiguous
        );
        assert_eq!(cmds[1].task_id.as_deref(), Some("c/task-1"));
        let with_result = AgyLiveBackgroundCompletionLookup {
            result_output: Some(
                json!({"output":"Command exited with code 0.\n\nCommand output:\nOK","exit_code":0}),
            ),
            result_status: Some("completed".into()),
            ..no_result.clone()
        };
        let m = find_completed_synthetic_background_command(
            dir.path(),
            "c",
            &mut cmds,
            7,
            &with_result,
        );
        assert_eq!(m.map(|e| e.call_id), Some("agy-wakeup-c-5-0".into()));
        let other_cmd = AgyLiveBackgroundCompletionLookup {
            command_line: Some("ls".into()),
            ..with_result
        };
        assert_eq!(
            inspect_synthetic_background_command_match(dir.path(), "c", &mut cmds, 7, &other_cmd),
            AgySyntheticCommandMatch::None
        );
    }

    #[test]
    fn usage_sums_optional_counters() {
        let a = Usage {
            input_tokens: 1.0,
            output_tokens: 2.0,
            thought_tokens: Some(3.0),
            ..Default::default()
        };
        let b = Usage {
            input_tokens: 4.0,
            output_tokens: 5.0,
            cache_read_tokens: Some(6.0),
            context_tokens: Some(9.0),
            ..Default::default()
        };
        assert_eq!(add_agy_usage(None, b.clone()), b);
        assert_eq!(
            add_agy_usage(Some(a), b),
            Usage {
                input_tokens: 5.0,
                output_tokens: 7.0,
                thought_tokens: Some(3.0),
                cache_read_tokens: Some(6.0),
                ..Default::default()
            }
        );
    }
}
