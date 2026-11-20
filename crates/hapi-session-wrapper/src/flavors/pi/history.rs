use super::transport::Transport;
use hapi_wrapper_core::session::client::SessionClient;
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

#[derive(Default)]
pub struct History {
    cursor: Option<String>,
    seen: HashSet<String>,
    pending: VecDeque<Option<String>>,
    enabled: bool,
    pub fatal: bool,
}
impl History {
    pub async fn initialize(&mut self, t: &Transport, c: &SessionClient) {
        let baseline = t.rpc(json!({"type":"get_entries"})).await.ok();
        if let Some(entries) = baseline.as_ref().and_then(|v| v["entries"].as_array()) {
            self.seen.extend(
                entries
                    .iter()
                    .filter_map(|e| e["id"].as_str().map(str::to_owned)),
            );
            self.cursor = entries
                .last()
                .and_then(|e| e["id"].as_str())
                .map(str::to_owned);
            self.enabled = t.rpc(json!({"type":"get_fork_messages"})).await.is_ok()
                && t.rpc(self.read_entries())
                    .await
                    .is_ok_and(|d| d["entries"].is_array());
        }
        let enabled = self.enabled;
        c.metadata.update(move |v| {
            let mut m = v.clone();
            if enabled {
                m["capabilities"]["conversationHistory"] =
                    json!({"forkCurrent":true,"forkAtMessage":true,"rewindToMessage":true})
            } else if let Some(cap) = m["capabilities"].as_object_mut() {
                cap.remove("conversationHistory");
            }
            m
        });
    }
    fn read_entries(&self) -> Value {
        let mut command = json!({"type":"get_entries"});
        if let Some(id) = &self.cursor {
            command["since"] = json!(id)
        }
        command
    }
    pub fn expect(&mut self, id: Option<String>) {
        if self.enabled {
            self.pending.push_back(id)
        }
    }
    pub fn rejected(&mut self, id: &Option<String>) {
        if let Some(index) = self.pending.iter().position(|pending| pending == id) {
            self.pending.remove(index);
        }
    }
    pub fn entry(&mut self, e: &Value, c: &SessionClient) {
        let Some(id) = e["id"].as_str() else { return };
        if !self.enabled
            || !self.seen.insert(id.to_owned())
            || e["type"] != "message"
            || e["message"]["role"] != "user"
        {
            return;
        }
        if let Some(Some(local)) = self.pending.pop_front() {
            let id = id.to_owned();
            c.metadata.update(move |m| {
                let mut m = m.clone();
                m["conversationHistoryEntryIds"][&local] = json!(id);
                m["conversationHistoryPoints"][&local] = true.into();
                m
            });
        }
    }
    pub async fn sync(&mut self, t: &Transport, c: &SessionClient) {
        if !self.pending.is_empty() {
            if let Ok(data) = t.rpc(self.read_entries()).await {
                for e in data["entries"].as_array().into_iter().flatten() {
                    self.entry(e, c);
                    if let Some(id) = e["id"].as_str() {
                        self.cursor = Some(id.to_owned())
                    }
                }
            }
        }
    }
    pub async fn change(
        &mut self,
        rewind: bool,
        v: &Value,
        t: &Transport,
        c: &SessionClient,
    ) -> Result<Value, String> {
        if !self.enabled || !self.pending.is_empty() {
            return Err("Pi history unavailable or pending".into());
        }
        let local = v["messageLocalId"].as_str().filter(|s| !s.is_empty());
        let command = if let Some(local) = local {
            let id = c.metadata.get().value["conversationHistoryEntryIds"][local]
                .as_str()
                .ok_or("No native history point")?
                .to_owned();
            let points = t.rpc(json!({"type":"get_fork_messages"})).await?;
            if !points["messages"]
                .as_array()
                .is_some_and(|a| a.iter().any(|p| p["entryId"] == id))
            {
                return Err("Missing Pi fork point".into());
            }
            json!({"type":"fork","entryId":id})
        } else if rewind {
            return Err("messageLocalId is required".into());
        } else {
            json!({"type":"clone"})
        };
        let source = t.rpc(json!({"type":"get_state"})).await?;
        let file = source["sessionFile"]
            .as_str()
            .ok_or("Missing source file")?;
        let source_id = source["sessionId"].as_str().ok_or("Missing source id")?;
        let result = async {
            let response = t.rpc(command).await?;
            if response["cancelled"] == true {
                return Err("Pi history operation cancelled".into());
            }
            let branch = t.rpc(json!({"type":"get_state"})).await?;
            let id = branch["sessionId"]
                .as_str()
                .filter(|id| *id != source_id)
                .ok_or("Pi did not create a new session")?
                .to_owned();
            if rewind {
                let response = t.rpc(json!({"type":"get_entries"})).await?;
                let entries = response["entries"]
                    .as_array()
                    .ok_or("Missing history entries")?;
                self.seen = entries
                    .iter()
                    .filter_map(|e| e["id"].as_str().map(str::to_owned))
                    .collect();
                self.cursor = entries
                    .last()
                    .and_then(|e| e["id"].as_str())
                    .map(str::to_owned);
                let valid = self.seen.clone();
                c.metadata.update(move |m| {
                    let mut m = m.clone();
                    m["piSessionId"] = json!(id);
                    if let Some(ids) = m["conversationHistoryEntryIds"].as_object_mut() {
                        ids.retain(|_, v| v.as_str().is_some_and(|s| valid.contains(s)));
                        let points =
                            Value::Object(ids.keys().map(|k| (k.clone(), true.into())).collect());
                        m["conversationHistoryPoints"] = points;
                    }
                    m
                });
                if !c.flush_metadata(Duration::from_secs(5)).await {
                    self.fatal = true;
                    return Err("Pi rewind metadata did not persist".into());
                }
                c.set_native_resume_ready(None);
                Ok(json!({"success":true,"truncateFromLocalId":local,"messages":[]}))
            } else {
                Ok(json!({"nativeSessionId":id}))
            }
        }
        .await;
        if result
            .as_ref()
            .is_err_and(|e: &String| e.contains("timed out"))
        {
            self.fatal = true
        }
        if !self.fatal && (!rewind || result.is_err()) {
            let restored = async {
                t.rpc(json!({"type":"switch_session","sessionPath":file}))
                    .await?;
                t.rpc(json!({"type":"get_state"})).await
            }
            .await;
            if !restored.is_ok_and(|s| s["sessionId"] == source_id && s["sessionFile"] == file) {
                self.fatal = true;
                return Err("Failed to restore Pi source session".into());
            }
        }
        result
    }
}
