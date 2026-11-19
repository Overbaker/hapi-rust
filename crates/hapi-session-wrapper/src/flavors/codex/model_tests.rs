use super::*;
use hapi_wrapper_core::rpc::handlers::GeneratedImages;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};
type Update = Box<dyn Fn(&Value) -> Value + Send + Sync>;
#[derive(Default)]
struct Sink {
    effects: Vec<Value>,
    updates: Vec<Update>,
    root: String,
    aliases: Vec<Value>,
    metadata: Value,
    thread: Value,
    lookup: compaction_summary::Lookup,
    images: GeneratedImages,
    ledger: generation_ledger::GenerationLedger,
    clock: round_duration::RoundDuration,
}
impl Sink {
    fn update(&mut self, kind: &str, f: Update) {
        let n = self.updates.len();
        self.updates.push(f);
        self.effects.push(json!([kind, n]));
    }
}
impl projection::Host for Sink {
    fn current_metadata(&self) -> Value {
        self.metadata.clone()
    }
    fn agent(&mut self, id: &str, v: Value) {
        self.effects.push(json!(["sendAgentMessage", v, id]));
    }
    fn user(&mut self, id: &str, s: &str) {
        self.effects.push(json!(["sendUserMessage", s, null, id]));
    }
    fn status(&mut self, v: Value) {
        self.effects.push(json!(["sendSessionEvent", v]));
    }
    fn metadata(&mut self, f: Update) {
        self.update("updateMetadata", f);
    }
    fn committed(&mut self, _: &str, _: &Value) -> Result<Option<Vec<Value>>, String> {
        let v = self.aliases.pop().expect("unexpected commit");
        Ok(v.as_array().cloned())
    }
    fn observed(&mut self, t: &str, i: &Value) {
        self.effects.push(json!(["observe", t, i]));
    }
    fn generation_event(&mut self, e: &Value, at: i64) {
        self.ledger.observe(e, at);
    }
    fn turn_clock(&mut self, t: &str, at: i64, ok: Option<bool>) -> Option<Value> {
        if let Some(ok) = ok {
            let g = self.ledger.settle(t, at, ok);
            self.clock
                .finish(t, at, ok)
                .map(|v| super::value::merge(v, &g.unwrap_or(Value::Null)))
        } else {
            self.clock.start(t, at);
            self.ledger.start(t, at);
            None
        }
    }
    fn image(&mut self, p: &str, id: &str, name: Option<&str>) -> Option<Value> {
        root::output::image(&self.images, p, id, name)
    }
    fn compaction(&mut self, _: &str, t: &str, i: &str, _: bool) -> Option<Value> {
        self.lookup.probe(&self.thread, t, i).ok().flatten()
    }
}
impl permissions::Host for Sink {
    fn root_thread(&self) -> &str {
        &self.root
    }
    fn send(&mut self, id: &str, v: Value) {
        self.effects.push(json!(["sendAgentMessage", v, id]));
    }
    fn state(&mut self, f: Update) {
        self.update("updateAgentState", f);
    }
    fn wait_started(&mut self, t: &str, k: &str) {
        self.effects.push(json!(["waitStart", t, k]));
    }
    fn wait_ended(&mut self, t: &str, k: &str) {
        self.effects.push(json!(["waitEnd", t, k]));
    }
}
struct Model {
    sink: Sink,
    projection: projection::Projection,
    permissions: permissions::Permissions,
    questions: async_questions::AsyncQuestions,
}
impl Model {
    fn call(&mut self, op: &str, a: &Value) -> Result<Value, String> {
        let p = &mut self.projection;
        let sink = &mut self.sink;
        let val = &a[0];
        let s = val.as_str().unwrap_or("");
        let id = &a[1];
        Ok(match op {
            "apply" => (sink.updates[val.as_u64().unwrap() as usize])(id),
            "titleRevision" => json!(p.title_revision()),
            "notify" | "history" => {
                sink.metadata = a[5].clone();
                sink.aliases = a[4].as_array().unwrap().iter().rev().cloned().collect();
                if a.get(6).is_some() {
                    sink.thread = a[6].clone();
                }
                if op == "notify" {
                    p.notification(sink, s, id, a[2].as_str(), a[3].as_i64().unwrap())?;
                } else {
                    if val["id"] == p.thread {
                        sink.thread = val.clone();
                    }
                    p.history_at(sink, val, id.as_u64().unwrap())?;
                }
                assert!(sink.aliases.is_empty());
                Value::Null
            }
            "turnFor" => json!(p.turn_for(s)),
            "seed" => {
                sink.ledger.usage(0);
                Value::Null
            }
            "beginWait" => json!(sink.ledger.begin_wait(val.as_i64().unwrap())),
            "endWait" => {
                sink.ledger
                    .end_wait(val.as_u64().unwrap(), id.as_i64().unwrap());
                Value::Null
            }
            "reset" | "hubReset" => {
                p.reset();
                if op == "reset" {
                    sink.ledger.reset();
                }
                Value::Null
            }
            "receive" => self
                .permissions
                .receive(
                    sink,
                    &val["id"],
                    val["method"].as_str().unwrap(),
                    &val["params"],
                    42,
                )
                .map_or(Value::Null, |v| json!({"id":val["id"],"response":v})),
            "permReply" => match self.permissions.submit(
                sink,
                &serde_json::from_value(val.clone()).map_err(|e| e.to_string())?,
            )? {
                permissions::Submission::Response(id, v) => json!({"id":id,"response":v}),
                permissions::Submission::Interrupt(v) => json!({"interrupt":v}),
            },
            "resolved" => {
                self.permissions
                    .resolved(sink, s, id, a[2] == "canceled", 43);
                Value::Null
            }
            "permClose" => {
                self.permissions.close(sink, 44);
                Value::Null
            }
            "questions" => json!(async_questions::questions(s, id)),
            "observe" => {
                self.questions.observe(sink, s, id, 42);
                Value::Null
            }
            "qHistory" => {
                self.questions.history(sink, s, id, 42);
                Value::Null
            }
            "qAnswer" => match self.questions.answer(
                sink,
                &serde_json::from_value(val.clone()).map_err(|e| e.to_string())?,
                43,
            )? {
                async_questions::AsyncAnswer::Other => json!(false),
                async_questions::AsyncAnswer::Dismissed => json!(true),
                async_questions::AsyncAnswer::Submit {
                    thread,
                    text,
                    client_id,
                } => json!([thread, text, client_id]),
            },
            "submitFailed" => {
                self.questions.submit_failed(s, id == true);
                Value::Null
            }
            "qClose" => {
                self.questions.close(sink, 44);
                Value::Null
            }
            "inputText" => json!(projection::input_text(val)),
            _ => panic!("{op}"),
        })
    }
}
#[test]
fn original_projection_and_permission_suites() {
    let mut child = Command::new("bun")
        .args(["test", "src/flavors/codex/model_parity.test.ts"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HAPI_RUST_CODEX_MODELS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut models = HashMap::new();
    let mut count = 0;
    for line in BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
    {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        count += 1;
        let n = v[0].as_u64().unwrap();
        let op = v[1].as_str().unwrap();
        let args = &v[2];
        let model = models.entry(n).or_insert_with(|| {
            let t = args[0].as_str().unwrap_or("").to_owned();
            Model {
                sink: Sink {
                    root: args[3].as_str().unwrap_or("").into(),
                    lookup: compaction_summary::Lookup::new(t.clone()),
                    ..Default::default()
                },
                projection: projection::Projection::new(
                    t,
                    args[1].as_str().map(str::to_owned),
                    &args[2],
                ),
                permissions: permissions::Permissions::new(args[4].as_str().unwrap_or("").into()),
                questions: Default::default(),
            }
        });
        let result = if op == "new" {
            Ok(Value::Null)
        } else {
            model.call(op, args)
        };
        let mut reply = json!({"effects":std::mem::take(&mut model.sink.effects)});
        match result {
            Ok(v) => reply["result"] = v,
            Err(e) => reply["error"] = json!(e),
        }
        writeln!(input, "{reply}").unwrap();
    }
    assert!(child.wait().unwrap().success());
    assert!(count > 300);
}
