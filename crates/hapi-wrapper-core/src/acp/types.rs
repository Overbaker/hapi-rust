//! Shared ACP types (TS `cli/src/agent/types.ts`).
use serde_json::Value;

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
