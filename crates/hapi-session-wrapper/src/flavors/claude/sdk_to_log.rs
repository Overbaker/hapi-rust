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
        }
    }
    pub fn reset_parent_chain(&mut self) {
        self.last = None;
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
            self.session_id = id.into();
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
        let mut summary = json!({});
        for k in [
            "usage",
            "modelUsage",
            "total_cost_usd",
            "num_turns",
            "duration_ms",
        ] {
            copy(&mut summary, k, v, k);
        }
        let measurable = |v: &Value| {
            v.as_f64()
                .is_some_and(|x| x > 0.0 && x.fract() == 0.0 && x <= 9_007_199_254_740_991.0)
        };
        if v["is_error"] != true
            && v["hapi_compact_turn"] != true
            && measurable(&v["duration_api_ms"])
            && measurable(&v["usage"]["output_tokens"])
        {
            summary["generation_ms"] = v["duration_api_ms"].clone();
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
