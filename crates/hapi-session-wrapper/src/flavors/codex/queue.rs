use super::app_server_client::Error;
use futures_util::future::BoxFuture;
use hapi_wrapper_core::jsjson::stringify;
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, Error>;
fn invalid() -> Error {
    Error::Indeterminate("Invalid native queue response".into())
}
fn disk(e: impl std::fmt::Display) -> Error {
    Error::Indeterminate(e.to_string())
}
fn missing(e: &Error) -> bool {
    matches!(e,Error::Rejected(s) if s.to_ascii_lowercase().contains("queued submission not found"))
}
pub type Input = Vec<Value>;
fn input<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Input, D::Error> {
    let input = Input::deserialize(d)?;
    if input.iter().all(|v| v["type"].is_string()) {
        Ok(input)
    } else {
        Err(serde::de::Error::custom("Invalid queue input"))
    }
}
#[derive(Copy, Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Unknown,
    Queued,
    Consumed,
    Canceled,
    Rejected,
    Released,
}
use State::*;
impl State {
    fn terminal(self) -> bool {
        matches!(self, Consumed | Canceled)
    }
    fn retry(self) -> bool {
        matches!(self, Rejected | Canceled | Released)
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    #[serde(deserialize_with = "input")]
    input: Input,
    state: State,
    #[serde(
        default,
        deserialize_with = "super::value::present",
        skip_serializing_if = "Option::is_none"
    )]
    native_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "super::value::present",
        skip_serializing_if = "Option::is_none"
    )]
    batch_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    detached_steer: bool,
}
impl Entry {
    fn new(input: Input, state: State) -> Self {
        Self {
            input,
            state,
            native_id: None,
            batch_id: None,
            detached_steer: false,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    #[serde(deserialize_with = "input")]
    pub input: Input,
}
#[derive(Copy, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Intent {
    #[default]
    Cancel,
    Replace,
    Steer,
}
#[derive(Clone, Serialize, Deserialize)]
struct Removal {
    #[serde(flatten)]
    member: Member,
    #[serde(default)]
    intent: Intent,
}
#[derive(Copy, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    #[default]
    Eligible,
    Superseded,
    Consumed,
}
#[derive(Clone, Serialize, Deserialize)]
struct Version {
    #[serde(deserialize_with = "input")]
    input: Input,
    members: Vec<Member>,
    #[serde(
        default,
        deserialize_with = "super::value::present",
        skip_serializing_if = "Option::is_none"
    )]
    removed: Option<Vec<Removal>>,
    #[serde(default)]
    status: Status,
}
impl Version {
    fn new(members: Vec<Member>, removed: Option<Vec<Removal>>) -> Self {
        let input = members
            .iter()
            .flat_map(|m| m.input.iter().cloned())
            .collect();
        Self {
            input,
            members,
            removed,
            status: Status::Eligible,
        }
    }
    fn without(&self, id: &str, intent: Intent) -> Self {
        let mut removed = self.removed.clone().unwrap_or_default();
        let members = self
            .members
            .iter()
            .filter_map(|m| {
                if m.id == id {
                    removed.push(Removal {
                        member: m.clone(),
                        intent,
                    });
                    None
                } else {
                    Some(m.clone())
                }
            })
            .collect();
        Self::new(members, Some(removed))
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Batch {
    client_id: String,
    native_id: String,
    key: String,
    state: State,
    #[serde(default)]
    current: usize,
    versions: Vec<Version>,
}
impl Batch {
    fn current(&self) -> Option<&Version> {
        self.versions
            .get(self.current)
            .or_else(|| self.versions.last())
    }
    fn matches(&self, input: &[Value], accepted: bool) -> Vec<usize> {
        let normalized = normalize(input);
        self.versions
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                v.status == Status::Eligible || accepted && v.status == Status::Consumed
            })
            .filter(|(_, v)| normalize(&v.input) == normalized)
            .map(|(i, _)| i)
            .collect()
    }
    fn append(&mut self, v: Version) -> usize {
        self.versions.push(v);
        self.current = self.versions.len() - 1;
        self.current
    }
    fn confirm(&mut self, index: usize) {
        for v in &mut self.versions[..index] {
            if v.status == Status::Eligible {
                v.status = Status::Superseded;
            }
        }
        self.current = index;
    }
}
#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    entries: IndexMap<String, Entry>,
    batches: IndexMap<String, Batch>,
}
impl Ledger {
    fn native(&self, e: &Entry) -> Option<&str> {
        if e.detached_steer {
            return None;
        }
        e.batch_id
            .as_ref()
            .and_then(|id| self.batches.get(id))
            .map(|b| b.native_id.as_str())
            .filter(|v| !v.is_empty())
    }
    fn native_id(&self, e: &Entry) -> Option<String> {
        if e.detached_steer {
            None
        } else {
            self.native(e)
                .map(str::to_owned)
                .or_else(|| e.native_id.clone())
        }
    }
    fn ids(&self, state: State) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| e.state == state)
            .map(|(id, _)| id.clone())
            .collect()
    }
    fn withdraw(&mut self, batch: &str, v: &Version, h: &dyn Host) {
        for r in v.removed.iter().flatten() {
            if let Some(e) = self
                .entries
                .get_mut(&r.member.id)
                .filter(|e| e.batch_id.as_deref() == Some(batch))
            {
                if r.intent == Intent::Steer {
                    e.detached_steer = true;
                } else if !e.state.terminal() {
                    e.state = Canceled;
                    h.mirror(&r.member.id, None);
                }
            }
        }
    }
    fn replace(&self, b: &Batch, input: Input) -> Version {
        let current = b.current();
        let members = current.map(|v| v.members.as_slice()).unwrap_or_default();
        let first = members
            .iter()
            .find(|m| self.entries.get(&m.id).is_none_or(|e| !e.state.terminal()));
        let mut removed = current.and_then(|v| v.removed.clone()).unwrap_or_default();
        removed.extend(
            members
                .iter()
                .filter(|m| first.is_none_or(|f| f.id != m.id))
                .cloned()
                .map(|member| Removal {
                    member,
                    intent: Intent::Replace,
                }),
        );
        Version {
            members: first
                .map(|m| Member {
                    id: m.id.clone(),
                    input: input.clone(),
                })
                .into_iter()
                .collect(),
            input,
            removed: Some(removed),
            status: Status::Eligible,
        }
    }
}
pub fn normalize(input: &[Value]) -> String {
    let rows: Vec<_> = input
        .iter()
        .map(|v| {
            let mut out = json!({});
            if let Some(o) = v.as_object() {
                let mut keys = o.keys().collect::<Vec<_>>();
                keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                for k in keys {
                    if k != "text_elements" || v[k].as_array().is_some_and(|l| !l.is_empty()) {
                        out[k] = v[k].clone();
                    }
                }
            }
            out
        })
        .collect();
    stringify(&json!(rows))
}
pub fn batch_key(settings: &Value, meta: &Value) -> String {
    let mut key = super::value::object(settings);
    for k in [
        "fallbackModel",
        "customSystemPrompt",
        "appendSystemPrompt",
        "allowedTools",
        "disallowedTools",
    ] {
        key[k] = meta[k].clone();
    }
    stringify(&key)
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Submission {
    pub id: String,
    #[serde(deserialize_with = "input")]
    pub input: Input,
    pub client_user_message_id: String,
}
pub trait Host: Send + Sync {
    fn rpc<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<Value>>;
    fn consumed(&self, ids: &[String], steered: bool);
    fn uncertain(&self, ids: &[String]);
    fn mirror(&self, id: &str, input: Option<&Input>);
    fn requeued<'a>(&'a self, ids: Vec<String>) -> BoxFuture<'a, Result<()>>;
}
pub struct Queue {
    thread: String,
    file: PathBuf,
    host: Arc<dyn Host>,
    ledger: Mutex<Ledger>,
    operations: tokio::sync::Mutex<()>,
}
impl Queue {
    pub fn load(thread: String, file: PathBuf, host: Arc<dyn Host>) -> Result<Self> {
        let mut ledger = match fs::read(&file) {
            Ok(raw) => {
                let v: Value = serde_json::from_slice(&raw).map_err(disk)?;
                if v["version"] == 2 {
                    serde_json::from_value(v).map_err(disk)?
                } else {
                    Ledger {
                        entries: serde_json::from_value(v).map_err(disk)?,
                        ..Default::default()
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ledger::default(),
            Err(e) => return Err(disk(e)),
        };
        for (id, v) in ledger
            .batches
            .iter()
            .filter_map(|(id, b)| {
                let v = b.current()?;
                let proven = v.status == Status::Consumed
                    || b.current > 0
                        && b.versions
                            .iter()
                            .take(b.current)
                            .all(|v| v.status != Status::Eligible);
                proven.then(|| (id.clone(), v.clone()))
            })
            .collect::<Vec<_>>()
        {
            ledger.withdraw(&id, &v, host.as_ref());
        }
        Ok(Self {
            thread,
            file,
            host,
            ledger: Mutex::new(ledger),
            operations: Default::default(),
        })
    }
    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap()
    }
    fn save(&self, l: &Ledger) -> Result<()> {
        let dir = self.file.parent().unwrap_or(Path::new("."));
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(dir)
            .map_err(disk)?;
        let tmp = self
            .file
            .with_added_extension(format!("{}.tmp", Uuid::new_v4()));
        let result = (|| -> io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec(
                &json!({"version":2,"entries":l.entries,"batches":l.batches}),
            )?)?;
            fs::rename(&tmp, &self.file)
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result.map_err(disk)
    }
    pub fn state(&self, id: &str) -> Option<State> {
        self.lock().entries.get(id).map(|e| e.state)
    }
    pub fn owns(&self, id: &str) -> bool {
        self.state(id).is_some()
    }
    pub async fn flush(&self) {
        drop(self.operations.lock().await);
    }
    pub fn original_boundary_id(&self, id: &str, content: &Value) -> Option<String> {
        let l = self.lock();
        let Some(b) = l.batches.get(id) else {
            return Some(id.into());
        };
        let input = content
            .as_array()
            .or_else(|| content["text_elements"].as_array())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let [idx] = b.matches(input, true)[..] else {
            return None;
        };
        b.versions[idx].members.first().map(|m| m.id.clone())
    }
    pub fn committed(&self, id: &str, content: &Value) -> Result<Option<Vec<Member>>> {
        let mut l = self.lock();
        let Some(b) = l.batches.get(id) else {
            l.entries
                .entry(id.into())
                .or_insert_with(|| Entry::new(vec![], Consumed))
                .state = Consumed;
            self.save(&l)?;
            self.host.consumed(&[id.into()], false);
            return Ok(None);
        };
        let input = content
            .as_array()
            .or_else(|| content["text_elements"].as_array())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let [idx] = b.matches(input, true)[..] else {
            let ids = b
                .current()
                .into_iter()
                .flat_map(|v| &v.members)
                .filter(|m| l.entries.get(&m.id).is_some_and(|e| !e.state.terminal()))
                .map(|m| m.id.clone())
                .collect::<Vec<_>>();
            if !ids.is_empty() {
                self.host.uncertain(&ids);
            }
            return Ok(Some(vec![]));
        };
        let v = b.versions[idx].clone();
        for m in &v.members {
            if let Some(e) = l.entries.get_mut(&m.id) {
                if normalize(&e.input) != normalize(&m.input) {
                    e.input = m.input.clone();
                    self.host.mirror(&m.id, Some(&m.input));
                }
                e.state = Consumed;
            }
        }
        l.withdraw(id, &v, self.host.as_ref());
        let b = &mut l.batches[id];
        b.versions[idx].status = Status::Consumed;
        if b.current == idx && b.state != Canceled {
            b.state = Consumed;
        }
        self.save(&l)?;
        let ids = v.members.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        if !ids.is_empty() {
            self.host.consumed(&ids, false);
        }
        Ok(Some(v.members))
    }
    pub async fn command<F: std::future::Future<Output = Result<Option<String>>>>(
        &self,
        id: &str,
        work: impl FnOnce() -> F,
    ) -> Result<Option<String>> {
        let _op = self.operations.lock().await;
        match self.state(id) {
            Some(Consumed) => {
                self.host.consumed(&[id.into()], false);
                return Ok(None);
            }
            Some(state) if !matches!(state, Rejected | Canceled) => {
                self.host.uncertain(&[id.into()]);
                return Err(Error::Rejected(
                    "Previous command outcome is unknown; not replaying".into(),
                ));
            }
            _ => {}
        }
        {
            let mut l = self.lock();
            l.entries.insert(id.into(), Entry::new(vec![], Unknown));
            self.save(&l)?;
        }
        match work().await {
            Ok(None) => {
                self.committed(id, &Value::Null)?;
                Ok(None)
            }
            Ok(text) => {
                let mut l = self.lock();
                l.entries.shift_remove(id);
                self.save(&l)?;
                Ok(text)
            }
            Err(e) => {
                self.host.uncertain(&[id.into()]);
                Err(e)
            }
        }
    }
    pub async fn list(&self) -> Result<Vec<Submission>> {
        let mut items = vec![];
        let mut params = json!({"threadId":self.thread});
        loop {
            let page = self.host.rpc("thread/queue/list", params.clone()).await?;
            items.extend(
                serde_json::from_value::<Vec<Submission>>(page["data"].clone())
                    .map_err(|_| invalid())?,
            );
            match &page["nextCursor"] {
                Value::Null => break,
                Value::String(s) if s.is_empty() => break,
                Value::String(_) => params["cursor"] = page["nextCursor"].clone(),
                _ => return Err(invalid()),
            }
        }
        Ok(items)
    }
    pub async fn reconcile(&self) -> Result<Vec<String>> {
        let _op = self.operations.lock().await;
        self.reconcile_locked().await
    }
    async fn publish_released(&self) -> Result<()> {
        let ids = {
            let l = self.lock();
            let ids = l.ids(Released);
            for id in &ids {
                self.host.mirror(id, Some(&l.entries[id].input));
            }
            ids
        };
        if !ids.is_empty() {
            self.host.requeued(ids).await?;
        }
        Ok(())
    }
    async fn reconcile_locked(&self) -> Result<Vec<String>> {
        let items = self.list().await?;
        let mut present = HashSet::new();
        let mut members = HashSet::new();
        let mut dequeued = vec![];
        {
            let mut l = self.lock();
            for item in items {
                let id = &item.client_user_message_id;
                present.insert(id.clone());
                let adopted = if let Some(b) = l.batches.get(id) {
                    let indexes = b.matches(&item.input, false);
                    let (v, new) = match indexes.as_slice() {
                        [idx] => (b.versions[*idx].clone(), Some(*idx)),
                        [] => {
                            let v = l.replace(b, item.input.clone());
                            if v.members.is_empty() {
                                continue;
                            }
                            (v, None)
                        }
                        _ => continue,
                    };
                    let b = &mut l.batches[id];
                    let idx = new.unwrap_or_else(|| b.append(v.clone()));
                    let confirmed = idx >= b.current;
                    if confirmed {
                        b.confirm(idx);
                        if new.is_none() || !matches!(b.state, Consumed | Canceled | Released) {
                            b.state = Queued;
                        }
                    }
                    b.native_id = item.id.clone();
                    if confirmed {
                        l.withdraw(id, &v, self.host.as_ref());
                    }
                    for m in &v.members {
                        members.insert(m.id.clone());
                    }
                    v.members
                } else {
                    vec![Member {
                        id: id.clone(),
                        input: item.input.clone(),
                    }]
                };
                for m in adopted {
                    let e = l
                        .entries
                        .entry(m.id.clone())
                        .or_insert_with(|| Entry::new(m.input.clone(), Queued));
                    if e.state.terminal() {
                        continue;
                    }
                    let changed = e.native_id.as_deref() != Some(&item.id)
                        || stringify(&json!(e.input)) != stringify(&json!(m.input));
                    e.native_id = Some(item.id.clone());
                    e.input = m.input;
                    e.state = Queued;
                    e.detached_steer = false;
                    if changed {
                        self.host.mirror(&m.id, Some(&e.input));
                    }
                }
            }
            for (id, e) in &mut l.entries {
                if e.state == Queued
                    && !(if e.batch_id.is_some() {
                        &members
                    } else {
                        &present
                    })
                    .contains(id)
                {
                    e.state = Unknown;
                    dequeued.push(id.clone());
                }
            }
            for b in l.batches.values_mut() {
                if b.state == Queued && !present.contains(&b.client_id) {
                    b.state = Unknown;
                }
            }
            self.save(&l)?;
            let ids = l.ids(Unknown);
            if !ids.is_empty() {
                self.host.uncertain(&ids);
            }
        }
        self.publish_released().await?;
        Ok(dequeued)
    }
    pub fn replay(&self) {
        for (id, e) in &self.lock().entries {
            match e.state {
                Queued | Released => self.host.mirror(id, Some(&e.input)),
                Canceled => self.host.mirror(id, None),
                _ => {}
            }
        }
    }
    fn delete_proven(&self, native: &str) -> Result<()> {
        let mut l = self.lock();
        let ids = l
            .entries
            .iter()
            .filter(|(_, e)| l.native_id(e).as_deref() == Some(native) && e.state != Consumed)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            l.entries[&id].state = Canceled;
            self.host.mirror(&id, None);
        }
        for b in l.batches.values_mut() {
            if b.native_id == native && b.state != Consumed {
                b.state = Canceled;
            }
        }
        self.save(&l)
    }
    pub async fn deleted(&self, native: &str) -> Result<()> {
        let _op = self.operations.lock().await;
        self.delete_proven(native)
    }
    async fn delete(&self, native: &str) -> Result<bool> {
        self.host
            .rpc(
                "thread/queue/delete",
                json!({"threadId":self.thread,"queuedSubmissionId":native}),
            )
            .await?["deleted"]
            .as_bool()
            .ok_or_else(invalid)
    }
    pub async fn suspend(&self) -> Result<()> {
        let _op = self.operations.lock().await;
        if self.reconcile_locked().await.is_err() {
            let mut l = self.lock();
            for e in l.entries.values_mut() {
                if e.state == Queued {
                    e.state = Unknown;
                }
            }
            self.save(&l)?;
            let ids = l.ids(Unknown);
            if !ids.is_empty() {
                self.host.uncertain(&ids);
            }
            return Ok(());
        }
        let mut attempted = HashSet::new();
        let mut deleted = HashSet::new();
        let ids: Vec<_> = self.lock().entries.keys().cloned().collect();
        for id in ids {
            let native = {
                let mut l = self.lock();
                let e = &mut l.entries[&id];
                if !matches!(e.state, Queued | Unknown) {
                    continue;
                }
                e.state = Unknown;
                let native = l.native_id(&l.entries[&id]);
                for b in l.batches.values_mut() {
                    if Some(&b.native_id) == native.as_ref() && b.state == Queued {
                        b.state = Unknown;
                    }
                }
                self.save(&l)?;
                native
            };
            if let Some(n) = native {
                if attempted.insert(n.clone())
                    && self.delete(&n).await.unwrap_or(false)
                    && self.state(&id) != Some(Consumed)
                {
                    deleted.insert(n);
                }
            }
        }
        {
            let mut l = self.lock();
            let ids: Vec<_> = l
                .entries
                .iter()
                .filter(|(_, e)| {
                    e.state == Unknown && l.native_id(e).is_some_and(|n| deleted.contains(&n))
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                l.entries[&id].state = Released;
            }
            for b in l.batches.values_mut() {
                if deleted.contains(&b.native_id) && b.state != Consumed {
                    b.state = Released;
                } else if b.state == Queued {
                    b.state = Unknown;
                }
            }
            self.save(&l)?;
            let ids = l.ids(Unknown);
            if !ids.is_empty() {
                self.host.uncertain(&ids);
            }
        }
        self.publish_released().await
    }
    fn journal_proposal(&self, bid: &str, v: Version, member: Option<&str>) -> Result<Proposal> {
        let mut l = self.lock();
        let b = &mut l.batches[bid];
        let previous = b.current;
        let state = b.state;
        let index = b.append(v.clone());
        if b.state == Queued {
            b.state = Unknown;
        }
        let p = Proposal {
            index,
            previous,
            state,
            input: v.input,
            native: b.native_id.clone(),
            batch: bid.into(),
        };
        if let Some(id) = member {
            l.entries[id].state = Unknown;
        }
        self.save(&l)?;
        Ok(p)
    }
    async fn update(&self, p: &Proposal, params: Option<Value>) -> Result<Value> {
        let response = self.host.rpc("thread/queue/update", params.unwrap_or_else(|| json!({"threadId":self.thread,"queuedSubmissionId":p.native,"input":p.input}))).await?;
        let sub: Submission =
            serde_json::from_value(response["queuedSubmission"].clone()).map_err(|_| invalid())?;
        if sub.id != p.native
            || sub.client_user_message_id != p.batch
            || normalize(&sub.input) != normalize(&p.input)
        {
            return Err(invalid());
        }
        Ok(response)
    }
    pub async fn native_mutation(&self, method: &str, params: Value) -> Result<Value> {
        let _op = self.operations.lock().await;
        let native = params["queuedSubmissionId"].as_str().unwrap_or("");
        match method {
            "thread/queue/add" | "thread/queue/reorder" | "thread/queue/start" => {
                self.host.rpc(method, params).await
            }
            "thread/queue/delete" => {
                if !native.is_empty() {
                    let mut l = self.lock();
                    let ids = l
                        .entries
                        .iter()
                        .filter(|(_, e)| {
                            l.native_id(e).as_deref() == Some(native)
                                && matches!(e.state, Unknown | Queued)
                        })
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    for id in ids {
                        l.entries[&id].state = Unknown;
                    }
                    for b in l.batches.values_mut() {
                        if b.native_id == native && matches!(b.state, Queued | Unknown) {
                            b.state = Unknown;
                        }
                    }
                    self.save(&l)?;
                }
                let res = self.host.rpc(method, params.clone()).await?;
                if res["deleted"] == true && !native.is_empty() {
                    self.delete_proven(native)?;
                }
                Ok(res)
            }
            "thread/queue/update" => {
                let input = valid_input(&params["input"])?;
                let pair = {
                    let l = self.lock();
                    l.batches
                        .values()
                        .find(|b| !native.is_empty() && b.native_id == native)
                        .map(|b| (b.client_id.clone(), l.replace(b, input.clone())))
                };
                let Some((bid, v)) = pair else {
                    return self.host.rpc(method, params).await;
                };
                let p = self.journal_proposal(&bid, v.clone(), None)?;
                let res = self.update(&p, Some(params)).await;
                let mut l = self.lock();
                let b = &mut l.batches[&bid];
                match &res {
                    Ok(_) => {
                        b.confirm(p.index);
                        if !b.state.terminal() {
                            b.state = Queued;
                        }
                        l.withdraw(&bid, &v, self.host.as_ref());
                        if let Some(m) = v.members.first() {
                            if let Some(e) =
                                l.entries.get_mut(&m.id).filter(|e| e.state != Consumed)
                            {
                                if normalize(&e.input) != normalize(&input) {
                                    self.host.mirror(&m.id, Some(&input));
                                }
                                e.input = input;
                                e.state = Queued;
                            }
                        }
                    }
                    Err(Error::Indeterminate(_)) => {
                        b.state = Unknown;
                        b.versions[p.index].status = Status::Eligible;
                    }
                    Err(_) => p.reject(b),
                }
                self.save(&l)?;
                res
            }
            _ => Err(Error::Rejected(format!(
                "Unsupported native queue mutation: {method}"
            ))),
        }
    }
    pub async fn enqueue(
        &self,
        id: &str,
        input: Input,
        resume: bool,
        key: Option<&str>,
    ) -> Result<()> {
        let _op = self.operations.lock().await;
        let existing = self.lock().entries.get(id).cloned();
        if let Some(e) = existing.as_ref().filter(|e| !e.state.retry()) {
            if e.state == Consumed {
                self.host.consumed(&[id.into()], false);
            } else {
                self.reconcile_locked().await?;
            }
            return Ok(());
        }
        let released = existing.is_some_and(|e| e.state == Released);
        let input = if released {
            self.lock().entries[id].input.clone()
        } else {
            input
        };
        let key = key.filter(|s| !s.is_empty());
        {
            let mut l = self.lock();
            l.entries.insert(
                id.into(),
                Entry::new(
                    input.clone(),
                    if key.is_none() {
                        Unknown
                    } else if released {
                        Released
                    } else {
                        Rejected
                    },
                ),
            );
            self.save(&l)?;
        }
        if let Some(key) = key {
            let tail = self.list().await?.pop();
            if self.state(id) == Some(Consumed) {
                return Ok(());
            }
            let candidate = tail.and_then(|s| {
                let l = self.lock();
                let b = l.batches.get(&s.client_user_message_id)?;
                if b.state != Queued || b.key != key {
                    return None;
                }
                let [idx] = b.matches(&s.input, false)[..] else {
                    return None;
                };
                Some((b.client_id.clone(), b.versions[idx].clone()))
            });
            if let Some((bid, v)) = candidate {
                let mut members = v.members;
                members.push(Member {
                    id: id.into(),
                    input: input.clone(),
                });
                self.lock().entries[id].batch_id = Some(bid.clone());
                let p = self.journal_proposal(&bid, Version::new(members, None), Some(id))?;
                let result = self.update(&p, None).await;
                {
                    let mut l = self.lock();
                    match result {
                        Ok(_) => {
                            let b = &mut l.batches[&bid];
                            b.confirm(p.index);
                            if !b.state.terminal() {
                                b.state = Queued;
                            }
                            let e = &mut l.entries[id];
                            if e.state != Consumed {
                                e.state = Queued;
                                e.native_id = Some(p.native);
                            }
                            self.save(&l)?;
                        }
                        Err(_) if l.entries[id].state == Consumed => return Ok(()),
                        Err(ref e) if missing(e) => {
                            let b = &mut l.batches[&bid];
                            b.versions.pop();
                            b.current = b.versions.len().saturating_sub(1);
                            b.state = Unknown;
                            l.entries[id].batch_id = None;
                            self.save(&l)?;
                        }
                        Err(e) => {
                            match e {
                                Error::Indeterminate(_) => {
                                    l.batches[&bid].state = Unknown;
                                    l.entries[id].state = Unknown;
                                    self.host.uncertain(&[id.into()]);
                                }
                                Error::Rejected(_) | Error::NotReady(_) => {
                                    p.reject(&mut l.batches[&bid]);
                                    l.entries[id].state = Rejected;
                                }
                            }
                            self.save(&l)?;
                            return Err(e);
                        }
                    }
                }
                if result.is_ok() {
                    self.start_if(resume).await;
                    return Ok(());
                }
            }
        }
        self.add(id, input, key, resume).await
    }
    async fn start_if(&self, resume: bool) {
        if resume {
            let _ = self
                .host
                .rpc("thread/queue/start", json!({"threadId":self.thread}))
                .await;
        }
    }
    async fn add(&self, id: &str, input: Input, key: Option<&str>, resume: bool) -> Result<()> {
        let client = key.map_or_else(
            || id.to_owned(),
            |_| format!("hapi-batch-{}", Uuid::new_v4()),
        );
        {
            let mut l = self.lock();
            l.entries[id].state = Unknown;
            if let Some(k) = key {
                l.entries[id].batch_id = Some(client.clone());
                l.batches.insert(
                    client.clone(),
                    Batch {
                        client_id: client.clone(),
                        native_id: String::new(),
                        state: Unknown,
                        current: 0,
                        key: k.into(),
                        versions: vec![Version::new(
                            vec![Member {
                                id: id.into(),
                                input: input.clone(),
                            }],
                            None,
                        )],
                    },
                );
            }
            self.save(&l)?;
        }
        let result = self
            .host
            .rpc(
                "thread/queue/add",
                json!({"threadId":self.thread,"input":input,"clientUserMessageId":client}),
            )
            .await
            .and_then(|v| {
                serde_json::from_value::<Submission>(v["queuedSubmission"].clone())
                    .map_err(|_| invalid())
            });
        {
            let mut l = self.lock();
            match &result {
                Ok(sub) => {
                    let e = &mut l.entries[id];
                    let accepted = e.state == Consumed;
                    if !accepted {
                        e.state = Queued;
                        e.native_id = Some(sub.id.clone());
                    }
                    if let Some(b) = l.batches.get_mut(&client) {
                        b.native_id = sub.id.clone();
                        if !accepted {
                            b.state = Queued;
                        }
                    }
                }
                Err(err) if l.entries[id].state != Consumed => {
                    let state = if matches!(err, Error::Indeterminate(_)) {
                        Unknown
                    } else {
                        Rejected
                    };
                    l.entries[id].state = state;
                    if let Some(b) = l.batches.get_mut(&client) {
                        b.state = state;
                    }
                    if state == Unknown {
                        self.host.uncertain(&[id.into()]);
                    }
                }
                _ => {}
            }
            self.save(&l)?;
        }
        result?;
        self.start_if(resume).await;
        Ok(())
    }
    async fn remove_member(&self, id: &str, steer: bool) -> Result<Removed> {
        if self.reconcile_locked().await.is_err() {
            self.host.uncertain(&[id.into()]);
            return Ok(Removed::Unknown);
        }
        let (e, b) = {
            let l = self.lock();
            let e = l.entries[id].clone();
            let b = e
                .batch_id
                .as_ref()
                .and_then(|id| l.batches.get(id))
                .cloned();
            (e, b)
        };
        if e.state == Consumed {
            return Ok(Removed::Consumed);
        }
        let batch = e.batch_id.as_ref();
        let mut original = None;
        let native = if let Some(b) = &b {
            if b.state != Queued {
                return Ok(Removed::Unknown);
            }
            let Some(v) = b.current() else {
                return Ok(Removed::Unknown);
            };
            if !v.members.iter().any(|m| m.id == id) {
                return Ok(Removed::Unknown);
            }
            if v.members.len() > 1 {
                original = Some(v.clone());
            }
            b.native_id.clone()
        } else {
            if batch.is_some() || steer && e.state != Queued {
                return Ok(Removed::Unknown);
            }
            let Some(n) = e.native_id else {
                return Ok(Removed::Unknown);
            };
            n
        };
        let operation = if let (Some(v), Some(bid)) = (&original, batch) {
            let p = self.journal_proposal(
                bid,
                v.without(id, if steer { Intent::Steer } else { Intent::Cancel }),
                Some(id),
            )?;
            match self.update(&p, None).await {
                Ok(_) => {
                    let mut l = self.lock();
                    let b = &mut l.batches[bid];
                    b.confirm(p.index);
                    if b.state == Unknown {
                        b.state = Queued;
                    }
                    Ok(true)
                }
                Err(e) => {
                    let mut l = self.lock();
                    if l.entries[id].state == Consumed {
                        return Ok(Removed::Consumed);
                    }
                    if !matches!(e, Error::Indeterminate(_)) && !missing(&e) {
                        p.reject(&mut l.batches[bid]);
                        l.entries[id].state = Queued;
                        self.save(&l)?;
                        if steer {
                            return Ok(Removed::Rejected(e.to_string()));
                        }
                    }
                    Err(e)
                }
            }
        } else {
            {
                let mut l = self.lock();
                l.entries[id].state = Unknown;
                if let Some(bid) = batch {
                    l.batches[bid].state = Unknown;
                }
                self.save(&l)?;
            }
            self.delete(&native).await
        };
        let mut l = self.lock();
        if l.entries[id].state == Consumed {
            return Ok(Removed::Consumed);
        }
        if !matches!(operation, Ok(true)) {
            self.host.uncertain(&[id.into()]);
            self.save(&l)?;
            return Ok(Removed::Unknown);
        }
        if steer {
            if batch.is_some() {
                l.entries[id].detached_steer = true;
            }
        } else {
            l.entries[id].state = Canceled;
            if let Some(bid) = batch {
                if original.is_none() {
                    l.batches[bid].state = Canceled;
                }
                self.host.mirror(id, None);
            }
        }
        self.save(&l)?;
        Ok(Removed::Done(original))
    }
    pub async fn cancel(&self, id: &str) -> Result<Value> {
        let _op = self.operations.lock().await;
        match self.state(id) {
            None => return Ok(json!("indeterminate")),
            Some(Consumed) => return Ok(json!("consumed")),
            Some(s) if s.retry() => {
                let mut l = self.lock();
                l.entries[id].state = Canceled;
                self.save(&l)?;
                return Ok(json!(true));
            }
            _ => {}
        }
        Ok(match self.remove_member(id, false).await? {
            Removed::Done(_) => json!(true),
            Removed::Consumed => json!("consumed"),
            _ => json!("indeterminate"),
        })
    }
    pub async fn steer(&self, id: &str, turn: &str, fresh: Option<Input>) -> Result<Value> {
        let _op = self.operations.lock().await;
        let prior = self.state(id);
        let unknown = json!({"steered":false,"indeterminate":true});
        let consumed = json!({"steered":false,"error":"Message already consumed"});
        let original = match prior {
            Some(Consumed) => return Ok(consumed),
            Some(_) => match self.remove_member(id, true).await? {
                Removed::Done(v) => v,
                Removed::Unknown => return Ok(unknown),
                Removed::Rejected(e) => return Ok(json!({"steered":false,"error":e})),
                Removed::Consumed => return Ok(consumed),
            },
            None => {
                let Some(input) = fresh.clone() else {
                    return Ok(json!({"steered":false,"error":"Message not queued"}));
                };
                let mut l = self.lock();
                l.entries.insert(id.into(), Entry::new(input, Unknown));
                self.save(&l)?;
                None
            }
        };
        let entry = self.lock().entries[id].clone();
        let params = json!({"threadId":self.thread,"expectedTurnId":turn,"input":entry.input,"clientUserMessageId":id});
        let err = match self.host.rpc("turn/steer", params).await {
            Ok(_) => {
                let mut l = self.lock();
                l.entries[id].state = Consumed;
                self.save(&l)?;
                self.host.consumed(&[id.into()], true);
                return Ok(json!({"steered":true}));
            }
            Err(_) if self.state(id) == Some(Consumed) => return Ok(json!({"steered":true})),
            Err(e) => e,
        };
        if matches!(err, Error::Indeterminate(_)) {
            let mut l = self.lock();
            if let Some(b) = entry.batch_id.as_ref().and_then(|id| l.batches.get_mut(id)) {
                b.state = Unknown;
            }
            self.save(&l)?;
            self.host.uncertain(&[id.into()]);
            return Ok(unknown);
        }
        let mut restore = prior.is_some() && fresh.is_none() || entry.batch_id.is_some();
        if let (Some(old), Some(bid)) = (original, &entry.batch_id) {
            let p = self.journal_proposal(bid, Version::new(old.members, None), None)?;
            let res = self.update(&p, None).await;
            let mut l = self.lock();
            match res {
                Ok(_) => {
                    let b = &mut l.batches[bid];
                    b.confirm(p.index);
                    if b.state == Unknown {
                        b.state = Queued;
                    }
                    let e = &mut l.entries[id];
                    e.detached_steer = false;
                    if e.state != Consumed {
                        e.state = Queued;
                    }
                    restore = false;
                }
                Err(_) if l.entries[id].state == Consumed => return Ok(json!({"steered":true})),
                Err(e) if missing(&e) => {
                    l.batches[bid].versions[p.index].status = Status::Superseded
                }
                Err(_) => {
                    l.batches[bid].state = Unknown;
                    l.entries[id].state = Unknown;
                    self.host.uncertain(&[id.into()]);
                    restore = false;
                }
            }
            self.save(&l)?;
        }
        if restore {
            {
                let mut l = self.lock();
                l.entries
                    .insert(id.into(), Entry::new(entry.input.clone(), Released));
                self.save(&l)?;
            }
            if self.add(id, entry.input, None, false).await.is_err()
                && entry.batch_id.is_none()
                && self.state(id) != Some(Consumed)
            {
                let mut l = self.lock();
                l.entries[id].state = Unknown;
                self.save(&l)?;
                self.host.uncertain(&[id.into()]);
            }
        } else if prior.is_none() {
            let mut l = self.lock();
            l.entries[id].state = Rejected;
            self.save(&l)?;
        }
        Ok(json!({"steered":false,"error":err.to_string()}))
    }
}
struct Proposal {
    index: usize,
    previous: usize,
    state: State,
    input: Input,
    native: String,
    batch: String,
}
impl Proposal {
    fn reject(&self, b: &mut Batch) {
        b.versions[self.index].status = Status::Superseded;
        b.current = self.previous;
        if b.state == Unknown {
            b.state = self.state;
        }
    }
}
enum Removed {
    Done(Option<Version>),
    Consumed,
    Unknown,
    Rejected(String),
}
fn valid_input(v: &Value) -> Result<Input> {
    let a = v
        .as_array()
        .filter(|a| a.iter().all(|v| v["type"].is_string()))
        .ok_or_else(invalid)?;
    Ok(a.clone())
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
