use super::value::*;
use serde_json::{json, Value};
pub fn request_id(thread: &str, item: &str) -> String {
    format!("codex-async-question:{}", json!([thread, item]))
}
pub fn replies(s: &str) -> Vec<Value> {
    let Some(raw) = s
        .trim()
        .strip_prefix("<send_user_message_question_reply>")
        .and_then(|s| s.strip_suffix("</send_user_message_question_reply>"))
    else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return vec![];
    };
    v.as_array()
        .map(Vec::as_slice)
        .unwrap_or(std::slice::from_ref(&v))
        .iter()
        .filter_map(|v| {
            let mut out =
                json!({"questionItemId":text(v,"questionItemId")?,"answer":v["answer"].as_str()?});
            if let Some(s) = text(v, "question") {
                out["question"] = json!(s);
            }
            Some(out)
        })
        .collect()
}
pub fn questions(thread: &str, item: &Value) -> Vec<Value> {
    let Some(id) =
        text(item, "id").filter(|_| item["type"] == "agentMessage" && item["delivery"] == "async")
    else {
        return vec![];
    };
    item["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(n, q)| {
            let title = text(q, "title")?;
            let item_id = json!(["request_user_input_async", id, n]).to_string();
            let options: Vec<_> = q["options"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            Some(json!({"id":request_id(thread,&item_id),"threadId":thread,"questionItemId":item_id,"title":title,"options":options,"input":{"isBlocking":false,"questions":[{"id":item_id,"question":title,"isOther":true,"options":options.iter().map(|s|json!({"label":s,"description":""})).collect::<Vec<_>>()}]}}))
        })
        .collect()
}

use super::permissions::{self, Host, Reply};
use std::collections::{HashMap, HashSet};
#[derive(Default)]
pub struct AsyncQuestions {
    pending: HashMap<String, bool>,
    known: HashMap<String, Value>,
    completed: HashMap<String, Option<String>>,
    closed: bool,
    dismissed: HashSet<String>,
}
impl AsyncQuestions {
    pub fn restore(completed: &Value) -> Self {
        let dismissed = completed
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, row)| row["decision"] == "denied")
            .map(|(id, _)| id.clone())
            .collect();
        Self {
            dismissed,
            ..Default::default()
        }
    }
    pub fn observe(&mut self, host: &mut impl Host, thread: &str, item: &Value, at: u64) {
        if self.closed {
            return;
        }
        if item["type"] == "userMessage" {
            for p in item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["type"] == "text")
            {
                for reply in replies(text(p, "text").unwrap_or("")) {
                    let id = request_id(thread, reply["questionItemId"].as_str().unwrap());
                    if self.completed.get(&id).is_some_and(Option::is_some) {
                        continue;
                    }
                    self.completed
                        .insert(id.clone(), Some(reply["answer"].as_str().unwrap().into()));
                    if self.known.contains_key(&id) {
                        self.resolve(host, &id, true, at);
                    }
                }
            }
        }
        for q in questions(thread, item) {
            let id = q["id"].as_str().unwrap().to_owned();
            self.known.insert(id.clone(), q.clone());
            if self.completed.contains_key(&id) {
                self.resolve(host, &id, true, at);
                continue;
            }
            if self.dismissed.contains(&id) || self.pending.contains_key(&id) {
                continue;
            }
            self.pending.insert(id.clone(), false);
            let row = json!({"tool":"request_user_input","toolCallId":id,"arguments":q["input"],"createdAt":item.get("completedAtMs").filter(|v|v.is_number()).cloned().unwrap_or(json!(at))});
            host.state(Box::new(move |s| {
                let mut s = s.clone();
                let mut completed = object(&s["completedRequests"]);
                completed.as_object_mut().unwrap().remove(&id);
                s["completedRequests"] = completed;
                s["requests"][&id] = row.clone();
                s
            }));
        }
    }
    pub fn history(&mut self, host: &mut impl Host, thread: &str, snapshot: &Value, at: u64) {
        for turn in snapshot["turns"].as_array().into_iter().flatten() {
            for item in turn["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v["type"] == "userMessage")
            {
                self.observe(host, thread, item, at);
            }
        }
    }
    pub fn answer(
        &mut self,
        host: &mut impl Host,
        reply: &Reply,
        at: u64,
    ) -> Result<AsyncAnswer, String> {
        let Some(submitted) = self.pending.get_mut(&reply.id) else {
            return Ok(AsyncAnswer::Other);
        };
        if self.closed || *submitted {
            return Err("Question already submitted or unavailable".into());
        }
        if !reply.approved {
            self.completed.insert(reply.id.clone(), None);
            self.resolve(host, &reply.id, true, at);
            return Ok(AsyncAnswer::Dismissed);
        }
        let q = &self.known[&reply.id];
        let question = q["questionItemId"].as_str().unwrap();
        let options = strings(&q["options"]).into_iter().collect::<HashSet<_>>();
        let answers = json!(reply.answers);
        let answer = super::permission_adapter::values(&answers, question)
            .into_iter()
            .filter(|v| *v != "None of the above" || options.contains(*v))
            .map(|s| {
                if options.contains(s) {
                    s
                } else {
                    s.strip_prefix("user_note: ").unwrap_or(s)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if answer.trim().is_empty() {
            return Err("An answer is required".into());
        }
        *submitted = true;
        let text = format!(
            "<send_user_message_question_reply>\n{}\n</send_user_message_question_reply>",
            json!([{"answer":answer,"question":q["title"],"questionItemId":question}])
        );
        Ok(AsyncAnswer::Submit {
            thread: q["threadId"].as_str().unwrap().into(),
            text,
            client_id: format!("{}:answer", reply.id),
        })
    }
    pub fn submit_failed(&mut self, id: &str, indeterminate: bool) {
        if !indeterminate {
            if let Some(entry) = self.pending.get_mut(id) {
                *entry = false;
            }
        }
    }
    fn resolve(&mut self, host: &mut impl Host, id: &str, emit: bool, at: u64) {
        self.pending.remove(id);
        let q = &self.known[id];
        let answer = self.completed.get(id).and_then(Option::as_deref);
        let mut resolution = json!({"status":if answer.is_some(){"resolved"}else{"canceled"}});
        if let Some(s) = answer {
            let value = if q["options"].as_array().unwrap().iter().any(|v| v == s) {
                s.to_owned()
            } else {
                format!("user_note: {s}")
            };
            resolution["answers"][q["questionItemId"].as_str().unwrap()] =
                json!({"answers":[value]});
        }
        if emit {
            let thread = q["threadId"].as_str().unwrap();
            let key = format!(
                "codex:{thread}:{id}:{}",
                resolution["status"].as_str().unwrap()
            );
            permissions::send(
                host,
                thread,
                &key,
                json!({"type":"tool-call-result","callId":id,"output":resolution}),
            );
        }
        if answer.is_none() && self.completed.contains_key(id) {
            resolution["decision"] = json!("denied");
        }
        let id = id.to_owned();
        let args = q["input"].clone();
        host.state(Box::new(move |s| {
            let mut s = s.clone();
            let mut row = json!({"tool":"request_user_input","toolCallId":id,"arguments":args});
            let prior = s["requests"]
                .get(&id)
                .unwrap_or(&s["completedRequests"][&id]);
            copy_fields(&mut row, prior, &[("createdAt", "createdAt")]);
            row["completedAt"] = s["completedRequests"][&id]
                .get("completedAt")
                .cloned()
                .unwrap_or(json!(at));
            let mut requests = object(&s["requests"]);
            requests.as_object_mut().unwrap().remove(&id);
            s["requests"] = requests;
            s["completedRequests"][&id] = merge(row, &resolution);
            s
        }));
    }
    pub fn close(&mut self, host: &mut impl Host, at: u64) {
        self.closed = true;
        for id in self.pending.keys().cloned().collect::<Vec<_>>() {
            self.resolve(host, &id, false, at);
        }
    }
}
pub enum AsyncAnswer {
    Other,
    Dismissed,
    Submit {
        thread: String,
        text: String,
        client_id: String,
    },
}
