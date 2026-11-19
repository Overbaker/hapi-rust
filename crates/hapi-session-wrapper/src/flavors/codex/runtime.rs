use super::{
    app_server::Server,
    app_server_client::{Client, Error, Event},
    bridge,
    gateway::{Gateway, Hooks},
    launch::{take_reserved_session_id, Launch},
    registry::{self, Binding, Intent, Record},
    root::{self, Root, RootHost},
    value::{object, text},
};
use futures_util::{
    future::{join_all, BoxFuture},
    FutureExt,
};
use hapi_wrapper_core::{
    config::Config,
    lifecycle::start_marker::start_marker,
    mcp::tools,
    rest::Rest,
    session::{
        bootstrap,
        client::SessionClient,
        metadata::{self, Overrides},
    },
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{watch, RwLock, RwLockReadGuard},
    time::{sleep, timeout},
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;
#[derive(Default)]
pub(super) struct Startup {
    pub session: Option<String>,
    pub activated: bool,
}
#[derive(Clone)]
struct Prepared {
    root: Arc<Root>,
    metadata: Value,
    namespace: String,
}
struct Reservation {
    root: Arc<Root>,
}
#[derive(Default)]
struct State {
    prepared: HashMap<String, Prepared>,
    roots: HashMap<String, Arc<Root>>,
    ending: HashSet<String>,
    reservations: HashMap<(String, String), Reservation>,
    settings: HashMap<String, Value>,
    reserved: Option<String>,
    first_id: Option<String>,
}
struct Runtime {
    command: PathBuf,
    models: Arc<super::models::Models>,
    retiring: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    config: Arc<Config>,
    rest: Rest,
    id: String,
    home: PathBuf,
    upstream: PathBuf,
    cwd: PathBuf,
    record: Arc<Mutex<Record>>,
    state: Mutex<State>,
    control: Mutex<Option<Arc<Client>>>,
    operations: RwLock<()>,
    stop: watch::Sender<Option<Result<()>>>,
    activated: AtomicBool,
}
impl Runtime {
    fn stop(&self, cause: Result<()>) {
        self.stop.send_if_modified(|state| {
            if state.is_some() {
                return false;
            }
            *state = Some(cause);
            true
        });
    }
    fn running(&self) -> Result<()> {
        if self.stop.borrow().is_some() {
            Err("Codex execution is stopping".into())
        } else {
            Ok(())
        }
    }
    async fn admit(&self) -> Result<RwLockReadGuard<'_, ()>> {
        self.running()?;
        let guard = self.operations.read().await;
        self.running()?;
        Ok(guard)
    }
    fn root(&self, id: &str) -> Option<Arc<Root>> {
        self.state.lock().unwrap().roots.get(id).cloned()
    }
    async fn edit(&self, f: impl FnOnce(&mut Record) -> Result<()> + Send + 'static) -> Result<()> {
        let record = self.record.clone();
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let mut r = record.lock().unwrap();
            f(&mut r)?;
            registry::save_runtime(&r, &config.home).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn rpc(&self, m: &str, p: Value) -> Result<Value> {
        let client = self
            .control
            .lock()
            .unwrap()
            .clone()
            .ok_or("Codex control client disconnected")?;
        client.request(m, p).await.map_err(|e| e.to_string())
    }
    fn observe(self: &Arc<Self>, mut client: Client) {
        let mut events =
            std::mem::replace(&mut client.events, tokio::sync::mpsc::unbounded_channel().1);
        let client = Arc::new(client);
        *self.control.lock().unwrap() = Some(client.clone());
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(e) = events.recv().await {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                if runtime.running().is_err() {
                    return;
                }
                match e {
                    Event::Notification { method, params }
                        if method == "thread/settings/updated" =>
                    {
                        if let Some(id) = text(&params, "threadId") {
                            let settings = object(&params["threadSettings"]);
                            runtime
                                .state
                                .lock()
                                .unwrap()
                                .settings
                                .insert(id.into(), settings.clone());
                            if let Some(r) = runtime.root(id).filter(|r| !r.connected()) {
                                r.accept_settings(&settings);
                            }
                        }
                    }
                    Event::Disconnected(_) => {
                        runtime.control.lock().unwrap().take();
                        tokio::spawn(runtime.reconnect());
                        return;
                    }
                    _ => {}
                }
            }
        });
    }
    fn reconnect(self: Arc<Self>) -> BoxFuture<'static, ()> {
        async move {
            while self.running().is_ok() {
                if let Ok(mut c) = Client::connect(&self.upstream, None).await {
                    if c.initialize().await.is_ok() {
                        let roots: Vec<_> =
                            self.state.lock().unwrap().roots.keys().cloned().collect();
                        let mut ok = true;
                        for id in roots {
                            if c.request(
                                "thread/resume",
                                json!({"threadId":id,"excludeTurns":true}),
                            )
                            .await
                            .is_err()
                            {
                                ok = false;
                                break;
                            }
                        }
                        if ok && self.running().is_ok() {
                            self.observe(c);
                            return;
                        }
                    }
                }
                sleep(Duration::from_secs(1)).await;
            }
        }
        .boxed()
    }
    async fn prepare(
        self: &Arc<Self>,
        cwd: &Path,
        existing: Option<String>,
        parent: Option<&Root>,
        initial: bool,
    ) -> Result<Arc<Root>> {
        self.running()?;
        let mut overrides = Overrides::default();
        for key in ["terminal", "concurrentClients"] {
            overrides.capabilities.insert(key.into(), Some(json!(true)));
        }
        if existing.is_some() {
            overrides
                .capabilities
                .insert("conversationHistory".into(), None);
        }
        if let Some(p) = parent {
            let m = p.session.metadata.get().value;
            let title = [m["name"].as_str(), m["summary"]["text"].as_str()]
                .into_iter()
                .flatten()
                .map(str::trim)
                .find(|s| !s.is_empty());
            if let Some(t) = title {
                overrides.fields.insert(
                    "summary".into(),
                    Some(json!({"text":format!("Fork: {t}"),"updatedAt":metadata::now()})),
                );
            }
            overrides
                .fields
                .insert("forkedFrom".into(), Some(json!(p.session.id)));
        }
        let opts = bootstrap::Options {
            flavor: "codex".into(),
            runner: true,
            working_directory: Some(cwd.into()),
            existing_id: existing.clone(),
            reserved_id: if initial {
                self.state.lock().unwrap().reserved.take()
            } else {
                None
            },
            agent_state: Some(json!({"controlledByUser":false})),
            overrides,
            ..Default::default()
        };
        let prepared = bootstrap::prepare(&self.rest, &self.config, &opts)
            .await
            .map_err(|e| e.to_string())?;
        let sid = text(&prepared.session, "id")
            .ok_or("Missing session id")?
            .to_string();
        if initial {
            self.state.lock().unwrap().first_id = Some(sid.clone());
        }
        let boot = bootstrap::open_prepared(
            &self.config,
            &opts,
            self.rest.clone(),
            prepared,
            false,
            false,
            |m| m["capabilities"]["conversationHistory"].is_object(),
        )
        .await
        .map_err(|e| e.to_string())?;
        self.models
            .register(&boot.client.rpc, &self.command, &self.home);
        let host = Arc::new(Host {
            runtime: Arc::downgrade(self),
            session: boot.client.clone(),
            bridge: Mutex::new(None),
            prompt: bridge::instructions(boot.session_summary_contract.unwrap_or(false)),
        });
        let opts = root::Options {
            directory: self.config.home.join("codex-runtimes/queues"),
            cwd: cwd.into(),
            endpoint: self.upstream.clone(),
            token: None,
            generation: self.id.clone(),
            attached: false,
        };
        let root = match Root::prepare(boot.client.clone(), boot.events, opts, host).await {
            Ok(r) => r,
            Err(e) => {
                if existing.is_none() && self.running().is_ok() {
                    boot.client.metadata.update(|m| {
                        let mut m = m.clone();
                        m["lifecycleState"] = json!("archived");
                        m["lifecycleStateSince"] = json!(metadata::now());
                        m
                    });
                }
                boot.client.session_end(None);
                boot.client.flush(Duration::from_secs(5)).await;
                boot.client.close();
                return Err(e);
            }
        };
        let entry = Prepared {
            root: root.clone(),
            metadata: boot.metadata,
            namespace: boot.session["namespace"]
                .as_str()
                .ok_or("Missing session namespace")?
                .into(),
        };
        self.state.lock().unwrap().prepared.insert(sid, entry);
        self.running()?;
        Ok(root)
    }
    async fn reserve_record(&self, root: &Root, id: &str) -> Result<()> {
        self.running()?;
        let sid = root.session.id.clone();
        let namespace = self.state.lock().unwrap().prepared[&sid].namespace.clone();
        let thread = id.to_owned();
        self.edit(move |r| {
            if r.sessions
                .values()
                .any(|b| b.active && b.thread_id == thread)
            {
                return Err(
                    "Thread is already being attached in this runtime; retry after it is ready"
                        .into(),
                );
            }
            r.sessions.insert(
                sid.clone(),
                Binding {
                    thread_id: thread,
                    namespace,
                    active: true,
                },
            );
            if let Some(p) = &mut r.pending_creations {
                p.retain(|s| s != &sid);
            }
            Ok(())
        })
        .await
    }
    async fn with_owner<T, F: std::future::Future<Output = Result<T>>>(
        &self,
        thread: &str,
        work: impl FnOnce() -> F,
    ) -> Result<T> {
        registry::with_thread_ownership(
            &self.home,
            thread,
            &self.id,
            Intent::Owned,
            || async { work().await.map_err(io::Error::other) },
            |e| async { Err(e) },
        )
        .await
        .map_err(|e| e.to_string())
    }
    async fn cold(
        self: &Arc<Self>,
        id: &str,
        cwd: &Path,
        existing: Option<String>,
        initial: bool,
    ) -> Result<Arc<Root>> {
        let mut attempted = None;
        let result = self
            .with_owner(id, || async {
                if self
                    .record
                    .lock()
                    .unwrap()
                    .sessions
                    .values()
                    .any(|b| b.active && b.thread_id == id)
                {
                    return Err(
                        "Thread is already being attached in this runtime; retry after it is ready"
                            .into(),
                    );
                }
                let existing = match existing {
                    Some(s) => Some(s),
                    None => registry::find_binding(
                        &self.config.home,
                        &self.home,
                        &self.config.api_url,
                        &self.config.cli_api_token,
                        id,
                        Intent::Owned,
                    )
                    .map_err(|e| e.to_string())?,
                };
                if existing
                    .as_ref()
                    .is_some_and(|sid| self.state.lock().unwrap().prepared.contains_key(sid))
                {
                    return Err(
                        "Thread is already being attached in this runtime; retry after it is ready"
                            .into(),
                    );
                }
                let new_row = existing.is_none();
                let root = self.prepare(cwd, existing, None, initial).await?;
                attempted = Some((root.clone(), new_row));
                self.reserve_record(&root, id).await?;
                Ok(root)
            })
            .await;
        if result.is_err() {
            if let Some((root, new_row)) = attempted {
                let sid = root.session.id.clone();
                self.edit(move |r| {
                    if let Some(b) = r.sessions.get_mut(&sid) {
                        b.active = false;
                    }
                    Ok(())
                })
                .await?;
                root.close(new_row).await;
                self.state.lock().unwrap().prepared.remove(&root.session.id);
            }
        }
        result
    }
    async fn pending(&self, root: &Root) -> Result<()> {
        let sid = root.session.id.clone();
        if let Err(error) = self
            .edit(move |r| {
                r.pending_creations.get_or_insert_with(Vec::new).push(sid);
                Ok(())
            })
            .await
        {
            self.discard(root).await?;
            return Err(error);
        }
        Ok(())
    }
    async fn discard(&self, root: &Root) -> Result<()> {
        let sid = root.session.id.clone();
        self.edit(move |r| {
            if let Some(p) = &mut r.pending_creations {
                p.retain(|v| v != &sid);
            }
            if let Some(b) = r.sessions.get_mut(&sid) {
                b.active = false;
            }
            Ok(())
        })
        .await?;
        root.close(true).await;
        self.state.lock().unwrap().prepared.remove(&root.session.id);
        Ok(())
    }
    async fn bind(
        &self,
        root: Arc<Root>,
        response: Value,
        subscribe: bool,
        options: &Value,
        fresh: bool,
    ) -> Result<()> {
        self.running()?;
        let id = text(&response["thread"], "id")
            .ok_or("Codex lifecycle response has no thread ID")?
            .to_owned();
        let reserved = self
            .record
            .lock()
            .unwrap()
            .sessions
            .get(&root.session.id)
            .cloned();
        match reserved {
            Some(b) if b.thread_id != id => return Err("Codex retargeted a reserved thread".into()),
            None => {
                self.with_owner(&id, || self.reserve_record(&root, &id))
                    .await?
            }
            _ => {}
        }
        let cwd = text(&response["thread"], "cwd")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.cwd().into());
        self.rpc(
            "thread/metadata/update",
            json!({"threadId":id,"gitInfo":git_info(&cwd).await}),
        )
        .await?;
        self.running()?;
        self.state
            .lock()
            .unwrap()
            .roots
            .insert(id.clone(), root.clone());
        root.bind(&id, response, subscribe).await?;
        if fresh {
            root.seed_fresh_usage();
        }
        self.running()?;
        self.rpc("thread/resume", json!({"threadId":id,"excludeTurns":true}))
            .await?;
        root.activate(options).await?;
        root.session.flush(Duration::from_secs(5)).await;
        self.edit(|_| Ok(())).await?;
        let local = self.state.lock().unwrap().prepared[&root.session.id]
            .metadata
            .clone();
        let mut m = root.session.metadata.get().value;
        if !m.is_object() {
            m = local.clone();
        }
        m["hostStartMarker"] = local["hostStartMarker"].clone();
        m["capabilities"]["concurrentClients"] = json!(true);
        let _ = self
            .rest
            .report_started(&self.config.home, &root.session.id, &m)
            .await;
        self.activated.store(true, Ordering::Release);
        Ok(())
    }
    async fn create(
        self: &Arc<Self>,
        method: &str,
        mut params: Value,
        parent: Option<&Root>,
        options: &Value,
    ) -> Result<Arc<Root>> {
        let cwd = text(&params, "cwd")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.clone());
        let root = self
            .prepare(&cwd, None, parent, options.is_object())
            .await?;
        self.pending(&root).await?;
        if method == "thread/fork" {
            params["deferGoalContinuation"] = json!(true);
        }
        match root.request(method, root.config(params)?).await {
            Ok(v) => {
                if let Err(e) = self
                    .bind(root.clone(), v, true, options, method == "thread/start")
                    .await
                {
                    unconfirmed(&root);
                    return Err(e);
                }
                Ok(root)
            }
            Err(e) => {
                if matches!(e, Error::Indeterminate(_)) {
                    unconfirmed(&root);
                } else {
                    self.discard(&root).await?;
                }
                Err(e.to_string())
            }
        }
    }
    async fn initial(self: &Arc<Self>, launch: Launch, mut options: Value) -> Result<()> {
        if options["resumeLast"] == true {
            let mut p = json!({"limit":1,"sortKey":"updated_at","sortDirection":"desc"});
            if options["resumeAll"] != true {
                p["cwd"] = json!(launch.cwd);
            }
            let list = self.rpc("thread/list", p).await?;
            options["resumeSessionId"] =
                json!(text(&list["data"][0], "id").ok_or("No Codex session to resume")?);
        }
        let mut params = launch.thread_params;
        if let Some(id) = text(&options, "resumeSessionId") {
            let thread = self
                .rpc("thread/read", json!({"threadId":id,"includeTurns":false}))
                .await?;
            if text(&thread["thread"], "parentThreadId").is_some() {
                return Err("Cannot cold-resume a child agent independently. Resume its parent HAPI session instead.".into());
            }
            let existing = text(&options, "existingSessionId").map(str::to_owned);
            let unarchive = existing.is_some();
            let root = self.cold(id, &launch.cwd, existing, true).await?;
            params["threadId"] = json!(id);
            params["excludeTurns"] = json!(true);
            let params = root.config(params)?;
            let res = match root.request("thread/resume", params.clone()).await {
                Err(Error::Rejected(e))
                    if unarchive && e.starts_with(&format!("session {id} is archived.")) =>
                {
                    root.request("thread/unarchive", json!({"threadId":id}))
                        .await
                        .map_err(|e| e.to_string())?;
                    root.request("thread/resume", params).await
                }
                res => res,
            }
            .map_err(|e| e.to_string())?;
            self.bind(root, res, false, &options, false).await?;
        } else {
            params["cwd"] = json!(launch.cwd);
            self.create("thread/start", params, None, &options).await?;
        }
        Ok(())
    }
    async fn end(self: &Arc<Self>, root: &Root, archive: bool) -> Result<()> {
        let sid = root.session.id.clone();
        let id = root.thread_id();
        let prepared = {
            let mut s = self.state.lock().unwrap();
            if !s.roots.contains_key(id) || !s.ending.insert(sid.clone()) {
                return Ok(());
            }
            s.prepared[&sid].clone()
        };
        if archive {
            if let Err(e) = root.request("thread/archive", json!({"threadId":id})).await {
                self.state.lock().unwrap().ending.remove(&sid);
                return Err(e.to_string());
            }
        }
        let sid2 = sid.clone();
        if let Err(e) = self
            .edit(move |r| {
                r.sessions
                    .get_mut(&sid2)
                    .ok_or("Missing runtime binding")?
                    .active = false;
                Ok(())
            })
            .await
        {
            self.stop(Err(e.clone()));
            return Err(e);
        }
        self.state.lock().unwrap().roots.remove(id);
        let rt = self.clone();
        let task = tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            prepared.root.close(true).await;
            let mut m = prepared.metadata;
            m["lifecycleState"] = json!("archived");
            let _ = rt.rest.report_started(&rt.config.home, &sid, &m).await;
            let _gate = rt.operations.write().await;
            let mut s = rt.state.lock().unwrap();
            s.prepared.remove(&sid);
            s.ending.remove(&sid);
            if s.roots.is_empty() && s.reservations.is_empty() {
                rt.stop(Ok(()));
            }
        });
        let mut retiring = self.retiring.lock().unwrap();
        retiring.retain(|t| !t.is_finished());
        retiring.push(task);
        Ok(())
    }
    async fn shutdown(&self, server: Server, gateway: Option<Gateway>) -> Result<()> {
        {
            let entries: Vec<_> = self
                .state
                .lock()
                .unwrap()
                .prepared
                .values()
                .cloned()
                .collect();
            for p in entries {
                p.root.stop_accepting();
            }
        }
        let guard = self.operations.write().await;
        if let Some(g) = gateway {
            g.close().await;
        }
        let (entries, ending) = {
            let s = self.state.lock().unwrap();
            (
                s.prepared.values().cloned().collect::<Vec<_>>(),
                s.ending.clone(),
            )
        };
        drop(guard);
        for p in &entries {
            p.root.stop_accepting();
        }
        let _ = timeout(
            Duration::from_secs(5),
            join_all(
                entries
                    .iter()
                    .filter(|p| !ending.contains(&p.root.session.id))
                    .map(|p| p.root.suspend()),
            ),
        )
        .await;
        if let Some(c) = self.control.lock().unwrap().take() {
            c.disconnect();
        }
        let stopped = match server.stop().await {
            Ok(()) => {
                self.edit(|r| {
                    for b in r.sessions.values_mut() {
                        b.active = false;
                    }
                    r.pending_creations = Some(vec![]);
                    Ok(())
                })
                .await
            }
            Err(e) => Err(e.to_string()),
        };
        join_all(
            entries
                .iter()
                .map(|p| p.root.close(ending.contains(&p.root.session.id))),
        )
        .await;
        let retiring = std::mem::take(&mut *self.retiring.lock().unwrap());
        join_all(retiring).await;
        stopped
    }
}
fn unconfirmed(root: &Root) {
    root.session.send_session_event(json!({"type":"message","message":"Native thread creation/binding is unconfirmed. Do not retry blindly; inspect the runtime before recovery."}), None, None);
}

struct Host {
    runtime: Weak<Runtime>,
    session: Arc<SessionClient>,
    bridge: Mutex<Option<tools::Listener>>,
    prompt: String,
}
impl Host {
    fn runtime(&self) -> Result<Arc<Runtime>> {
        self.runtime
            .upgrade()
            .ok_or_else(|| "Codex execution is stopping".into())
    }
}
impl RootHost for Host {
    fn create<'a>(
        &'a self,
        m: &'a str,
        p: Value,
        parent: Option<&'a Root>,
    ) -> BoxFuture<'a, Result<Arc<Root>>> {
        async move {
            let rt = self.runtime()?;
            let _op = rt.admit().await?;
            rt.create(m, p, parent, &Value::Null).await
        }
        .boxed()
    }
    fn end<'a>(
        &'a self,
        r: &'a Root,
        archive: bool,
        _: Option<&'a str>,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            let rt = self.runtime()?;
            let _op = rt.admit().await?;
            rt.end(r, archive).await
        }
        .boxed()
    }
    fn settings_for(&self, id: &str) -> Value {
        self.runtime
            .upgrade()
            .and_then(|r| r.state.lock().unwrap().settings.get(id).cloned())
            .unwrap_or(Value::Null)
    }
    fn instructions(&self) -> String {
        self.prompt.clone()
    }
    fn prepare_bridge<'a>(
        &'a self,
        _: &'a SessionClient,
        cwd: &'a Path,
    ) -> BoxFuture<'a, Result<Value>> {
        async move {
            let (listener, config) = bridge::start(self.session.clone(), cwd).await?;
            *self.bridge.lock().unwrap() = Some(listener);
            Ok(config)
        }
        .boxed()
    }
    fn stop_bridge(&self, _: &str) {
        self.bridge.lock().unwrap().take();
    }
}

async fn git_info(cwd: &Path) -> Value {
    async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
        let out = timeout(
            Duration::from_secs(3),
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }
    let (sha, branch, origin) = tokio::join!(
        git(cwd, &["rev-parse", "HEAD"]),
        git(cwd, &["symbolic-ref", "--short", "HEAD"]),
        git(cwd, &["remote", "get-url", "origin"])
    );
    let origin = origin.and_then(|s| {
        if !s.starts_with("https:") && !s.starts_with("http:") {
            return Some(s);
        }
        let url = s.split(['?', '#']).next()?;
        let (scheme, rest) = url.split_once("://")?;
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = host.rsplit('@').next()?;
        if host.is_empty() || host.contains([' ', '\\']) {
            return None;
        }
        Some(format!("{scheme}://{host}/{path}"))
    });
    json!({"sha":sha,"branch":branch,"originUrl":origin})
}

struct NativeHooks(Arc<Runtime>);
impl NativeHooks {
    fn key(connection: &str, v: &Value) -> (String, String) {
        (
            connection.into(),
            hapi_wrapper_core::jsjson::stringify(&v["id"]),
        )
    }
    async fn before(&self, mut v: Value, connection: &str) -> Result<Value> {
        let rt = &self.0;
        let _op = rt.admit().await?;
        let method = text(&v, "method").unwrap_or("").to_owned();
        let mut p = object(&v["params"]);
        let thread = text(&p, "threadId").map(str::to_owned);
        let mut existing = thread.as_ref().and_then(|t| rt.root(t));
        if matches!(
            method.as_str(),
            "thread/queue/add"
                | "thread/queue/update"
                | "thread/queue/delete"
                | "thread/queue/reorder"
                | "thread/queue/start"
        ) {
            if let Some(root) = existing {
                if v.get("id").is_none() {
                    return Err("Queue mutations require a JSON-RPC request ID".into());
                }
                return Ok(
                    json!({"id":v["id"],"result":root.native_queue_mutation(&method,p).await?}),
                );
            }
            return Ok(v);
        }
        if matches!(method.as_str(), "thread/revert" | "thread/rollback") {
            return Err("In-place rewind is unavailable with concurrent HAPI clients. Use /fork or Fork at message instead.".into());
        }
        if !matches!(
            method.as_str(),
            "thread/start" | "thread/fork" | "thread/resume"
        ) || p["ephemeral"] == true
        {
            return Ok(v);
        }
        if v.get("id").is_none() {
            return Err("Lifecycle operations require a JSON-RPC request ID".into());
        }
        if truthy(&p["history"]) || truthy(&p["path"]) {
            return Err("Use a native thread ID to resume through HAPI".into());
        }
        if let Some(id) = thread.as_ref().filter(|_| existing.is_none()) {
            let mut candidate = id.clone();
            let mut seen = HashSet::new();
            loop {
                if !seen.insert(candidate.clone()) {
                    return Err("Invalid native thread ancestry".into());
                }
                rt.with_owner(&candidate, || async { Ok(()) }).await?;
                let response = rt
                    .rpc(
                        "thread/read",
                        json!({"threadId":candidate,"includeTurns":false}),
                    )
                    .await?;
                let Some(parent) = text(&response["thread"], "parentThreadId") else {
                    break;
                };
                existing = rt.root(parent);
                if existing.is_some() {
                    break;
                }
                candidate = parent.into();
            }
            if candidate != *id && existing.is_none() {
                return Err("Resume the parent HAPI session before attaching a child agent".into());
            }
        }
        if let Some(root) = existing.as_ref().filter(|_| method == "thread/resume") {
            v["params"] = root.config(p)?;
            return Ok(v);
        }
        if let Some(id) = &thread {
            rt.with_owner(id, || async { Ok(()) }).await?;
        }
        let cwd = text(&p, "cwd")
            .map(PathBuf::from)
            .or_else(|| existing.as_ref().map(|r| r.cwd().into()))
            .unwrap_or_else(|| rt.cwd.clone());
        let root = if let Some(t) = thread.as_ref().filter(|_| method == "thread/resume") {
            rt.cold(t, &cwd, None, false).await?
        } else {
            rt.prepare(
                &cwd,
                None,
                existing.as_deref().filter(|_| method == "thread/fork"),
                false,
            )
            .await?
        };
        if method != "thread/resume" {
            rt.pending(&root).await?;
        }
        rt.state.lock().unwrap().reservations.insert(
            Self::key(connection, &v),
            Reservation { root: root.clone() },
        );
        if method == "thread/fork" {
            p["deferGoalContinuation"] = json!(true);
        }
        v["params"] = root.config(p)?;
        Ok(v)
    }
    async fn after(&self, request: Value, response: Value, connection: &str) -> Result<Vec<Value>> {
        let rt = &self.0;
        let _op = rt.admit().await?;
        let method = text(&request, "method").unwrap_or("");
        let reservation = rt
            .state
            .lock()
            .unwrap()
            .reservations
            .remove(&Self::key(connection, &request));
        let success = !truthy(&response["error"]);
        if let Some(r) = reservation {
            if !success {
                rt.discard(&r.root).await?;
                return Ok(vec![]);
            }
            rt.bind(
                r.root,
                response["result"].clone(),
                true,
                &Value::Null,
                method == "thread/start",
            )
            .await?;
        } else if let Some(root) = text(&request["params"], "threadId")
            .and_then(|id| rt.root(id))
            .filter(|_| success)
        {
            if method == "thread/archive" {
                rt.end(&root, false).await?;
            } else if method == "thread/queue/delete" && response["result"]["deleted"] == true {
                if let Some(id) = text(&request["params"], "queuedSubmissionId") {
                    root.native_queue_deleted(id).await?;
                }
            }
        }
        if success && matches!(method, "thread/start" | "thread/resume" | "thread/fork") {
            if let Some(r) = text(&response["result"]["thread"], "id").and_then(|id| rt.root(id)) {
                return Ok(r.replay_settings());
            }
        }
        Ok(vec![])
    }
}
impl Hooks for NativeHooks {
    fn before<'a>(&'a self, v: Value, c: &'a str) -> BoxFuture<'a, Result<Value>> {
        let hooks = Self(self.0.clone());
        let connection = c.to_owned();
        async move {
            tokio::spawn(async move { hooks.before(v, &connection).await })
                .await
                .map_err(|e| e.to_string())?
        }
        .boxed()
    }
    fn after<'a>(&'a self, r: Value, v: Value, c: &'a str) -> BoxFuture<'a, Result<Vec<Value>>> {
        let hooks = Self(self.0.clone());
        let connection = c.to_owned();
        async move {
            tokio::spawn(async move { hooks.after(r, v, &connection).await })
                .await
                .map_err(|e| e.to_string())?
        }
        .boxed()
    }
    fn control(&self, m: String, p: Value) -> BoxFuture<'_, Result<Value>> {
        async move {
            let rt = &self.0;
            let _op = rt.admit().await?;
            let sid = text(&p, "sessionId").unwrap_or("");
            let root = rt
                .state
                .lock()
                .unwrap()
                .roots
                .values()
                .find(|r| r.session.id == sid)
                .cloned()
                .ok_or("Shared session not found")?;
            match m.as_str() {
                "hapi/stopSession" => {
                    rt.end(&root, true).await?;
                    Ok(json!({"stopped":true}))
                }
                "hapi/attach" => Ok(json!({"threadId":root.thread_id()})),
                _ => Err("Unknown runtime control".into()),
            }
        }
        .boxed()
    }
    fn disconnected(&self, c: &str) {
        for ((conn, _), r) in &self.0.state.lock().unwrap().reservations {
            if conn == c {
                r.root.session.send_session_event(json!({"type":"message","message":"Native lifecycle outcome is unknown after transport loss. Do not retry blindly; inspect the shared runtime log."}), None, None);
            }
        }
    }
}

pub(super) async fn run(
    config: Arc<Config>,
    rest: Rest,
    command: PathBuf,
    home: PathBuf,
    launch: Launch,
    mut options: Value,
    progress: &mut Startup,
) -> io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut exited = signal(SignalKind::child())?;
    let id = Uuid::new_v4().to_string();
    let pid = std::process::id();
    let marker = start_marker(pid)
        .await
        .ok_or_else(|| io::Error::other("Cannot verify the runtime process generation"))?;
    let mut server = Server::spawn(&command, &launch, &home).await?;
    let record = Record {
        id: id.clone(),
        pid: pid.try_into().unwrap(),
        marker,
        kind: None,
        native_endpoint: None,
        attached_at: None,
        supersedes: None,
        server_pid: server.pid.try_into().ok(),
        server_marker: Some(server.marker.clone()),
        endpoint: String::new(),
        token: None,
        command: command.to_string_lossy().into(),
        args: vec![],
        codex_home: home.to_string_lossy().into(),
        hub: config.api_url.clone(),
        auth_hash: registry::auth_hash(&config.cli_api_token),
        pending_creations: None,
        sessions: Default::default(),
    };
    let rt = Arc::new(Runtime {
        command,
        models: Default::default(),
        retiring: Default::default(),
        config,
        rest,
        id,
        home,
        upstream: server.socket(),
        cwd: launch.cwd.clone(),
        record: Arc::new(Mutex::new(record)),
        state: Mutex::new(State {
            reserved: take_reserved_session_id(&mut options),
            ..Default::default()
        }),
        control: Default::default(),
        operations: Default::default(),
        stop: watch::channel(None).0,
        activated: false.into(),
    });
    let pid = server.pid;
    let watcher = rt.clone();
    let monitor = tokio::spawn(async move {
        let engine_exit = async move {
            loop {
                if engine_exited(pid) {
                    break;
                }
                exited.recv().await;
            }
        };
        let reason = tokio::select! {
            _=term.recv()=>Ok(()),
            _=int.recv()=>Ok(()),
            _=hup.recv()=>Ok(()),
            _=engine_exit=>Err("Codex app-server exited".into())
        };
        watcher.stop(reason);
    });
    let mut gateway = None;
    let mut stopping = rt.stop.subscribe();
    let startup: Result<()> = async {
        rt.edit(|_| Ok(())).await?;
        let control = tokio::select! {
            result=server.initialize()=>result.map_err(|e|e.to_string())?,
            _=stopping.wait_for(Option::is_some)=>return Ok(())
        };
        rt.running()?;
        rt.observe(control);
        let path = rt.upstream.with_file_name("clients.sock");
        let g = Gateway::start(
            Some(&path),
            rt.upstream.clone(),
            String::new(),
            Arc::new(NativeHooks(rt.clone())),
        )
        .await
        .map_err(|e| e.to_string())?;
        let endpoint = g.endpoint.clone();
        gateway = Some(g);
        rt.edit(move |r| {
            r.endpoint = endpoint;
            Ok(())
        })
        .await?;
        let _op = rt.admit().await?;
        rt.initial(launch, options).await
    }
    .await;
    if let Err(e) = startup {
        rt.stop(Err(e));
    }
    let reason = stopping
        .wait_for(Option::is_some)
        .await
        .map_err(io::Error::other)?
        .as_ref()
        .unwrap()
        .clone();
    monitor.abort();
    let cleanup = rt.shutdown(server, gateway).await;
    progress.session = rt.state.lock().unwrap().first_id.clone();
    progress.activated = rt.activated.load(Ordering::Acquire);
    cleanup.and(reason).map_err(io::Error::other)
}

fn engine_exited(pid: u32) -> bool {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    let status = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if status == -1 {
        return io::Error::last_os_error().kind() != io::ErrorKind::Interrupted;
    }
    unsafe { info.si_pid() != 0 }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}
