//! AgentMessage to the hub's agent wire payload (TS
//! `cli/src/agent/messageConverter.ts`).
use crate::acp::types::{AgentMessage, Usage};
use serde_json::{json, Map, Value};

/// A JS number as JSON: integral values serialize without a fraction.
pub fn num(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

fn put(map: &mut Map<String, Value>, key: &str, value: Option<f64>) {
    if let Some(v) = value {
        map.insert(key.into(), num(v));
    }
}

fn token_count(u: &Usage, model: Option<&str>) -> Value {
    let mut total = Map::new();
    let input = u.input_tokens
        + u.cache_read_tokens.unwrap_or(0.0)
        + u.cache_creation_tokens.unwrap_or(0.0);
    total.insert("inputTokens".into(), num(input));
    total.insert("outputTokens".into(), num(u.output_tokens));
    put(&mut total, "totalTokens", u.total_tokens);
    put(&mut total, "thoughtTokens", u.thought_tokens);
    put(&mut total, "cachedInputTokens", u.cache_read_tokens);
    put(&mut total, "cacheWriteInputTokens", u.cache_creation_tokens);
    let mut info = Map::new();
    info.insert("total".into(), Value::Object(total));
    put(&mut info, "contextTokens", u.context_tokens);
    put(&mut info, "modelContextWindow", u.context_window);
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    json!({"type":"token_count","model":model,"usageSchema":"hapi.usage.v1","inputTokenSemantics":"includes-cache","info":info})
}

pub fn convert_agent_message(message: &AgentMessage, model: Option<&str>) -> Option<Value> {
    Some(match message {
        AgentMessage::Text {
            text,
            id,
            live,
            stream_snapshot,
        } => {
            let mut v = json!({"type":"message","message":text});
            if let Some(id) = id {
                v["id"] = json!(id);
            }
            if *live {
                v["live"] = true.into();
            }
            if *stream_snapshot {
                v["streamSnapshot"] = true.into();
            }
            v
        }
        AgentMessage::Reasoning { text, id, live } => {
            let id = id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let mut v = json!({"type":"reasoning","message":text,"id":id});
            if *live {
                v["live"] = true.into();
            }
            v
        }
        AgentMessage::Usage(u) => token_count(u, model),
        AgentMessage::ToolCall(c) => {
            let mut v = json!({"type":"tool-call","name":c.name,"callId":c.id,"input":c.input,"status":c.status});
            if let Some(t) = c.title.as_deref().filter(|t| !t.is_empty()) {
                v["nativeTitle"] = json!(t);
            }
            if let Some(k) = c.kind.as_deref().filter(|k| !k.is_empty()) {
                v["nativeKind"] = json!(k);
            }
            if let Some(p) = &c.progress {
                v["progress"] = p.clone();
            }
            v
        }
        AgentMessage::ToolResult { id, output, status } => {
            json!({"type":"tool-call-result","callId":id,"output":output,"is_error":status == "failed"})
        }
        AgentMessage::Plan(items) => json!({"type":"plan","entries":items}),
        AgentMessage::GeneratedImage {
            image_id,
            file_name,
            mime_type,
            source,
        } => {
            let mut v = json!({"type":"generated-image","imageId":image_id,"fileName":file_name,"mimeType":mime_type,"id":uuid::Uuid::new_v4().to_string()});
            if let Some(s) = source {
                v["source"] = s.clone();
            }
            v
        }
        AgentMessage::Error(m) => json!({"type":"error","message":m}),
        AgentMessage::TurnComplete { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::types::ToolCall;

    #[test]
    fn text_ids_and_markers() {
        let m = AgentMessage::Text {
            text: "hi".into(),
            id: Some("s1".into()),
            live: true,
            stream_snapshot: true,
        };
        assert_eq!(
            convert_agent_message(&m, None).unwrap().to_string(),
            r#"{"type":"message","message":"hi","id":"s1","live":true,"streamSnapshot":true}"#
        );
        assert_eq!(
            convert_agent_message(&AgentMessage::text("x"), None).unwrap(),
            json!({"type":"message","message":"x"})
        );
    }

    #[test]
    fn reasoning_ids() {
        let m = AgentMessage::Reasoning {
            text: "t".into(),
            id: Some("r".into()),
            live: false,
        };
        assert_eq!(
            convert_agent_message(&m, None).unwrap(),
            json!({"type":"reasoning","message":"t","id":"r"})
        );
        let m = AgentMessage::Reasoning {
            text: "t".into(),
            id: None,
            live: true,
        };
        let v = convert_agent_message(&m, None).unwrap();
        assert!(v["id"].as_str().is_some_and(|s| s.len() == 36));
        assert_eq!(v["live"], true);
    }

    #[test]
    fn tools() {
        let c = ToolCall {
            id: "c".into(),
            name: "Bash".into(),
            input: json!({"command":"ls"}),
            status: "in_progress".into(),
            title: Some("ls".into()),
            kind: Some("execute".into()),
            progress: None,
            native_name: Some("run_command".into()),
            native_args: None,
        };
        assert_eq!(
            convert_agent_message(&AgentMessage::ToolCall(c), None)
                .unwrap()
                .to_string(),
            r#"{"type":"tool-call","name":"Bash","callId":"c","input":{"command":"ls"},"status":"in_progress","nativeTitle":"ls","nativeKind":"execute"}"#
        );
        let r = AgentMessage::ToolResult {
            id: "c".into(),
            output: json!("x"),
            status: "failed".into(),
        };
        assert_eq!(
            convert_agent_message(&r, None).unwrap(),
            json!({"type":"tool-call-result","callId":"c","output":"x","is_error":true})
        );
        let t = AgentMessage::TurnComplete {
            stop_reason: "end_turn".into(),
            num_turns: None,
        };
        assert_eq!(convert_agent_message(&t, None), None);
    }

    #[test]
    fn usage_token_count() {
        let u = AgentMessage::Usage(Usage {
            input_tokens: 8119.0,
            output_tokens: 2.0,
            cache_read_tokens: Some(5760.0),
            thought_tokens: Some(11.0),
            total_tokens: Some(13892.0),
            context_tokens: Some(13879.0),
            context_window: Some(65536.0),
            ..Default::default()
        });
        assert_eq!(
            convert_agent_message(&u, Some(" kimi-k2.5 "))
                .unwrap()
                .to_string(),
            r#"{"type":"token_count","model":"kimi-k2.5","usageSchema":"hapi.usage.v1","inputTokenSemantics":"includes-cache","info":{"total":{"inputTokens":13879,"outputTokens":2,"totalTokens":13892,"thoughtTokens":11,"cachedInputTokens":5760},"contextTokens":13879,"modelContextWindow":65536}}"#
        );
        let u = AgentMessage::Usage(Usage {
            input_tokens: 100.0,
            output_tokens: 20.0,
            cache_read_tokens: Some(10.0),
            cache_creation_tokens: Some(5.0),
            ..Default::default()
        });
        let v = convert_agent_message(&u, None).unwrap();
        assert_eq!(v["model"], Value::Null);
        assert_eq!(v["info"]["total"]["inputTokens"], 115);
        assert_eq!(v["info"]["total"]["cacheWriteInputTokens"], 5);
    }

    #[test]
    fn numbers_match_js() {
        assert_eq!(num(3.0).to_string(), "3");
        assert_eq!(num(2.5).to_string(), "2.5");
    }
}
