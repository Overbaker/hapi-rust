//! HAPI permission bridge for ACP agents with one-shot choices (TS
//! `cli/src/modules/common/permission/AcpPermissionHandler.ts`).
use super::{
    tool_names::{derive_tool_input, derive_tool_name},
    types::{PermissionBackend, PermissionRequest, PermissionResponse},
};
use crate::{
    permission::base::{auto_approval, ApprovalRules, BasePermissionHandler, Completion, Pending},
    session::{client::SessionClient, metadata::now},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq)]
pub struct UserAnswer {
    /// `approved` | `approved_for_session` | `denied` | `abort`
    pub decision: String,
    pub answers: Option<Value>,
}

type UserWaitEnd = Box<dyn FnOnce() + Send>;

/// Empty hint lists mean "no tool-name / tool-id hints" (agy titles its
/// requests with the command line, so hints could only match command text).
pub type RequiresUser = Box<dyn Fn(&PermissionRequest) -> bool + Send + Sync>;
#[derive(Default)]
pub struct AcpPermissionOptions {
    pub always_tool_name_hints: Vec<String>,
    pub always_tool_id_hints: Vec<String>,
    /// Automatic approvals never leave standing grants (`approved_for_session`
    /// becomes `approved`).
    pub auto_approve_once: bool,
    /// Requests that need the user's answer in every mode (questions).
    pub requires_user: Option<RequiresUser>,
    /// Called when a request starts waiting for the user; the returned
    /// callback runs once it is answered.
    pub on_user_wait: Option<Box<dyn Fn() -> UserWaitEnd + Send + Sync>>,
}

fn pick_option(request: &PermissionRequest, kinds: &[&str]) -> Option<String> {
    kinds
        .iter()
        .find_map(|k| request.options.iter().find(|o| o.kind == *k))
        .or_else(|| request.options.first())
        .map(|o| o.option_id.clone())
}

/// The ACP outcome for a HAPI decision.
pub fn map_decision_to_outcome(request: &PermissionRequest, decision: &str) -> PermissionResponse {
    let kinds: &[&str] = match decision {
        "abort" => return PermissionResponse::Cancelled,
        "approved_for_session" => &["allow_always", "allow_once"],
        "approved" => &["allow_once", "allow_always"],
        _ => &["reject_once", "reject_always"],
    };
    pick_option(request, kinds).map_or(PermissionResponse::Cancelled, PermissionResponse::Selected)
}

/// The automatic decision for a request, before `auto_approve_once`.
pub fn auto_decision(
    mode: Option<&str>,
    tool: &str,
    tool_call_id: &str,
    opts: &AcpPermissionOptions,
) -> Option<&'static str> {
    let names: Vec<&str> = opts
        .always_tool_name_hints
        .iter()
        .map(String::as_str)
        .collect();
    let ids: Vec<&str> = opts
        .always_tool_id_hints
        .iter()
        .map(String::as_str)
        .collect();
    let rules = ApprovalRules {
        names: Some(&names),
        ids: Some(&ids),
        ..Default::default()
    };
    auto_approval(mode.unwrap_or(""), tool, tool_call_id, rules)
}

pub const STALE_REQUEST_REASON: &str = "The agent process that asked this has stopped";

/// Settles, as canceled, stored requests the agent no longer waits for (TS
/// `settleRequestsOfPreviousAgentProcess`): their JSON-RPC call died with the
/// previous agent process, so an answer could never reach it. `None` when
/// nothing changes.
pub fn settle_stale_requests(
    state: &Value,
    has_live_request: &dyn Fn(&str) -> bool,
    at: Value,
) -> Option<Value> {
    let requests = state.get("requests")?.as_object()?;
    let stale: Vec<_> = requests
        .iter()
        .filter(|(id, _)| !has_live_request(id))
        .collect();
    if stale.is_empty() {
        return None;
    }
    let mut next = state.clone();
    if !next["completedRequests"].is_object() {
        next["completedRequests"] = json!({});
    }
    for (id, request) in stale {
        let mut done = if request.is_object() {
            request.clone()
        } else {
            json!({})
        };
        done["completedAt"] = at.clone();
        done["status"] = json!("canceled");
        done["reason"] = json!(STALE_REQUEST_REASON);
        done["decision"] = json!("abort");
        next["completedRequests"][id] = done;
        next["requests"].as_object_mut()?.remove(id);
    }
    Some(next)
}

#[derive(Default)]
struct State {
    backend_requests: HashMap<String, PermissionRequest>,
    questions: HashMap<String, oneshot::Sender<UserAnswer>>,
    user_waits: HashMap<String, UserWaitEnd>,
    used: HashSet<String>,
}

pub struct AcpPermissionHandler {
    client: Weak<SessionClient>,
    base: Arc<BasePermissionHandler<()>>,
    backend: Arc<dyn PermissionBackend>,
    get_mode: Box<dyn Fn() -> Option<String> + Send + Sync>,
    opts: AcpPermissionOptions,
    state: Mutex<State>,
}

impl AcpPermissionHandler {
    /// Registers the hub `permission` RPC on the session.
    pub fn new(
        client: Arc<SessionClient>,
        backend: Arc<dyn PermissionBackend>,
        get_mode: Box<dyn Fn() -> Option<String> + Send + Sync>,
        opts: AcpPermissionOptions,
    ) -> Arc<Self> {
        let stored = client.agent_state.get().value;
        if stored["requests"]
            .as_object()
            .is_some_and(|r| !r.is_empty())
        {
            let live = backend.clone();
            client.agent_state.update(move |s| {
                settle_stale_requests(s, &|id| live.has_live_request(id), json!(now()))
                    .unwrap_or_else(|| s.clone())
            });
        }
        let weak = Arc::downgrade(&client);
        let base = Arc::new(BasePermissionHandler::new(
            Arc::new(move |f| {
                if let Some(s) = weak.upgrade() {
                    s.agent_state.update(f)
                }
            }),
            Default::default(),
        ));
        let this = Arc::new(Self {
            client: Arc::downgrade(&client),
            base: base.clone(),
            backend,
            get_mode,
            opts,
            state: Mutex::default(),
        });
        let handler = Arc::downgrade(&this);
        client.rpc.register(
            "permission",
            base.response_handler(
                move |r, p| {
                    let handler = handler.upgrade();
                    Box::pin(async move {
                        handler
                            .ok_or(crate::permission::base::MISSING)?
                            .respond(r, p)
                    })
                },
                |_| Ok(()),
            ),
        );
        this
    }

    fn start_user_wait(&self, id: &str) {
        if let Some(end) = self.opts.on_user_wait.as_ref().map(|f| f()) {
            self.state.lock().unwrap().user_waits.insert(id.into(), end);
        }
    }

    fn end_user_wait(&self, id: &str) {
        let end = self.state.lock().unwrap().user_waits.remove(id);
        if let Some(end) = end {
            end();
        }
    }

    pub fn handle_request(&self, request: PermissionRequest) {
        let tool = derive_tool_name(
            request.title.as_deref(),
            request.kind.as_deref(),
            request.raw_input.as_ref(),
        );
        let input = derive_tool_input(&request);
        let requires_user = self
            .opts
            .requires_user
            .as_ref()
            .is_some_and(|f| f(&request));
        let auto = if requires_user {
            None
        } else {
            let mode = (self.get_mode)();
            auto_decision(mode.as_deref(), &tool, &request.tool_call_id, &self.opts)
        };
        if let Some(decision) = auto {
            let decision = if self.opts.auto_approve_once && decision == "approved_for_session" {
                "approved"
            } else {
                decision
            };
            self.backend
                .respond_to_permission(&request, map_decision_to_outcome(&request, decision));
            let at = now();
            let entry = json!({"tool":tool,"toolCallId":request.tool_call_id,"arguments":input,"createdAt":at,"completedAt":at,"status":"approved","decision":decision});
            let id = request.id.clone();
            if let Some(client) = self.client.upgrade() {
                client.agent_state.update(move |s| {
                    let mut next = if s.is_object() { s.clone() } else { json!({}) };
                    if !next["completedRequests"].is_object() {
                        next["completedRequests"] = json!({});
                    }
                    next["completedRequests"][&id] = entry.clone();
                    next
                });
            }
            return;
        }
        let id = request.id.clone();
        let call = request.tool_call_id.clone();
        {
            let mut state = self.state.lock().unwrap();
            state.backend_requests.insert(id.clone(), request);
            state.used.insert(id.clone());
        }
        self.start_user_wait(&id);
        drop(self.base.add(id, tool, input, Some(call)));
    }

    /// A question for the user, shown as `tool_name` with `input`. Never
    /// answered automatically; `abort` once cancelled.
    pub async fn ask_user(
        &self,
        id: &str,
        tool_name: &str,
        input: Value,
        tool_call_id: Option<&str>,
    ) -> UserAnswer {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            state.questions.insert(id.into(), tx);
            state.used.insert(id.into());
        }
        self.start_user_wait(id);
        drop(self.base.add(
            id.into(),
            tool_name.into(),
            input,
            tool_call_id.map(str::to_owned),
        ));
        rx.await.unwrap_or(UserAnswer {
            decision: "abort".into(),
            answers: None,
        })
    }

    fn respond(&self, r: Value, _pending: Pending<()>) -> Result<Completion, String> {
        let id = r["id"].as_str().unwrap_or("").to_owned();
        let approved = r["approved"] == true;
        let decision = r["decision"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| if approved { "approved" } else { "denied" }.into());
        let reason = r["reason"].as_str().map(str::to_owned);
        let status = if approved { "approved" } else { "denied" }.to_owned();
        let question = self.state.lock().unwrap().questions.remove(&id);
        if let Some(tx) = question {
            let answers = r.get("answers").filter(|a| !a.is_null()).cloned();
            self.end_user_wait(&id);
            let _ = tx.send(UserAnswer {
                decision: decision.clone(),
                answers: answers.clone(),
            });
            return Ok(Completion {
                status,
                reason,
                decision: Some(decision),
                answers,
                ..Default::default()
            });
        }
        let request = self.state.lock().unwrap().backend_requests.remove(&id);
        if let Some(request) = &request {
            if decision == "abort" {
                self.backend.cancel_prompt(&request.session_id);
            }
            self.backend
                .respond_to_permission(request, map_decision_to_outcome(request, &decision));
        }
        self.end_user_wait(&id);
        Ok(Completion {
            status,
            reason,
            decision: Some(decision),
            ..Default::default()
        })
    }

    /// Whether a request with this id is open or was already shown.
    pub fn is_used(&self, id: &str) -> bool {
        self.state.lock().unwrap().used.contains(id)
            || self.base.pending_ids().iter().any(|p| p == id)
    }

    pub fn has_pending_requests(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.backend_requests.is_empty() || !state.questions.is_empty()
    }

    /// Questions first (answering them cannot fail), then the agent's
    /// requests, then the hub-side entries.
    pub fn cancel_all(&self, reason: &str) {
        let questions: Vec<_> = self.state.lock().unwrap().questions.drain().collect();
        for (id, tx) in questions {
            self.end_user_wait(&id);
            let _ = tx.send(UserAnswer {
                decision: "abort".into(),
                answers: None,
            });
        }
        let requests: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .backend_requests
            .drain()
            .collect();
        for (id, request) in requests {
            self.backend
                .respond_to_permission(&request, PermissionResponse::Cancelled);
            self.end_user_wait(&id);
        }
        self.base
            .cancel(reason.into(), reason.into(), Some("abort".into()), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::types::PermissionOption;

    fn request(kinds: &[&str]) -> PermissionRequest {
        PermissionRequest {
            id: "r".into(),
            session_id: "s".into(),
            tool_call_id: "c".into(),
            title: Some("ls -la".into()),
            kind: Some("execute".into()),
            raw_input: None,
            raw_output: None,
            options: kinds
                .iter()
                .map(|k| PermissionOption {
                    option_id: format!("o-{k}"),
                    name: k.to_string(),
                    kind: k.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn outcome_mapping() {
        let all = request(&["allow_once", "allow_always", "reject_once", "reject_always"]);
        let sel = |s: &str| PermissionResponse::Selected(s.into());
        assert_eq!(
            map_decision_to_outcome(&all, "abort"),
            PermissionResponse::Cancelled
        );
        assert_eq!(
            map_decision_to_outcome(&all, "approved"),
            sel("o-allow_once")
        );
        assert_eq!(
            map_decision_to_outcome(&all, "approved_for_session"),
            sel("o-allow_always")
        );
        assert_eq!(
            map_decision_to_outcome(&all, "denied"),
            sel("o-reject_once")
        );
        let only = request(&["allow_always"]);
        assert_eq!(
            map_decision_to_outcome(&only, "approved"),
            sel("o-allow_always")
        );
        assert_eq!(
            map_decision_to_outcome(&only, "denied"),
            sel("o-allow_always")
        );
        assert_eq!(
            map_decision_to_outcome(&request(&[]), "approved"),
            PermissionResponse::Cancelled
        );
    }

    #[test]
    fn settles_requests_of_a_previous_agent_process() {
        let state = json!({
            "requests": {
                "old": {"tool":"ls -la","toolCallId":"c1","arguments":{},"createdAt":1},
                "live": {"tool":"rm","createdAt":2},
            },
            "completedRequests": {"done": {"status":"approved"}},
            "steeringActive": false,
        });
        let next = settle_stale_requests(&state, &|id| id == "live", json!(9)).unwrap();
        assert_eq!(
            next,
            json!({
                "requests": {"live": {"tool":"rm","createdAt":2}},
                "completedRequests": {
                    "done": {"status":"approved"},
                    "old": {"tool":"ls -la","toolCallId":"c1","arguments":{},"createdAt":1,
                        "completedAt":9,"status":"canceled","reason":STALE_REQUEST_REASON,"decision":"abort"},
                },
                "steeringActive": false,
            })
        );
        assert_eq!(
            settle_stale_requests(&next, &|id| id == "live", json!(9)),
            None
        );
        assert_eq!(
            settle_stale_requests(&json!({}), &|_| false, json!(9)),
            None
        );
    }

    #[test]
    fn agy_auto_approval_by_mode() {
        let opts = AcpPermissionOptions::default();
        let d = |m| auto_decision(m, "ls -la", "c", &opts);
        assert_eq!(d(Some("always-proceed")), Some("approved_for_session"));
        assert_eq!(d(Some("request-review")), None);
        assert_eq!(d(Some("acceptEdits")), None);
        assert_eq!(d(Some("plan")), None);
        assert_eq!(d(None), None);
        assert_eq!(
            auto_decision(Some("request-review"), "change_title", "c", &opts),
            None
        );
    }
}
