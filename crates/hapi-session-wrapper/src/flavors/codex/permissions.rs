use super::{
    permission_adapter::{self, Action, Prompt},
    value::*,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Deserialize)]
pub struct Reply {
    pub id: String,
    pub approved: bool,
    #[serde(default, deserialize_with = "super::value::present")]
    pub decision: Option<Decision>,
    #[serde(default, deserialize_with = "super::value::present")]
    pub reason: Option<String>,
    #[serde(default, deserialize_with = "super::value::present")]
    pub answers: Option<HashMap<String, Answer>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approved,
    ApprovedForSession,
    Denied,
    Abort,
}
#[derive(Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum Answer {
    List(Vec<String>),
    Wrapped { answers: Vec<String> },
}
pub trait Host {
    fn root_thread(&self) -> &str;
    fn send(&mut self, id: &str, message: Value);
    fn state(&mut self, update: Box<dyn Fn(&Value) -> Value + Send + Sync>);
    fn wait_started(&mut self, thread: &str, key: &str);
    fn wait_ended(&mut self, thread: &str, key: &str);
}
pub(super) fn send(host: &mut impl Host, thread: &str, id: &str, mut m: Value) {
    m["id"] = json!(id);
    let root = host.root_thread();
    if !root.is_empty() && root != thread {
        m = json!({"type":"agent-run-trace","agentId":thread,"cardId":format!("codex-agent:{thread}"),"message":m,"id":id,"scope":{"role":"child","threadId":thread,"parentThreadId":root},"scope_role":"child"});
    }
    host.send(id, m);
}
struct Pending {
    thread: String,
    turn: Option<String>,
    call: String,
    native: Value,
    question: bool,
    submitted: bool,
    waiting: bool,
    prompt: Prompt,
}
pub enum Submission {
    Response(Value, Value),
    Interrupt(Value),
}
pub struct Permissions {
    generation: String,
    pending: HashMap<String, Pending>,
    retired: HashSet<String>,
    closed: bool,
}
impl Permissions {
    pub fn new(generation: String) -> Self {
        Self {
            generation,
            pending: HashMap::new(),
            retired: HashSet::new(),
            closed: false,
        }
    }
    pub(super) fn pending_thread(&self, key: &str) -> Option<String> {
        self.pending.get(key).map(|p| p.thread.clone())
    }
    pub(super) fn has_pending(&self, thread: &str, id: &Value) -> bool {
        self.pending.contains_key(&self.key(thread, id))
    }
    fn key(&self, thread: &str, id: &Value) -> String {
        format!(
            "{}:{thread}:{}:{}",
            self.generation,
            if id.is_string() { "string" } else { "number" },
            id.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| hapi_wrapper_core::jsjson::stringify(id))
        )
    }
    pub fn receive(
        &mut self,
        host: &mut impl Host,
        id: &Value,
        method: &str,
        p: &Value,
        at: u64,
    ) -> Option<Value> {
        let thread = text(p, "threadId")?;
        let key = self.key(thread, id);
        if self.closed || self.pending.contains_key(&key) || self.retired.contains(&key) {
            return None;
        }
        let prompt = match permission_adapter::prepare(method, p, true, true) {
            Action::Ask(q) => q,
            Action::Immediate(v) => return Some(v),
            Action::Ignore => return None,
        };
        let call = text(p, "itemId").unwrap_or(&key).to_owned();
        let question = method == "item/tool/requestUserInput";
        if question {
            send(
                host,
                thread,
                &format!("codex:{thread}:question:{call}:start"),
                json!({"type":"tool-call","name":"request_user_input","callId":call,"input":prompt.input}),
            );
        }
        let row =
            json!({"tool":prompt.tool,"toolCallId":call,"arguments":prompt.input,"createdAt":at});
        let k = key.clone();
        host.state(Box::new(move |s| {
            let mut s = s.clone();
            s["requests"][&k] = row.clone();
            s
        }));
        host.wait_started(thread, &key);
        self.pending.insert(
            key,
            Pending {
                thread: thread.into(),
                turn: text(p, "turnId").map(str::to_owned),
                native: id.clone(),
                call,
                question,
                submitted: false,
                waiting: true,
                prompt,
            },
        );
        None
    }
    pub fn submit(&mut self, host: &mut impl Host, reply: &Reply) -> Result<Submission, String> {
        let r = self
            .pending
            .get_mut(&reply.id)
            .filter(|r| !r.submitted)
            .ok_or("Request already resolved or submitted")?;
        let answers = json!(reply.answers);
        if let Some(q) = r.prompt.input["questions"]
            .as_array()
            .filter(|_| reply.approved)
        {
            if reply.answers.is_none() {
                return Err("Answers are required".into());
            }
            for v in q {
                if let Some(id) = text(v, "id") {
                    if answers.get(id).is_none() {
                        return Err(format!("Missing answer: {id}"));
                    }
                }
            }
        }
        if r.question && !reply.approved {
            let turn = r
                .turn
                .as_deref()
                .ok_or("Cannot cancel a question without its native turn ID")?;
            r.submitted = true;
            return Ok(Submission::Interrupt(
                json!({"threadId":r.thread,"turnId":turn}),
            ));
        }
        let decision = if r.prompt.is_question() {
            if reply.approved {
                "accept"
            } else {
                "cancel"
            }
        } else if reply.approved {
            if matches!(reply.decision, Some(Decision::ApprovedForSession)) {
                "approved_for_session"
            } else {
                "approved"
            }
        } else if matches!(reply.decision, Some(Decision::Denied)) {
            "denied"
        } else {
            "abort"
        };
        let response = if r.question {
            let mut v = json!({});
            for (k, a) in answers.as_object().into_iter().flatten() {
                v[k] = if a.is_array() {
                    json!({"answers":a})
                } else {
                    a.clone()
                };
            }
            json!({"answers":v})
        } else {
            r.prompt.response(decision, &answers)
        };
        r.submitted = true;
        host.wait_ended(&r.thread, &reply.id);
        r.waiting = false;
        Ok(Submission::Response(r.native.clone(), response))
    }
    pub fn interrupt_failed(&mut self, id: &str) {
        if let Some(r) = self.pending.get_mut(id) {
            r.submitted = false;
        }
    }
    pub fn resolved(
        &mut self,
        host: &mut impl Host,
        thread: &str,
        id: &Value,
        canceled: bool,
        at: u64,
    ) {
        let key = self.key(thread, id);
        self.retired.insert(key.clone());
        let Some(r) = self.pending.remove(&key) else {
            return;
        };
        if r.waiting {
            host.wait_ended(thread, &key);
        }
        let status = if canceled { "canceled" } else { "resolved" };
        if r.question {
            send(
                host,
                thread,
                &format!("codex:{thread}:question:{}:{status}", r.call),
                json!({"type":"tool-call-result","callId":r.call,"output":{"status":status},"is_error":false}),
            );
        }
        host.state(Box::new(move |s| {
            let mut s = s.clone();
            if let Some(prior) = s["requests"].as_object_mut().and_then(|m| m.remove(&key)) {
                s["completedRequests"][&key] =
                    merge(prior, &json!({"completedAt":at,"status":status}));
            }
            s
        }));
    }
    pub fn close(&mut self, host: &mut impl Host, at: u64) {
        self.closed = true;
        let requests: Vec<_> = self
            .pending
            .values()
            .map(|r| (r.thread.clone(), r.native.clone()))
            .collect();
        for (t, id) in requests {
            self.resolved(host, &t, &id, true, at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavors::codex::async_questions::{questions, AsyncAnswer, AsyncQuestions};
    #[derive(Default)]
    struct Sink {
        state: Value,
        messages: Vec<Value>,
        waits: i32,
    }
    impl Host for Sink {
        fn root_thread(&self) -> &str {
            "root"
        }
        fn send(&mut self, _: &str, m: Value) {
            self.messages.push(m);
        }
        fn state(&mut self, f: Box<dyn Fn(&Value) -> Value + Send + Sync>) {
            self.state = f(&self.state);
        }
        fn wait_started(&mut self, _: &str, _: &str) {
            self.waits += 1;
        }
        fn wait_ended(&mut self, _: &str, _: &str) {
            self.waits -= 1;
        }
    }
    #[test]
    fn native_resolution_arbitrates() {
        let mut s = Sink::default();
        let mut manager = Permissions::new("generation".into());
        let params = json!({"threadId":"child","turnId":"turn","itemId":"call","questions":[{"id":"choice","isOther":true}]});
        let id = json!(1);
        manager.receive(&mut s, &id, "item/tool/requestUserInput", &params, 100);
        let key = "generation:child:number:1";
        assert_eq!(s.messages[0]["type"], "agent-run-trace");
        let mut reply = json!({"id":key,"approved":true,"answers":{}});
        assert!(manager
            .submit(&mut s, &serde_json::from_value(reply.clone()).unwrap())
            .is_err());
        reply["answers"]["choice"] = json!(["B"]);
        let Submission::Response(native, r) = manager
            .submit(&mut s, &serde_json::from_value(reply).unwrap())
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(native, id);
        assert_eq!(r, json!({"answers":{"choice":{"answers":["B"]}}}));
        assert!(s.state["completedRequests"].is_null());
        manager.resolved(&mut s, "child", &id, false, 110);
        manager.receive(&mut s, &id, "item/tool/requestUserInput", &params, 120);
        assert_eq!(s.waits, 0);
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.state["completedRequests"][key]["status"], "resolved");
        assert!(s.state["completedRequests"][key].get("answers").is_none());
        manager.receive(
            &mut s,
            &json!("1"),
            "item/tool/requestUserInput",
            &params,
            120,
        );
        manager.close(&mut s, 130);
        assert_eq!(
            s.state["completedRequests"]["generation:child:string:1"]["status"],
            "canceled"
        );
    }
    #[test]
    fn optional_answers_replay_and_retry() {
        let mut s = Sink::default();
        let mut manager = AsyncQuestions::default();
        let item = json!({"type":"agentMessage","delivery":"async","id":"i","questions":[{"title":"Which?","options":["A"]},{"title":"Other?"}]});
        let rows = questions("child", &item);
        let id = rows[0]["id"].as_str().unwrap();
        let q = rows[0]["questionItemId"].as_str().unwrap();
        manager.observe(&mut s, "child", &item, 100);
        let mut reply = json!({"id":id,"approved":true,"answers":{q:["None of the above"]}});
        assert!(manager
            .answer(&mut s, &serde_json::from_value(reply.clone()).unwrap(), 101)
            .is_err());
        reply["answers"][q] = json!(["None of the above", "user_note: B"]);
        let AsyncAnswer::Submit {
            text, client_id, ..
        } = manager
            .answer(&mut s, &serde_json::from_value(reply.clone()).unwrap(), 102)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(client_id, format!("{id}:answer"));
        manager.submit_failed(id, true);
        assert!(manager
            .answer(&mut s, &serde_json::from_value(reply).unwrap(), 103)
            .is_err());
        let answer = json!({"type":"userMessage","content":[{"type":"text","text":text}]});
        let mut replay = AsyncQuestions::default();
        replay.history(
            &mut s,
            "child",
            &json!({"turns":[{"items":[item,answer]}]}),
            104,
        );
        replay.observe(&mut s, "child", &item, 105);
        assert_eq!(
            s.state["completedRequests"][id]["answers"][q],
            json!({"answers":["user_note: B"]})
        );
        assert!(s.state["requests"].get(id).is_none());
        assert_eq!(s.messages.last().unwrap()["scope_role"], "child");
        let count = s.messages.len();
        replay.close(&mut s, 106);
        assert_eq!(s.messages.len(), count);
    }
}
