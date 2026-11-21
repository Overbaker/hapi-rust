use base64::{engine::general_purpose::STANDARD, Engine};
use hapi_wrapper_core::rpc::handlers::GeneratedImages;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

pub struct Messages {
    nonce: Uuid,
    turn: u64,
    sequence: u64,
    active: bool,
    error_emitted: bool,
    segments: BTreeMap<(bool, u64), (String, String)>,
    last: Option<Instant>,
    images: GeneratedImages,
}
impl Messages {
    pub fn new(images: GeneratedImages) -> Self {
        Self {
            nonce: Uuid::new_v4(),
            turn: 0,
            sequence: 0,
            active: false,
            error_emitted: false,
            segments: Default::default(),
            last: None,
            images,
        }
    }
    fn snapshots(&mut self, live: bool) -> Vec<Value> {
        let mut out = Vec::new();
        for ((text, index), (buffer, previous)) in &mut self.segments {
            if buffer.trim().is_empty() || (buffer == previous && (live || !*text)) {
                continue;
            }
            previous.clone_from(buffer);
            let kind = if *text { "text" } else { "reasoning" };
            let id = format!(
                "pi-{}-turn-{}-message-{}-{kind}-{index}",
                self.nonce, self.turn, self.sequence
            );
            use hapi_wrapper_core::{
                acp::types::AgentMessage, agent::message_converter::convert_agent_message,
            };
            let message = if *text {
                AgentMessage::Text {
                    text: buffer.clone(),
                    id: Some(id),
                    live,
                    stream_snapshot: true,
                }
            } else {
                AgentMessage::Reasoning {
                    text: buffer.clone(),
                    id: Some(id),
                    live,
                }
            };
            let row = convert_agent_message(&message, None).unwrap();
            out.push(row);
        }
        out
    }
    pub fn flush(&mut self) -> Vec<Value> {
        let rows = self.snapshots(false);
        self.active = false;
        self.last = None;
        self.segments.clear();
        rows
    }
    pub fn event(&mut self, v: &Value) -> Vec<Value> {
        let mut rows = Vec::new();
        match v["type"].as_str().unwrap_or("") {
            "turn_start" => {
                self.turn += 1;
                self.error_emitted = false;
            }
            "message_start" => {
                rows = self.flush();
                self.active = true;
                self.error_emitted = false;
                self.sequence += 1;
            }
            "message_update" if self.active => {
                let d = &v["assistantMessageEvent"];
                let text = match d["type"].as_str() {
                    Some("text_delta") => true,
                    Some("thinking_delta") => false,
                    _ => return rows,
                };
                if let Some(delta) = d["delta"].as_str().filter(|s| !s.is_empty()) {
                    self.segments
                        .entry((text, d["contentIndex"].as_u64().unwrap_or_default()))
                        .or_default()
                        .0
                        .push_str(delta);
                    if self
                        .last
                        .is_none_or(|t| t.elapsed() >= Duration::from_millis(250))
                    {
                        rows = self.snapshots(true);
                        self.last = Some(Instant::now())
                    }
                }
            }
            "message_end" | "turn_end" => {
                rows = self.flush();
                if !self.error_emitted && v["message"]["stopReason"] == "error" {
                    self.error_emitted = true;
                    let message = v["message"]["errorMessage"]
                        .as_str()
                        .filter(|text| !text.is_empty())
                        .unwrap_or("Pi agent error");
                    rows.push(json!({"type":"error","message":message}));
                }
            }
            "agent_end" => rows = self.flush(),
            "tool_execution_start" | "tool_execution_update"
                if ["toolCallId", "toolName"]
                    .iter()
                    .all(|k| v[*k].as_str().is_some_and(|s| !s.is_empty())) =>
            {
                let mut row = json!({"type":"tool-call","name":v["toolName"],"callId":v["toolCallId"],"status":"in_progress"});
                for (key, target) in [("args", "input"), ("partialResult", "progress")] {
                    if let Some(value) = v.get(key) {
                        row[target] = value.clone()
                    }
                }
                rows.push(row);
            }
            "tool_execution_end"
                if v["isError"].is_boolean()
                    && ["toolCallId", "toolName"]
                        .iter()
                        .all(|k| v[*k].as_str().is_some_and(|s| !s.is_empty())) =>
            {
                let result = &v["result"];
                let mut output = result.clone();
                if let Some(blocks) = result["content"].as_array() {
                    output = json!(blocks
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"));
                    for b in blocks.iter().filter(|b| b["type"] == "image") {
                        let (Some(mime), Some(data)) = (b["mimeType"].as_str(), b["data"].as_str())
                        else {
                            continue;
                        };
                        if !mime.starts_with("image/") || data.len() > (35 << 20) {
                            continue;
                        }
                        let Ok(bytes) = STANDARD.decode(data) else {
                            continue;
                        };
                        let id = Uuid::new_v4().to_string();
                        let actual = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                            "image/png"
                        } else if bytes.starts_with(&[255, 216, 255]) {
                            "image/jpeg"
                        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
                            "image/gif"
                        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
                            "image/webp"
                        } else {
                            continue;
                        };
                        if actual != mime {
                            continue;
                        }
                        let name = format!("generated-{}.png", Uuid::new_v4());
                        if self
                            .images
                            .insert(id.clone(), bytes, mime.into(), name.clone())
                            .is_ok()
                        {
                            rows.push(json!({"type":"generated-image","imageId":id,"id":Uuid::new_v4().to_string(),"fileName":name,"mimeType":mime,"source":{"ingress":"tool_result","toolName":v["toolName"],"toolCallId":v["toolCallId"]}}))
                        }
                    }
                }
                rows.insert(0,json!({"type":"tool-call-result","callId":v["toolCallId"],"output":output,"is_error":v["isError"]==true}));
            }
            _ => {}
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_text_and_error_follow_live_snapshot_once() {
        let mut messages = Messages::new(Default::default());
        messages.event(&json!({"type":"turn_start"}));
        messages.event(&json!({"type":"message_start"}));
        let live=messages.event(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":2,"delta":"partial"}}));
        assert_eq!(live[0]["live"], true);
        let boundary = json!({"type":"message_end","message":{"stopReason":"error","errorMessage":"stream dropped"}});
        let done = messages.event(&boundary);
        assert_eq!(done[0]["id"], live[0]["id"]);
        assert_eq!(done[0]["message"], "partial");
        assert!(done[0].get("live").is_none());
        assert_eq!(done[1], json!({"type":"error","message":"stream dropped"}));
        assert!(messages.event(&boundary).is_empty());
    }
    #[test]
    fn provider_failure_without_a_message_start_is_reported_once_per_turn() {
        let mut messages = Messages::new(Default::default());
        let failure = json!({
            "role": "assistant",
            "stopReason": "error",
            "errorMessage": "request failed"
        });
        for _ in 0..2 {
            messages.event(&json!({"type":"turn_start"}));
            assert_eq!(
                messages.event(&json!({"type":"message_end","message":failure})),
                vec![json!({"type":"error","message":"request failed"})]
            );
            assert!(messages
                .event(&json!({"type":"turn_end","message":failure}))
                .is_empty());
        }
    }
    #[test]
    fn tool_images_must_match_the_declared_mime() {
        let mut messages = Messages::new(Default::default());
        let image = json!({"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="});
        let mut fake = image.clone();
        fake["mimeType"] = "image/gif".into();
        let output=messages.event(&json!({"type":"tool_execution_end","toolCallId":"img","toolName":"display","isError":false,"result":{"content":[{"type":"text","text":"a"},image,fake,{"type":"text","text":"b"}]}}));
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["output"], "a\nb");
        assert_eq!(output[1]["source"]["toolCallId"], "img");
    }
}
