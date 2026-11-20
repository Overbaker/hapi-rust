//! Shared ACP types (TS `cli/src/agent/types.ts`).
use serde_json::{json, Map, Value};

/// `tool_call` message (TS `AgentMessage` variant `tool_call`).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// `pending` | `in_progress` | `completed` | `failed`
    pub status: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub progress: Option<Value>,
    /// The agent's own tool name and args (agy sends `name` and `args`).
    pub native_name: Option<String>,
    pub native_args: Option<Value>,
}

/// `usage` message. Numbers stay `f64` like the TS numbers they mirror.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub total_tokens: Option<f64>,
    pub thought_tokens: Option<f64>,
    pub cache_read_tokens: Option<f64>,
    pub cache_creation_tokens: Option<f64>,
    pub context_tokens: Option<f64>,
    pub context_window: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum AgentMessage {
    Text {
        text: String,
        id: Option<String>,
        live: bool,
        stream_snapshot: bool,
    },
    Reasoning {
        text: String,
        id: Option<String>,
        live: bool,
    },
    ToolCall(ToolCall),
    ToolResult {
        id: String,
        output: Value,
        /// `completed` | `failed`
        status: String,
    },
    Usage(Usage),
    /// Plan items `{content, priority, status}` as received.
    Plan(Vec<Value>),
    GeneratedImage {
        image_id: String,
        file_name: String,
        mime_type: String,
        source: Option<Value>,
    },
    TurnComplete {
        stop_reason: String,
        num_turns: Option<f64>,
    },
    Error(String),
}

impl AgentMessage {
    pub fn text(text: impl Into<String>) -> Self {
        AgentMessage::Text {
            text: text.into(),
            id: None,
            live: false,
            stream_snapshot: false,
        }
    }
    /// Status of a tool message; `None` for other variants.
    pub fn status(&self) -> Option<&str> {
        match self {
            AgentMessage::ToolCall(c) => Some(&c.status),
            AgentMessage::ToolResult { status, .. } => Some(status),
            _ => None,
        }
    }
    /// Tool call id of a tool message.
    pub fn tool_id(&self) -> Option<&str> {
        match self {
            AgentMessage::ToolCall(c) => Some(&c.id),
            AgentMessage::ToolResult { id, .. } => Some(id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    /// `allow_once` | `allow_always` | `reject_once` | `reject_always` | other
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequest {
    pub id: String,
    pub session_id: String,
    pub tool_call_id: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
    pub options: Vec<PermissionOption>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PermissionResponse {
    Selected(String),
    Cancelled,
}
impl PermissionResponse {
    /// The ACP `session/request_permission` result.
    pub fn to_result(&self) -> Value {
        match self {
            PermissionResponse::Selected(id) => {
                json!({"outcome":{"outcome":"selected","optionId":id}})
            }
            PermissionResponse::Cancelled => json!({"outcome":{"outcome":"cancelled"}}),
        }
    }
}

/// Agent-initiated `elicitation/create` in form mode.
#[derive(Clone, Debug, PartialEq)]
pub struct FormElicitationRequest {
    pub session_id: String,
    pub tool_call_id: Option<String>,
    pub request_id: Option<String>,
    pub message: String,
    pub requested_schema: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FormElicitationResponse {
    Accept(Map<String, Value>),
    Decline,
    Cancel,
}
impl FormElicitationResponse {
    pub fn to_result(&self) -> Value {
        match self {
            FormElicitationResponse::Accept(c) => json!({"action":"accept","content":c}),
            FormElicitationResponse::Decline => json!({"action":"decline"}),
            FormElicitationResponse::Cancel => json!({"action":"cancel"}),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReasoningEffort {
    pub value: String,
    pub name: Option<String>,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelDescriptor {
    pub model_id: String,
    pub name: Option<String>,
    pub reasoning_efforts: Option<Vec<ReasoningEffort>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionModelsMetadata {
    pub available_models: Vec<ModelDescriptor>,
    pub current_model_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigOption {
    pub id: String,
    pub category: Option<String>,
    pub current_value: Option<String>,
    pub options: Vec<(String, Option<String>)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AvailableCommand {
    pub name: String,
    pub description: Option<String>,
}

/// Classified stderr line (TS `AcpStderrError`).
#[derive(Clone, Debug, PartialEq)]
pub struct StderrError {
    /// `rate_limit` | `model_not_found` | `authentication` | `quota_exceeded` | `unknown`
    pub kind: &'static str,
    pub message: String,
    pub raw: String,
}

/// What the permission handler needs from the backend. Both calls are
/// synchronous: they answer a pending agent request or write a notification.
pub trait PermissionBackend: Send + Sync {
    fn respond_to_permission(&self, request: &PermissionRequest, response: PermissionResponse);
    fn cancel_prompt(&self, session_id: &str);
    /// Whether the agent still waits for an answer to `request_id`.
    fn has_live_request(&self, request_id: &str) -> bool;
}

pub(crate) fn finite(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}
