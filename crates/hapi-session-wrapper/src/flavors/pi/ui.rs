use super::transport::Transport;
use hapi_wrapper_core::session::{client::SessionClient, metadata::now};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Dialogs {
    pending: HashMap<String, (Value, Option<Instant>)>,
    retired: HashSet<String>,
}
impl Dialogs {
    pub async fn open(
        &mut self,
        v: &Value,
        t: &Transport,
        c: &SessionClient,
    ) -> Result<(), String> {
        let id = v["id"].as_str().ok_or("Missing dialog ID")?.to_owned();
        let method = v["method"].as_str().unwrap_or("");
        match method {
            "notify" => {
                c.send_session_event(json!({"type":"message","message":format!("[Pi {}] {}",v["notifyType"].as_str().unwrap_or("info"),v["message"].as_str().unwrap_or(""))}),None,None);
                return Ok(());
            }
            "setTitle" => {
                if let Some(name) = v["title"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    let name = name.to_owned();
                    c.metadata.update(move |m| {
                        let mut m = m.clone();
                        m["name"] = json!(name);
                        m
                    });
                }
                return Ok(());
            }
            "confirm" | "select" | "input" | "editor" => {}
            _ => return Ok(()),
        }
        if self.pending.contains_key(&id) {
            return self
                .answer(&json!({"id":id,"approved":false,"decision":"abort"}), t, c)
                .await;
        }
        if self.retired.contains(&id) {
            return t
                .send(json!({"type":"extension_ui_response","id":id,"cancelled":true}))
                .await
                .map_err(|e| e.to_string());
        }
        let (tool, args) = if method == "confirm" {
            (
                "PiExtensionConfirm",
                json!({"title":v["title"],"message":v["message"]}),
            )
        } else {
            let options: Vec<_> = v["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|label| json!({"label":label,"description":null}))
                .collect();
            let mut q = json!({"id":id,"header":v["title"],"question":v["title"],"required":true,"multiple":false,"options":options});
            if method == "editor" {
                q["inputType"] = "editor".into()
            }
            for k in ["prefill", "placeholder"] {
                if let Some(value) = v.get(k) {
                    q[k] = value.clone()
                }
            }
            ("request_user_input", json!({"questions":[q]}))
        };
        let expiry = v["timeout"]
            .as_u64()
            .filter(|n| *n > 0 && method != "editor")
            .and_then(|n| {
                Instant::now().checked_add(Duration::from_millis(
                    n.saturating_sub((n / 10).clamp(1, 5000)),
                ))
            });
        self.pending.insert(id.clone(), (v.clone(), expiry));
        c.send_agent_message(
            json!({"type":"tool-call","callId":id,"name":tool,"input":args,"status":"in_progress"}),
            None,
        );
        let created = now();
        c.agent_state.update(move |s| {
            let mut s = s.clone();
            s["requests"][&id] = json!({"tool":tool,"arguments":args,"createdAt":created});
            s
        });
        Ok(())
    }
    pub async fn expire(
        &mut self,
        all: bool,
        t: &Transport,
        c: &SessionClient,
    ) -> Result<(), String> {
        let ids: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, (_, expiry))| all || expiry.is_some_and(|x| x <= Instant::now()))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.answer(&json!({"id":id,"decision":"abort","approved":false}), t, c)
                .await?
        }
        Ok(())
    }
    pub async fn answer(
        &mut self,
        v: &Value,
        t: &Transport,
        c: &SessionClient,
    ) -> Result<(), String> {
        let id = v["id"].as_str().unwrap_or("").to_owned();
        let Some((request, _)) = self.pending.remove(&id) else {
            return Ok(());
        };
        self.retired.insert(id.clone());
        let mut response = json!({"type":"extension_ui_response","id":id});
        let mut approved = v["approved"] == true;
        if request["method"] == "confirm" && v["decision"] != "abort" {
            response["confirmed"] = approved.into()
        } else {
            let raw = &v["answers"][&id];
            let answers = raw.as_array().or_else(|| raw["answers"].as_array());
            let answer = answers
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find(|s| {
                    request["method"] != "select"
                        || request["options"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|o| o == s))
                });
            if let Some(answer) = answer.filter(|_| approved) {
                response["value"] = json!(answer.strip_prefix("user_note: ").unwrap_or(answer))
            } else {
                response["cancelled"] = true.into();
                approved = false
            }
        }
        let aborted = v["decision"] == "abort";
        let stamp = now();
        c.send_agent_message(json!({"type":"tool-call-result","callId":id,"is_error":!approved,"output":v.get("answers").unwrap_or(&response)}),None);
        c.agent_state.update(move |s| {
            let mut s = s.clone();
            if let Some(mut r) = s["requests"].as_object_mut().and_then(|m| m.remove(&id)) {
                r["completedAt"] = json!(stamp);
                r["status"] = json!(if approved {
                    "approved"
                } else if aborted {
                    "canceled"
                } else {
                    "denied"
                });
                s["completedRequests"][&id] = r
            }
            s
        });
        t.send(response).await.map_err(|e| e.to_string())
    }
}
