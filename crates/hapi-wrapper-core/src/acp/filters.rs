//! Agent text filters (TS `cli/src/agent/rateLimitParser.ts` and
//! `cli/src/agent/internalEventFilter.ts`).
use super::types::AgentMessage;
use serde_json::Value;

/// Result of [`parse_rate_limit_text`].
#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum RateLimit {
    Suppress,
    Message(AgentMessage),
}

/// Detects a `rate_limit_event` JSON text and converts it to the
/// pipe-delimited text the web reads. `None`: not a rate-limit event.
pub fn parse_rate_limit_text(text: &str) -> Option<RateLimit> {
    if !text.starts_with('{') {
        return None;
    }
    let parsed: Value = crate::jsjson::parse(text.as_bytes()).ok()?;
    if !parsed.is_object() {
        return None;
    }
    let inner = if parsed["type"] == "output" && parsed["data"].is_object() {
        &parsed["data"]
    } else {
        &parsed
    };
    if inner["type"] != "rate_limit_event" {
        return None;
    }
    let info = &inner["rate_limit_info"];
    if !info.is_object() {
        return None;
    }
    let status = info["status"].as_str();
    if status == Some("allowed") {
        return Some(RateLimit::Suppress);
    }
    let Some(resets_at) = info["resetsAt"].as_f64() else {
        return Some(RateLimit::Suppress);
    };
    let resets = js_round(resets_at);
    let limit_type = info["rateLimitType"].as_str().unwrap_or("");
    match status {
        Some("allowed_warning") => {
            let pct = info["utilization"]
                .as_f64()
                .map_or(0, |u| js_round(u * 100.0));
            Some(RateLimit::Message(AgentMessage::text(format!(
                "Claude AI usage limit warning|{resets}|{pct}|{limit_type}"
            ))))
        }
        Some("rejected") => Some(RateLimit::Message(AgentMessage::text(format!(
            "Claude AI usage limit reached|{resets}|{limit_type}"
        )))),
        _ => Some(RateLimit::Suppress),
    }
}

/// `Math.round` (halves round toward +infinity).
fn js_round(v: f64) -> i64 {
    (v + 0.5).floor() as i64
}

/// Internal session-metadata JSON leaked into agent text:
/// `{type:"output", data:{parentUuid, sessionId, userType, ...}}`.
pub fn is_internal_event_json(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with('{') {
        return false;
    }
    let Ok(parsed) = crate::jsjson::parse::<Value>(trimmed.as_bytes()) else {
        return false;
    };
    if parsed["type"] != "output" || !parsed["data"].is_object() {
        return false;
    }
    let data = &parsed["data"];
    let parent = data.get("parentUuid");
    parent.is_some_and(|p| p.is_string() || p.is_null())
        && data["sessionId"].is_string()
        && data["userType"].is_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_events() {
        assert_eq!(parse_rate_limit_text("hello"), None);
        assert_eq!(parse_rate_limit_text("{not json"), None);
        assert_eq!(parse_rate_limit_text(r#"{"type":"other"}"#), None);
        assert_eq!(
            parse_rate_limit_text(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1}}"#
            ),
            Some(RateLimit::Suppress)
        );
        assert_eq!(
            parse_rate_limit_text(
                r#"{"type":"output","data":{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1700000000.4,"utilization":0.876,"rateLimitType":"five_hour"}}}"#
            ),
            Some(RateLimit::Message(AgentMessage::text(
                "Claude AI usage limit warning|1700000000|88|five_hour"
            )))
        );
        assert_eq!(
            parse_rate_limit_text(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":5}}"#
            ),
            Some(RateLimit::Message(AgentMessage::text(
                "Claude AI usage limit reached|5|"
            )))
        );
        assert_eq!(
            parse_rate_limit_text(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#
            ),
            Some(RateLimit::Suppress)
        );
        assert_eq!(
            parse_rate_limit_text(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"new","resetsAt":5}}"#
            ),
            Some(RateLimit::Suppress)
        );
    }

    #[test]
    fn internal_events() {
        assert!(is_internal_event_json(
            r#"  {"type":"output","data":{"parentUuid":null,"sessionId":"s","userType":"external"}} "#
        ));
        assert!(is_internal_event_json(
            r#"{"type":"output","data":{"parentUuid":"p","sessionId":"s","userType":"u"}}"#
        ));
        assert!(!is_internal_event_json(
            r#"{"type":"output","data":{"sessionId":"s","userType":"u"}}"#
        ));
        assert!(!is_internal_event_json(r#"{"type":"output","data":{}}"#));
        assert!(!is_internal_event_json("plain text"));
        assert!(!is_internal_event_json(r#"{"answer":42}"#));
    }
}
