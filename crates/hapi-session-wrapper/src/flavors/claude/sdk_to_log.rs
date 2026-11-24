use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub struct Converter {
    pub session_id: String,
    pub cwd: String,
    pub version: String,
    pub git_branch: Option<String>,
    pub selected_model: Option<String>,
    pub responses: Value,
    last: Option<String>,
    side: HashMap<String, String>,
    model: String,
    key: String,
    windows: HashMap<String, Value>,
    effective: HashSet<String>,
    /// Cumulative cost/modelUsage/duration_api_ms survive --resume, so only
    /// deltas against a known baseline are emitted (TS `previousCumulative`).
    baseline: Option<Baseline>,
    /// Native session id that owns `baseline`; `None` until learned.
    usage_session: Option<String>,
}

/// Per-model `modelUsage` counters that are cumulative across --resume.
const CUMULATIVE: [&str; 6] = [
    "inputTokens",
    "outputTokens",
    "cacheReadInputTokens",
    "cacheCreationInputTokens",
    "webSearchRequests",
    "costUSD",
];
const TOKEN_COUNTERS: [&str; 4] = [
    "inputTokens",
    "outputTokens",
    "cacheReadInputTokens",
    "cacheCreationInputTokens",
];

#[derive(Clone, Default)]
struct Baseline {
    total_cost_usd: Option<f64>,
    duration_api_ms: Option<f64>,
    model_usage: HashMap<String, HashMap<&'static str, f64>>,
    /// Every model with counters is listed, so an absent model is new.
    snapshot_known: bool,
    /// Models whose baseline predates a result that omitted modelUsage.
    stale: HashSet<String>,
    /// A result omitted modelUsage; unseen models may hold earlier counters.
    gap_seen: bool,
    /// Token counters reported earlier that are still absent, per model.
    missing: HashMap<String, HashSet<&'static str>>,
}

/// Per-round delta of a cumulative counter, `None` without a trustworthy one.
fn delta(current: Option<f64>, previous: Option<f64>) -> Option<f64> {
    let d = current? - previous?;
    (d >= 0.0).then_some(d)
}

fn number(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_991.0 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

/// Per-round fields derived from a result's cumulative counters.
#[derive(Default)]
struct RoundUsage {
    total_cost_usd: Option<f64>,
    duration_api_ms: Option<f64>,
    model_usage: Option<Value>,
}
fn copy(out: &mut Value, to: &str, input: &Value, from: &str) {
    if let Some(v) = input.get(from) {
        out[to] = v.clone();
    }
}
fn now() -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = t.as_secs() as libc::time_t;
    let mut time = std::mem::MaybeUninit::uninit();
    let tm = unsafe {
        assert!(!libc::gmtime_r(&seconds, time.as_mut_ptr()).is_null());
        time.assume_init()
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        t.subsec_millis()
    )
}
impl Converter {
    pub fn new(
        session_id: String,
        cwd: String,
        version: Option<String>,
        git_branch: Option<String>,
    ) -> Self {
        let git_branch = git_branch.or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(&cwd)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .filter(|s| !s.is_empty())
        });
        Self {
            session_id,
            cwd,
            version: version
                .or_else(|| std::env::var("npm_package_version").ok())
                .unwrap_or_else(|| "0.0.0".into()),
            git_branch,
            selected_model: None,
            responses: json!({}),
            last: None,
            side: HashMap::new(),
            model: String::new(),
            key: String::new(),
            windows: HashMap::new(),
            effective: HashSet::new(),
            baseline: None,
            usage_session: None,
        }
    }
    pub fn reset_parent_chain(&mut self) {
        self.last = None;
        // The usage baseline is kept: update_session_id clears it when the
        // native session id changes.
    }
    pub fn update_session_id(&mut self, id: &str) {
        self.session_id = id.into();
        // The baseline belongs to one native session: a different id (new
        // session, /clear rotation, fork) cannot be differenced against it.
        if self.usage_session.as_deref().is_some_and(|s| s != id) {
            self.baseline = None;
        }
        self.usage_session = Some(id.into());
    }
    /// Seed a zero baseline for a process started without a prior transcript.
    pub fn start_fresh_process_baseline(&mut self) {
        self.usage_session = None;
        self.baseline = Some(Baseline {
            total_cost_usd: Some(0.0),
            duration_api_ms: Some(0.0),
            snapshot_known: true,
            ..Default::default()
        });
    }
    /// Advance the baseline for a result that produces no transcript entry.
    pub fn consume_result_for_accounting(&mut self, v: &Value) {
        self.consume_cumulative(v);
    }
    /// Diff a result's cumulative fields against the previous raw values and
    /// advance the baseline, even for errored/compaction rounds and backwards
    /// values. Fields without a trustworthy delta are omitted, never raw.
    fn consume_cumulative(&mut self, v: &Value) -> RoundUsage {
        if let (Some(owner), Some(id)) = (&self.usage_session, v["session_id"].as_str()) {
            if owner != id {
                return RoundUsage::default();
            }
        }
        let previous = self.baseline.clone();
        let prev = previous.as_ref();
        let total_cost_usd = delta(
            v["total_cost_usd"].as_f64(),
            prev.and_then(|p| p.total_cost_usd),
        );
        let duration_api_ms = delta(
            v["duration_api_ms"].as_f64(),
            prev.and_then(|p| p.duration_api_ms),
        );
        let raw_usage = v.get("modelUsage");
        let mut model_usage = raw_usage.cloned();
        let mut next_usage = prev.map(|p| p.model_usage.clone()).unwrap_or_default();
        let snapshot_known = prev.is_some_and(|p| p.snapshot_known);
        let gap_seen = prev.is_some_and(|p| p.gap_seen);
        let mut stale = prev.map(|p| p.stale.clone()).unwrap_or_default();
        let mut next_missing = prev.map(|p| p.missing.clone()).unwrap_or_default();
        let mut token_incomplete = !snapshot_known;
        if let Some(models) = raw_usage.and_then(Value::as_object) {
            let mut per_round = serde_json::Map::new();
            for (model, usage) in models {
                let mut per = usage.as_object().cloned().unwrap_or_default();
                let base = prev.and_then(|p| p.model_usage.get(model));
                let is_new = base.is_none() && snapshot_known;
                let untrusted =
                    !snapshot_known || stale.contains(model) || (base.is_none() && gap_seen);
                token_incomplete |= untrusted;
                stale.remove(model);
                let mut counters = if untrusted {
                    HashMap::new()
                } else {
                    base.cloned().unwrap_or_default()
                };
                let mut missing = prev
                    .and_then(|p| p.missing.get(model).cloned())
                    .unwrap_or_default();
                if untrusted {
                    for key in TOKEN_COUNTERS {
                        if base.is_some_and(|b| b.contains_key(key)) {
                            missing.insert(key);
                        }
                    }
                }
                let (mut comparable, mut moved) = (false, false);
                for key in CUMULATIVE {
                    let Some(current) = usage[key].as_f64() else {
                        // A counter that vanished loses its baseline and rebases when it returns.
                        if (counters.remove(key).is_some() || missing.contains(key))
                            && TOKEN_COUNTERS.contains(&key)
                        {
                            missing.insert(key);
                            token_incomplete = true;
                        }
                        continue;
                    };
                    missing.remove(key);
                    counters.insert(key, current);
                    if untrusted {
                        per.remove(key);
                        continue;
                    }
                    if is_new {
                        continue;
                    }
                    match delta(Some(current), base.and_then(|b| b.get(key).copied())) {
                        Some(d) => {
                            per.insert(key.into(), number(d));
                            comparable = true;
                            moved |= d > 0.0;
                        }
                        None => {
                            per.remove(key);
                            token_incomplete |= TOKEN_COUNTERS.contains(&key);
                        }
                    }
                }
                next_usage.insert(model.clone(), counters);
                next_missing.insert(model.clone(), missing);
                // A baseline with no counter movement means this model did no work this round.
                if base.is_some() && comparable && !moved {
                    continue;
                }
                per_round.insert(model.clone(), Value::Object(per));
            }
            // A partial token breakdown would be summed with the gap as zero,
            // so drop token counters and let the round's usage stand in.
            // costUSD keeps its own per-model delta.
            if token_incomplete {
                for per in per_round.values_mut().filter_map(Value::as_object_mut) {
                    for key in TOKEN_COUNTERS {
                        per.remove(key);
                    }
                }
            }
            model_usage = Some(Value::Object(per_round));
        }
        if raw_usage.is_none() {
            stale.extend(next_usage.keys().cloned());
        }
        self.baseline = Some(Baseline {
            total_cost_usd: v["total_cost_usd"].as_f64(),
            duration_api_ms: v["duration_api_ms"].as_f64(),
            model_usage: next_usage,
            snapshot_known: raw_usage.is_some(),
            stale,
            gap_seen: gap_seen || raw_usage.is_none(),
            missing: next_missing,
        });
        RoundUsage {
            total_cost_usd,
            duration_api_ms,
            model_usage,
        }
    }
    pub fn current_context_window(&self) -> Option<&Value> {
        self.windows.get(&self.key)
    }
    pub fn apply_context_usage(&mut self, model: &str, tokens: &Value) {
        if !self.key.is_empty()
            && !model.is_empty()
            && self.model.trim_end_matches("[1m]") == model.trim_end_matches("[1m]")
            && tokens.as_f64().is_some_and(|v| v > 0.0)
        {
            self.windows.insert(self.key.clone(), tokens.clone());
            self.effective.insert(self.key.clone());
        }
    }
    fn init(&mut self, v: &Value) {
        if let Some(id) = v["session_id"].as_str().filter(|s| !s.is_empty()) {
            self.update_session_id(id);
        }
        let Some(model) = v["model"].as_str() else {
            return;
        };
        self.model = model.into();
        let selected = self.selected_model.as_deref().unwrap_or_default();
        self.key = if selected.ends_with("[1m]") && !model.ends_with("[1m]") {
            format!("{model}[1m]")
        } else {
            model.into()
        };
        let large =
            self.key.ends_with("[1m]") || matches!(selected, "opus" | "sonnet" | "fable" | "haiku");
        self.windows
            .entry(self.key.clone())
            .or_insert(json!(if large { 1_000_000 } else { 200_000 }));
    }
    fn base(&self, side: Option<&str>) -> Value {
        let parent = match side {
            Some(id) => self.side.get(id),
            None => self.last.as_ref(),
        };
        let mut out = json!({"uuid":Uuid::new_v4().to_string(),"timestamp":now(),"parentUuid":parent,"isSidechain":side.is_some(),"userType":"external","sessionId":self.session_id,"version":self.version,"cwd":self.cwd});
        if let Some(id) = side {
            out["parentToolUseId"] = json!(id);
        }
        if let Some(branch) = &self.git_branch {
            out["gitBranch"] = json!(branch);
        }
        out
    }
    fn result(&mut self, out: &mut Value, v: &Value) {
        if let Some(usage) = v["modelUsage"].as_object() {
            for (model, u) in usage {
                let key = if model == &self.model {
                    &self.key
                } else {
                    model
                };
                if u["contextWindow"].as_f64().is_some_and(|v| v > 0.0)
                    && !self.effective.contains(key)
                {
                    self.windows.insert(key.clone(), u["contextWindow"].clone());
                }
            }
        }
        out["type"] = json!("system");
        out["subtype"] = json!("turn_duration");
        copy(out, "durationMs", v, "duration_ms");
        // These fields are cumulative across resumes; emit only a trustworthy
        // per-round delta, never a lifetime total.
        let round = if v["hapi_skip_usage"] == true {
            RoundUsage::default()
        } else {
            self.consume_cumulative(v)
        };
        let mut summary = json!({});
        copy(&mut summary, "usage", v, "usage");
        if let Some(u) = round.model_usage {
            summary["modelUsage"] = u;
        }
        if let Some(c) = round.total_cost_usd {
            summary["total_cost_usd"] = number(c);
        }
        for k in ["num_turns", "duration_ms"] {
            copy(&mut summary, k, v, k);
        }
        let generation_ms = round.duration_api_ms.map(number).unwrap_or(Value::Null);
        let measurable = |v: &Value| {
            v.as_f64()
                .is_some_and(|x| x > 0.0 && x.fract() == 0.0 && x <= 9_007_199_254_740_991.0)
        };
        if v["is_error"] != true
            && v["hapi_compact_turn"] != true
            && measurable(&generation_ms)
            && measurable(&v["usage"]["output_tokens"])
        {
            summary["generation_ms"] = generation_ms;
            summary["generation_output_tokens"] = v["usage"]["output_tokens"].clone();
        }
        out["resultSummary"] = summary;
    }
    pub fn convert(&mut self, v: &Value) -> Option<Value> {
        let kind = v["type"].as_str()?;
        if kind == "rate_limit_event" {
            let i = &v["rate_limit_info"];
            let reset = (i["resetsAt"].as_f64()? + 0.5).floor();
            let name = i["rateLimitType"].as_str().unwrap_or("");
            let text = match i["status"].as_str()? {
                "allowed_warning" => format!(
                    "Claude AI usage limit warning|{reset}|{}|{name}",
                    (100. * i["utilization"].as_f64().unwrap_or_default() + 0.5).floor()
                ),
                "rejected" => format!("Claude AI usage limit reached|{reset}|{name}"),
                _ => return None,
            };
            return self.convert(&json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":text}]}}));
        }
        if !matches!(
            kind,
            "user" | "assistant" | "result" | "system" | "tool_result"
        ) {
            return None;
        }
        let side = v["parent_tool_use_id"].as_str().filter(|s| !s.is_empty());
        let mut out = self.base(side);
        let uuid = out["uuid"].as_str().unwrap().to_owned();
        out["type"] = json!(kind);
        match kind {
            "user" => {
                copy(&mut out, "message", v, "message");
                if v["isSynthetic"] == true || v["isMeta"] == true {
                    out["isMeta"] = json!(true);
                }
                for b in v["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "tool_result")
                {
                    self.add_mode(&mut out, b);
                }
            }
            "assistant" => {
                for k in ["message", "requestId", "error"] {
                    copy(&mut out, k, v, k);
                }
                if let (Some(window), Some(usage)) = (
                    self.current_context_window(),
                    out.pointer_mut("/message/usage")
                        .and_then(Value::as_object_mut),
                ) {
                    usage
                        .entry("context_window")
                        .or_insert_with(|| window.clone());
                }
            }
            "system" => {
                if v["subtype"] == "init" {
                    self.init(v);
                }
                out.as_object_mut()?.extend(v.as_object()?.clone());
            }
            "result" => self.result(&mut out, v),
            _ => {
                let mut block = json!({"type":"tool_result"});
                for k in ["tool_use_id", "content"] {
                    copy(&mut block, k, v, k);
                }
                out["type"] = json!("user");
                out["message"] = json!({"role":"user","content":[block]});
                copy(&mut out, "toolUseResult", v, "content");
                self.add_mode(&mut out, v);
            }
        }
        if kind != "result" {
            if let Some(id) = side {
                self.side.insert(id.into(), uuid.clone());
            }
            self.last = Some(uuid);
        }
        Some(out)
    }
    fn add_mode(&self, out: &mut Value, b: &Value) {
        if let Some(mode) = b["tool_use_id"]
            .as_str()
            .and_then(|id| self.responses[id]["mode"].as_str())
            .filter(|s| !s.is_empty())
        {
            out["mode"] = json!(mode);
        }
    }
    pub fn sidechain_user(&mut self, id: &str, text: &str) -> Value {
        let mut out = self.base(Some(id));
        out.as_object_mut().unwrap().remove("parentToolUseId");
        out["parentUuid"] = Value::Null;
        self.side
            .insert(id.into(), out["uuid"].as_str().unwrap().into());
        out["type"] = json!("user");
        out["message"] = json!({"role":"user","content":text});
        out
    }
    pub fn interrupted(&mut self, id: &str, parent: Option<&str>) -> Value {
        let text = "[Request interrupted by user for tool use]";
        let mut out=self.convert(&json!({"type":"user","parent_tool_use_id":parent,"message":{"role":"user","content":[{"type":"tool_result","content":text,"is_error":true,"tool_use_id":id}]}})).unwrap();
        out.as_object_mut().unwrap().remove("mode");
        out["toolUseResult"] = json!(format!("Error: {text}"));
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn uuid_chain(events in prop::collection::vec((0u8..5,0u8..4),0..100)){
            let mut c=Converter::new("s".into(),".".into(),None,Some("branch".into()));
            let mut last=Value::Null;let mut sides=HashMap::new();
            for (kind,side) in events{
                let id=(side>0).then(||side.to_string());
                let kind=["assistant","user","result","tool_progress","unrecognized"][kind as usize];
                let row=c.convert(&json!({"type":kind,"parent_tool_use_id":id}));
                if matches!(kind,"tool_progress"|"unrecognized"){prop_assert!(row.is_none());continue;}
                let row=row.unwrap();let parent=if side==0{&last}else{sides.get(&side).unwrap_or(&Value::Null)};
                prop_assert_eq!(&row["parentUuid"],parent);
                if kind!="result"{last=row["uuid"].clone();sides.insert(side,last.clone());}
            }
        }
    }
}
#[cfg(test)]
#[test]
fn typescript_converter_suite() {
    use std::{
        io::{BufRead, BufReader, Write},
        process::Stdio,
    };
    let mut child = Command::new("bun")
        .env("HAPI_RUST_CONVERTER_TEST", "1")
        .args(["test", "src/flavors/claude/converter_parity.test.ts"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut converters = Vec::new();
    let mut stdin = child.stdin.take().unwrap();
    let mut calls = 0;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if !line.starts_with('[') {
            continue;
        }
        let args: Value = serde_json::from_str(&line).unwrap();
        calls += 1;
        let action = args[0].as_str().unwrap();
        let result = if action == "new" {
            let ctx = &args[1];
            let field = |k: &str| ctx[k].as_str().map(str::to_owned);
            let mut c = Converter::new(
                field("sessionId").unwrap(),
                field("cwd").unwrap(),
                field("version"),
                field("gitBranch"),
            );
            c.selected_model = field("selectedModel");
            c.responses = args[2].clone();
            converters.push(c);
            Some(json!(converters.len() - 1))
        } else {
            let c = &mut converters[args[1].as_u64().unwrap() as usize];
            match action {
                "convert" => Some(c.convert(&args[2]).unwrap_or(Value::Null)),
                "window" => c.current_context_window().cloned(),
                "usage" => {
                    c.apply_context_usage(args[2].as_str().unwrap_or(""), &args[3]);
                    None
                }
                "select" => {
                    c.selected_model = args[2].as_str().map(str::to_owned);
                    None
                }
                "reset" => {
                    c.reset_parent_chain();
                    None
                }
                "session" => {
                    c.update_session_id(args[2].as_str().unwrap());
                    None
                }
                "fresh" => {
                    c.start_fresh_process_baseline();
                    None
                }
                "consume" => {
                    c.consume_result_for_accounting(&args[2]);
                    None
                }
                "interrupt" => Some(c.interrupted(args[2].as_str().unwrap(), args[3].as_str())),
                _ => panic!("unknown action {action}"),
            }
        };
        writeln!(
            stdin,
            "{}",
            result.map(|v| json!({"value":v})).unwrap_or(json!({}))
        )
        .unwrap();
    }
    assert!(child.wait().unwrap().success());
    assert!(calls > 100);
}
