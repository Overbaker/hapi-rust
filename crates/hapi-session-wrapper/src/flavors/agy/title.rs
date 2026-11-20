//! agy session titles: the prompt fallback (TS `agySessionTitle.ts`) and the
//! ACP native title sync (TS `acpSessionTitle.ts`, `nativeSessionTitle.ts`).
//! agy's `conversation_summaries.db` is not read (design A17).
use hapi_wrapper_core::session::{client::SessionClient, metadata::now};
use regex_lite::{Captures, Regex};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

const MAX_TITLE_LENGTH: usize = 60;
const URL: &str = r"(?i)https?://[^\s]+";

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap()
}

/// `None` for empty and placeholder titles (TS `normalizeNativeSessionTitle`).
pub fn normalize_native_title(title: &str) -> Option<String> {
    let t = title.trim();
    (!t.is_empty()
        && !re(r"(?i)^(?:Untitled|New Session)$").is_match(t)
        && !re(r"(?i)^(?:New|Child) session - \d{4}-\d{2}-\d{2}T").is_match(t))
    .then(|| t.to_owned())
}

/// `host/path` of a URL (TS `new URL()` hostname + pathname without trailing slashes).
fn host_and_path(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let rest = &rest[..end];
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = if host.starts_with('[') {
        host.split_inclusive(']').next().unwrap_or(host)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    format!("{}{}", host.to_lowercase(), path.trim_end_matches('/'))
}

/// Strips URLs, code and Markdown noise from a prompt, for a session title.
pub fn clean_prompt_for_title(raw: &str) -> Option<String> {
    let text = re(r"(?s)```.*?```").replace_all(raw, " ");
    let session_url = re(r"(?i)/sessions/[0-9a-f-]+|session://");
    let bare_id = re(r"(?i)^[0-9a-f]{8,}(?:-[0-9a-f]{4,})*$");
    let text = re(r"\[([^\]]+)\]\(([^)]*)\)").replace_all(&text, |c: &Captures| {
        let label = c[1].trim();
        if session_url.is_match(&c[2]) && bare_id.is_match(label) {
            " ".to_owned()
        } else {
            format!(" {label} ")
        }
    });
    let text = re(r"`([^`]+)`").replace_all(&text, "$1");
    let spaces = re(r"\s+");
    let without_urls = spaces
        .replace_all(&re(URL).replace_all(&text, " "), " ")
        .trim()
        .to_owned();
    let text = if re(r"[一-龥]|\b[a-zA-Z0-9_-]{2,}\b").is_match(&without_urls) {
        without_urls
    } else {
        match re(URL).find(raw) {
            Some(m) => host_and_path(m.as_str()),
            None => without_urls,
        }
    };
    let text = re(r"^[#>*+\s:=-]+").replace(&text, "");
    let text = spaces.replace_all(&text, " ").trim().to_owned();
    if text.is_empty() {
        return None;
    }
    let text = if text.chars().count() > MAX_TITLE_LENGTH {
        let head: String = text.chars().take(MAX_TITLE_LENGTH - 1).collect();
        format!("{}…", head.trim_end())
    } else {
        text
    };
    normalize_native_title(&text)
}

/// A missing summary, or a raw URL left by an older session.
pub fn is_broken_or_missing_summary_title(value: &Value) -> bool {
    match value.as_str().map(str::trim) {
        Some(t) if !t.is_empty() => re(r"(?i)^https?://\S+$").is_match(t),
        _ => true,
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => true,
    }
}

fn with_fallback_title(m: &Value, title: &str, at: u64) -> Value {
    if m["name"].as_str().is_some_and(|n| !n.trim().is_empty())
        || truthy(&m["acpManualTitle"])
        || !is_broken_or_missing_summary_title(&m["summary"]["text"])
    {
        return m.clone();
    }
    let mut next = m.clone();
    next["summary"] = json!({"text":title,"updatedAt":at});
    next
}

/// Sets a cleaned prompt as the title while the session has none; false when
/// the prompt yields no title.
pub fn apply_title_fallback(client: &SessionClient, prompt: &str) -> bool {
    let Some(title) = clean_prompt_for_title(prompt) else {
        return false;
    };
    client
        .metadata
        .update(move |m| with_fallback_title(m, &title, now()));
    true
}

/// Normalized, deduplicated sink for agy's native titles
/// (`session_info_update`, `session/list`).
pub struct TitleSync {
    last: Mutex<Option<String>>,
    manual: AtomicBool,
}
impl TitleSync {
    pub fn new(client: &SessionClient) -> Self {
        Self {
            last: Mutex::new(None),
            manual: (client.metadata.get().value["acpManualTitle"] == true).into(),
        }
    }
    /// Publishes a native title as a transcript summary unless the user renamed
    /// the session.
    pub fn sync(&self, client: &SessionClient, title: Option<&str>) {
        if self.manual.load(Ordering::SeqCst) {
            return;
        }
        let Some(title) = title.and_then(normalize_native_title) else {
            return;
        };
        let mut last = self.last.lock().unwrap();
        if last.as_deref() == Some(title.as_str()) {
            return;
        }
        *last = Some(title.clone());
        drop(last);
        client.send_claude_session_message(&json!({
            "type":"summary","summary":title,"leafUuid":uuid::Uuid::new_v4().to_string()
        }));
    }
    /// A manual rename wins over later native titles, also after a resume.
    pub fn mark_manual(&self, client: &SessionClient) {
        if self.manual.swap(true, Ordering::SeqCst) {
            return;
        }
        client.metadata.update(|m| {
            let mut m = m.clone();
            m["acpManualTitle"] = true.into();
            m
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_prompts() {
        assert_eq!(
            clean_prompt_for_title(
                "https://example.com/private?token=secret\n检查 [会话](/sessions/1234abcd) 的错误"
            )
            .as_deref(),
            Some("检查 会话 的错误")
        );
        assert_eq!(
            clean_prompt_for_title("https://example.com/path?token=secret#fragment").as_deref(),
            Some("example.com/path")
        );
        assert_eq!(
            clean_prompt_for_title(&"😀".repeat(100)),
            Some("😀".repeat(59) + "…")
        );
        assert_eq!(
            clean_prompt_for_title("see [abcdef12-3456](session://x) `code` here").as_deref(),
            Some("see code here")
        );
        assert_eq!(
            clean_prompt_for_title("## - Fix ```let x = 1;``` it").as_deref(),
            Some("Fix it")
        );
        assert_eq!(clean_prompt_for_title("   "), None);
        assert_eq!(clean_prompt_for_title("New Session"), None);
    }

    #[test]
    fn fallback_keeps_existing_titles() {
        for existing in [
            json!({"name":"My title"}),
            json!({"summary":{"text":"Native title","updatedAt":1}}),
            json!({"acpManualTitle":true}),
        ] {
            assert_eq!(with_fallback_title(&existing, "new prompt", 5), existing);
        }
        assert_eq!(
            with_fallback_title(&json!({}), "Fix the problem", 5),
            json!({"summary":{"text":"Fix the problem","updatedAt":5}})
        );
        assert_eq!(
            with_fallback_title(&json!({"summary":{"text":"https://a.b/c"}}), "T", 5)["summary"]
                ["text"],
            "T"
        );
    }

    #[test]
    fn normalizes_native_titles() {
        assert_eq!(
            normalize_native_title("  Native  ").as_deref(),
            Some("Native")
        );
        for t in [
            "",
            "untitled",
            "New Session",
            "New session - 2026-01-01T00:00",
            "Child session - 2026-01-01T1",
        ] {
            assert_eq!(normalize_native_title(t), None);
        }
        assert!(is_broken_or_missing_summary_title(&Value::Null));
        assert!(!is_broken_or_missing_summary_title(&json!("ok")));
    }
}
