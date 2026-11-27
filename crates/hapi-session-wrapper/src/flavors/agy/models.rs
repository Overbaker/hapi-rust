//! The agy model catalogue from `agy models`, with the hardcoded mirror as a
//! fallback (TS `modules/common/agyModels.ts` + `handlers/agyModels.ts`).
use super::version::capture;
use hapi_wrapper_core::{jsjson::parse as parse_json, rpc::manager::RpcHandlerManager};
use regex_lite::Regex;
use serde_json::{json, Value};
use std::{
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::process::Command;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const FRESH_TTL: Duration = Duration::from_secs(10 * 60);
const STALE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const PROBE_BACKOFF: Duration = Duration::from_secs(60);
const AUTH_REQUIRED_PATTERNS: [&str; 3] = [
    "Authentication required",
    "Please sign in",
    "accounts.google.com/o/oauth2/auth",
];
const AUTH_ERROR: &str =
    "Authentication required. Please run `agy` in a terminal to sign in with Google.";

/// `AGY_MODEL_LABELS` (shared/src/models.ts), in agy's listing order.
pub const AGY_MODEL_LABELS: [(&str, &str); 14] = [
    ("gemini-3.7-flash-high", "Gemini 3.7 Flash (High)"),
    ("gemini-3.7-flash-medium", "Gemini 3.7 Flash (Medium)"),
    ("gemini-3.7-flash-low", "Gemini 3.7 Flash (Low)"),
    ("gemini-3.6-flash-high", "Gemini 3.6 Flash (High)"),
    ("gemini-3.6-flash-medium", "Gemini 3.6 Flash (Medium)"),
    ("gemini-3.6-flash-low", "Gemini 3.6 Flash (Low)"),
    ("gemini-3.5-flash-medium", "Gemini 3.5 Flash (Medium)"),
    ("gemini-3.5-flash-high", "Gemini 3.5 Flash (High)"),
    ("gemini-3.5-flash-low", "Gemini 3.5 Flash (Low)"),
    ("gemini-3.1-pro-low", "Gemini 3.1 Pro (Low)"),
    ("gemini-3.1-pro-high", "Gemini 3.1 Pro (High)"),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6 (Thinking)"),
    ("claude-opus-4-6-thinking", "Claude Opus 4.6 (Thinking)"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
];

fn label_of(id: &str) -> Option<&'static str> {
    AGY_MODEL_LABELS
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, l)| *l)
}
fn id_of(name: &str) -> Option<&'static str> {
    AGY_MODEL_LABELS
        .iter()
        .find(|(_, l)| *l == name)
        .map(|(k, _)| *k)
}
fn entry(id: &str, name: Option<&str>) -> Value {
    match name {
        Some(n) => json!({"modelId":id,"name":n}),
        None => json!({"modelId":id}),
    }
}
fn mirror() -> Vec<Value> {
    AGY_MODEL_LABELS
        .iter()
        .map(|(id, name)| entry(id, Some(name)))
        .collect()
}

/// Best-effort id from a display name: lowercase, spaces→dashes,
/// "(Variant)"→"-variant"; Claude versions use dashes (4.6→4-6).
fn derive_agy_id(name: &str) -> String {
    let id = name.trim().to_lowercase();
    let id = Regex::new(r"\s*\(([^)]+)\)\s*$")
        .unwrap()
        .replace(&id, "-$1")
        .into_owned();
    let mut id = Regex::new(r"\s+")
        .unwrap()
        .replace_all(&id, "-")
        .into_owned();
    if id.starts_with("claude") {
        id = Regex::new(r"(\d)\.(\d)")
            .unwrap()
            .replace_all(&id, "$1-$2")
            .into_owned();
    }
    Regex::new("-+").unwrap().replace_all(&id, "-").into_owned()
}

/// agy's structured listing (`--output-format=json`): `command.data.models[{id,label}]`.
pub fn parse_agy_models_json(output: &str) -> Option<Vec<Value>> {
    for line in output.split('\n').map(str::trim) {
        if !line.starts_with('{') {
            continue;
        }
        let Ok(payload) = parse_json::<Value>(line.as_bytes()) else {
            continue;
        };
        let Some(entries) = payload["command"]["data"]["models"].as_array() else {
            continue;
        };
        let mut models: Vec<Value> = Vec::new();
        for e in entries {
            let Some(id) = e["id"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            if models.iter().any(|m| m["modelId"] == id) {
                continue;
            }
            models.push(entry(id, e["label"].as_str().filter(|s| !s.is_empty())));
        }
        if !models.is_empty() {
            return Some(models);
        }
    }
    None
}

/// The printed `agy models` table (TTY columns, piped tab rows, bare ids or
/// legacy display names), in agy's order.
pub fn parse_agy_models_output(output: &str) -> Option<Vec<Value>> {
    let clean = Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]")
        .unwrap()
        .replace_all(output, "")
        .replace('\r', "\n");
    let columns = Regex::new(r"(?i)^([a-z0-9][a-z0-9._/-]*)(?:\t+| {2,})(.+)$").unwrap();
    let bare_id = Regex::new(r"(?i)^[a-z0-9][a-z0-9._/-]*$").unwrap();
    let model_like = Regex::new(r"^[A-Za-z][\w./ -]*\([^)]+\)$").unwrap();
    let mut models: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut add = |id: &str, name: Option<&str>| {
        if !seen.iter().any(|s| s == id) {
            seen.push(id.to_owned());
            models.push(entry(id, name));
        }
    };
    for line in clean.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(c) = columns.captures(line) {
            add(&c[1], Some(c[2].trim()));
        } else if bare_id.is_match(line) && line.contains('-') {
            add(line, label_of(line));
        } else if id_of(line).is_some() || model_like.is_match(line) {
            let id = id_of(line).map_or_else(|| derive_agy_id(line), str::to_owned);
            add(&id, Some(line));
        }
    }
    (!models.is_empty()).then_some(models)
}

fn auth_error(output: &str) -> bool {
    AUTH_REQUIRED_PATTERNS.iter().any(|p| output.contains(p))
}

#[derive(Clone, Debug, PartialEq)]
enum Fetch {
    Live(Vec<Value>),
    AuthError,
    Unavailable,
}

/// One `agy` invocation; both streams, since the auth failure may be on either.
async fn probe(args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("agy");
    cmd.args(args).env("GEMINI_FORCE_FILE_STORAGE", "true");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("SSH_") {
            cmd.env_remove(key);
        }
    }
    let c = capture(cmd, PROBE_TIMEOUT, Duration::from_millis(500))
        .await
        .ok()??;
    Some(c.stdout + &c.stderr)
}

async fn fetch() -> Fetch {
    // `--output-format` is global and only accepted in the `=` form before the
    // subcommand; older releases ignore it and print the table.
    let Some(output) = probe(&["--output-format=json", "models"]).await else {
        return Fetch::Unavailable;
    };
    if auth_error(&output) {
        return Fetch::AuthError;
    }
    if let Some(m) = parse_agy_models_json(&output).or_else(|| parse_agy_models_output(&output)) {
        return Fetch::Live(m);
    }
    let Some(output) = probe(&["models"]).await else {
        return Fetch::Unavailable;
    };
    if auth_error(&output) {
        return Fetch::AuthError;
    }
    parse_agy_models_output(&output).map_or(Fetch::Unavailable, Fetch::Live)
}

#[derive(Default)]
struct State {
    cached: Option<(Vec<Value>, Instant)>,
    failed: Option<(Instant, Fetch)>,
    generation: u64,
    last: Option<Fetch>,
}
static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static PROBE: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

impl State {
    fn servable(&self) -> Option<Value> {
        let (models, _) = self
            .cached
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STALE_TTL)?;
        let mut reply = json!({"success":true,"availableModels":models});
        if matches!(self.failed, Some((_, Fetch::AuthError))) {
            reply["error"] = json!(AUTH_ERROR);
        }
        Some(reply)
    }
    fn response(&self, fetched: &Fetch) -> Value {
        if let Fetch::Live(models) = fetched {
            return json!({"success":true,"availableModels":models});
        }
        // A failed probe is not evidence that the catalog changed.
        if let Some(cached) = self.servable() {
            return cached;
        }
        match fetched {
            Fetch::AuthError => json!({"success":false,"error":AUTH_ERROR}),
            _ => json!({"success":true,"availableModels":mirror()}),
        }
    }
}

async fn refresh(force: bool) -> Fetch {
    let seen = STATE.lock().unwrap().generation;
    let _probe = PROBE.lock().await;
    {
        let s = STATE.lock().unwrap();
        // A probe finished while this call waited: answer from it, unless the
        // user asked for a new one (it predates what they just fixed).
        if !force && s.generation != seen {
            if let Some(last) = &s.last {
                return last.clone();
            }
        }
        if let Some((at, result)) = &s.failed {
            if !force && at.elapsed() < PROBE_BACKOFF {
                return result.clone();
            }
        }
    }
    let fetched = fetch().await;
    let mut s = STATE.lock().unwrap();
    if let Fetch::Live(models) = &fetched {
        s.cached = Some((models.clone(), Instant::now()));
        s.failed = None;
    } else {
        s.failed = Some((Instant::now(), fetched.clone()));
    }
    s.generation += 1;
    s.last = Some(fetched.clone());
    fetched
}

/// The `ListAgyModelsResponse` JSON. `refresh` (the picker's Retry) always
/// costs a probe; otherwise a servable listing is answered at once and
/// revalidated in the background when stale or after a failure.
pub async fn list_agy_models(refresh_requested: bool) -> Value {
    if refresh_requested {
        let fetched = refresh(true).await;
        return STATE.lock().unwrap().response(&fetched);
    }
    let cached = {
        let s = STATE.lock().unwrap();
        s.servable().map(|reply| {
            let fresh = s
                .cached
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < FRESH_TTL);
            (reply, !fresh || s.failed.is_some())
        })
    };
    if let Some((reply, revalidate)) = cached {
        if revalidate {
            tokio::spawn(refresh(false));
        }
        return reply;
    }
    let fetched = refresh(false).await;
    STATE.lock().unwrap().response(&fetched)
}

/// Registers `listAgyModels` (`{refresh?: boolean}`).
pub fn register(rpc: &RpcHandlerManager) {
    rpc.register("listAgyModels", |v| async move {
        Ok(Some(list_agy_models(v["refresh"] == true).await))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing() -> String {
        json!({"conversation_id":"","status":"SUCCESS","command":{"name":"models","data":{"models":[
            {"id":"gemini-3.7-flash-high","label":"Gemini 3.7 Flash (High)"},
            {"id":"gemini-3.7-flash-medium","label":"Gemini 3.7 Flash (Medium)"},
            {"id":"gemini-3.6-flash-low","label":"Gemini 3.6 Flash (Low)"},
            {"id":"gpt-oss-120b-medium","label":"GPT-OSS 120B (Medium)"}]}}})
        .to_string()
    }

    #[test]
    fn parses_structured_listing() {
        let expected = json!([
            {"modelId":"gemini-3.7-flash-high","name":"Gemini 3.7 Flash (High)"},
            {"modelId":"gemini-3.7-flash-medium","name":"Gemini 3.7 Flash (Medium)"},
            {"modelId":"gemini-3.6-flash-low","name":"Gemini 3.6 Flash (Low)"},
            {"modelId":"gpt-oss-120b-medium","name":"GPT-OSS 120B (Medium)"}
        ]);
        assert_eq!(json!(parse_agy_models_json(&listing())), expected);
        let with_progress = format!("Fetching available models...\n{}\n", listing());
        assert_eq!(json!(parse_agy_models_json(&with_progress)), expected);
        for other in [
            "gemini-3.6-flash-high\tGemini 3.6 Flash (High)\n",
            r#"{"command":{"data":{"models":[]}}}"#,
            r#"{"command":{"name":"models"}}"#,
            "{ truncated",
        ] {
            assert_eq!(parse_agy_models_json(other), None);
        }
        assert_eq!(
            json!(parse_agy_models_json(
                r#"{"command":{"data":{"models":[{"id":"gemini-9-future"}]}}}"#
            )),
            json!([{"modelId":"gemini-9-future"}])
        );
    }

    #[test]
    fn parses_printed_table() {
        assert_eq!(
            json!(parse_agy_models_output("gemini-3.6-flash-high     Gemini 3.6 Flash (High)\r\nclaude-sonnet-4-6         Claude Sonnet 4.6 (Thinking)")),
            json!([{"modelId":"gemini-3.6-flash-high","name":"Gemini 3.6 Flash (High)"},{"modelId":"claude-sonnet-4-6","name":"Claude Sonnet 4.6 (Thinking)"}])
        );
        assert_eq!(
            json!(parse_agy_models_output("Gemini 3.5 Flash (High)\n")),
            json!([{"modelId":"gemini-3.5-flash-high","name":"Gemini 3.5 Flash (High)"}])
        );
        assert_eq!(
            json!(parse_agy_models_output(
                "gemini-3.6-flash-high\ngemini-3.6-flash-low\n"
            )),
            json!([{"modelId":"gemini-3.6-flash-high","name":"Gemini 3.6 Flash (High)"},{"modelId":"gemini-3.6-flash-low","name":"Gemini 3.6 Flash (Low)"}])
        );
        assert_eq!(
            json!(parse_agy_models_output("gemini-9.9-experimental\n")),
            json!([{"modelId":"gemini-9.9-experimental"}])
        );
        assert_eq!(
            json!(parse_agy_models_output(
                "Fetching available models...\ngemini-3.7-flash-low\tGemini 3.7 Flash (Low)\n"
            )),
            json!([{"modelId":"gemini-3.7-flash-low","name":"Gemini 3.7 Flash (Low)"}])
        );
        assert_eq!(
            json!(parse_agy_models_output(
                "\x1b[1mClaude Opus 5.1 (Thinking)\x1b[0m\n"
            )),
            json!([{"modelId":"claude-opus-5-1-thinking","name":"Claude Opus 5.1 (Thinking)"}])
        );
        assert_eq!(
            parse_agy_models_output("Fetching available models...\n"),
            None
        );
    }

    #[test]
    fn responses() {
        let s = State::default();
        assert_eq!(
            s.response(&Fetch::AuthError),
            json!({"success":false,"error":AUTH_ERROR})
        );
        assert_eq!(
            s.response(&Fetch::Unavailable)["availableModels"]
                .as_array()
                .unwrap()
                .len(),
            14
        );
        let s = State {
            cached: Some((vec![json!({"modelId":"m"})], Instant::now())),
            failed: Some((Instant::now(), Fetch::AuthError)),
            ..Default::default()
        };
        assert_eq!(
            s.response(&Fetch::Unavailable),
            json!({"success":true,"availableModels":[{"modelId":"m"}],"error":AUTH_ERROR})
        );
        assert!(auth_error("x Please sign in y"));
    }
}
