use super::*;
use futures_util::{SinkExt, StreamExt};
use hapi_wrapper_core::{config::Config, rest::Rest};
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::{Child, Command},
    sync::broadcast,
    time::{sleep, timeout},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
struct Engine {
    state: Mutex<Value>,
    packets: broadcast::Sender<Value>,
    calls: Mutex<Vec<Value>>,
    connections: AtomicU64,
}
impl Engine {
    fn start(path: &Path) -> Arc<Self> {
        let listener = UnixListener::bind(path).unwrap();
        let engine = Arc::new(Self {
            state: Mutex::new(
                json!({"settings":{"model":"mock","collaborationMode":{"mode":"default"}},"queue":[],"turns":[]}),
            ),
            packets: broadcast::channel(32).0,
            calls: Mutex::new(vec![]),
            connections: 0.into(),
        });
        let weak = Arc::downgrade(&engine);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Some(e) = weak.upgrade() else {
                    break;
                };
                tokio::spawn(async move {
                    let mut ws = accept_async(stream).await.unwrap();
                    let mut broadcasts = e.packets.subscribe();
                    e.connections.fetch_add(1, Ordering::Relaxed);
                    loop {
                        let response = tokio::select! {
                            notice=broadcasts.recv()=>{let Ok(v)=notice else{break};if v.is_null(){break}v},
                            packet = ws.next() => {
                                let Some(Ok(Message::Text(raw))) = packet else { break };
                                let req: Value = serde_json::from_str(raw.as_str()).unwrap();
                                e.calls.lock().unwrap().push(req.clone());
                                if req.get("id").is_none() || req.get("method").is_none() { continue; }
                                let opening={
                                    let mut s=e.state.lock().unwrap();
                                    if s["dropMethod"]==req["method"] && (s["dropThread"].is_null()||s["dropThread"]==req["params"]["threadId"]){
                                        s["dropMethod"]=Value::Null;break
                                    }
                                    if req["method"]=="initialize"{s.as_object_mut().unwrap().remove("opening")}else{None}
                                };
                                for packet in opening.as_ref().and_then(Value::as_array).into_iter().flatten(){
                                    ws.send(Message::text(packet.to_string())).await.unwrap();
                                }
                                let reply = e.reply(&req);
                                let defer = {
                                    let mut s = e.state.lock().unwrap();
                                    if s["defer"] == req["method"] {
                                        s["defer"] = Value::Null;
                                        s["heldReply"] = reply.clone();
                                        true
                                    } else {false}
                                };
                                if defer { continue; }
                                reply
                            }
                        };
                        if ws.send(Message::text(response.to_string())).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        engine
    }
    fn reply(&self, req: &Value) -> Value {
        let p = &req["params"];
        let id = &p["threadId"];
        let mut s = self.state.lock().unwrap();
        let method = req["method"].as_str().unwrap();
        if s["reject"] == method {
            return json!({"id":req["id"],"error":{"message":"scripted rejection"}});
        }
        let result = match method {
            "initialize"
            | "turn/interrupt"
            | "turn/steer"
            | "turn/start"
            | "thread/compact/start" => json!({}),
            "thread/read" | "thread/resume" => {
                let mut t = json!({"id":id,"turns":s["turns"],"historyMode":s["historyMode"]});
                if let Some(v) = s["children"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|t| t["id"] == *id)
                {
                    t = merge(t, v);
                }
                merge(s["settings"].clone(), &json!({"thread":t}))
            }
            "thread/turns/list" => json!({"data":s["turns"]}),
            "thread/list" => json!({"data":s["children"].as_array().cloned().unwrap_or_default()}),
            "thread/loaded/list" => {
                json!({"data":s["loaded"].as_array().cloned().unwrap_or_default()})
            }
            "thread/queue/list" => json!({"data":s["queue"]}),
            "thread/queue/add" => {
                let sub = json!({"id":"native","input":p["input"],"clientUserMessageId":p["clientUserMessageId"]});
                s["queue"].as_array_mut().unwrap().push(sub.clone());
                if s["lostAdd"] == true {
                    json!({})
                } else {
                    json!({"queuedSubmission":sub})
                }
            }
            "thread/queue/update" => {
                let sub = s["queue"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|v| v["id"] == p["queuedSubmissionId"])
                    .unwrap();
                sub["input"] = p["input"].clone();
                json!({"queuedSubmission":sub})
            }
            "thread/queue/delete" => {
                s["queue"] = json!([]);
                json!({"deleted":true})
            }
            "thread/settings/update" => {
                s["settings"] = merge(s["settings"].clone(), p);
                if s["holdSettings"] != true {
                    let _ = self.packets.send(json!({"method":"thread/settings/updated","params":{"threadId":id,"threadSettings":s["settings"]}}));
                }
                json!({})
            }
            "model/list" => {
                json!({"data":[{"model":"mock","defaultReasoningEffort":"high","serviceTiers":[{"id":"priority","name":"Fast"}]}]})
            }
            m => panic!("unexpected RPC {m}"),
        };
        json!({"id":req["id"],"result":result})
    }
    fn emit(&self, method: &str, p: Value) {
        self.packets
            .send(json!({"method":method,"params":p}))
            .unwrap();
    }
}
#[derive(Default)]
struct Host {
    ended: AtomicU64,
    bridge: AtomicU64,
    created: Mutex<Vec<Value>>,
    child: Mutex<Option<Arc<Root>>>,
}
impl RootHost for Host {
    fn create<'a>(
        &'a self,
        m: &'a str,
        p: Value,
        _: Option<&'a Root>,
    ) -> BoxFuture<'a, Result<Arc<Root>>> {
        self.created.lock().unwrap().push(json!([m, p]));
        async {
            self.child
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| "No prepared child".into())
        }
        .boxed()
    }
    fn end<'a>(&'a self, _: &'a Root, _: bool, _: Option<&'a str>) -> BoxFuture<'a, Result<()>> {
        async {
            self.ended.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        .boxed()
    }
    fn settings_for(&self, _: &str) -> Value {
        json!({})
    }
    fn instructions(&self) -> String {
        "instructions".into()
    }
    fn prepare_bridge<'a>(
        &'a self,
        _: &'a SessionClient,
        _: &'a Path,
    ) -> BoxFuture<'a, Result<Value>> {
        async {
            self.bridge.fetch_add(1, Ordering::Relaxed);
            Ok(json!({"command":"wrapper","args":["mcp"]}))
        }
        .boxed()
    }
    fn stop_bridge(&self, _: &str) {}
}
struct Fixture {
    _dir: tempfile::TempDir,
    hub: Child,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    root: Arc<Root>,
    host: Arc<Host>,
    engine: Arc<Engine>,
}
impl Fixture {
    async fn new(id: &str, attached: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("engine.sock");
        let engine = Engine::start(&endpoint);
        let mut hub = Command::new("bun")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../hapi-wrapper-core/tests/session_hub.cjs"
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(hub.stdout.take().unwrap()).lines();
        let port = lines.next_line().await.unwrap().unwrap();
        let config = Config {
            home: dir.path().into(),
            api_url: format!("http://127.0.0.1:{port}"),
            cli_api_token: "token".into(),
            extra_headers: vec![("x-fixture".into(), "extra".into())],
            machine_id: None,
        };
        let (session, events) = SessionClient::start(&config, Rest::new(&config).await.unwrap(), &json!({"id":"s","metadata":{"host":"test","path":"/tmp"},"agentState":{},"metadataVersion":1,"agentStateVersion":1})).unwrap();
        let options = Options {
            directory: dir.path().into(),
            cwd: dir.path().into(),
            endpoint,
            token: None,
            generation: "gen".into(),
            attached,
        };
        let host = Arc::new(Host::default());
        let root = Root::prepare(session, events, options, host.clone())
            .await
            .unwrap();
        root.bind(id, json!({"model":"mock","thread":{"turns":[]}}), attached)
            .await
            .unwrap();
        root.activate(&json!({})).await.unwrap();
        let mut fixture = Self {
            _dir: dir,
            hub,
            lines,
            root,
            host,
            engine,
        };
        timeout(Duration::from_secs(4), async {
            loop {
                if fixture.op("snapshot", Value::Null).await["connections"]
                    .as_u64()
                    .unwrap()
                    > 0
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        fixture
    }
    async fn op(&mut self, op: &str, value: Value) -> Value {
        self.hub
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("{}\n", json!({"op":op,"value":value})).as_bytes())
            .await
            .unwrap();
        serde_json::from_str(&self.lines.next_line().await.unwrap().unwrap()).unwrap()
    }
}
async fn until(mut check: impl FnMut() -> bool) {
    timeout(Duration::from_secs(4), async {
        while !check() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn binding_settings_new_fork_and_archive() {
    let mut f = Fixture::new("main", false).await;
    let root = f.root.clone();
    let child = Fixture::new("fork", false).await;
    *f.host.child.lock().unwrap() = Some(child.root.clone());
    assert_eq!(f.host.bridge.load(Ordering::Relaxed), 1);
    assert_eq!(
        root.config(json!({})).unwrap()["config"]["shell_environment_policy.set.HAPI_SESSION_ID"],
        "s"
    );
    let applied = root.apply_settings(&json!({"permissionMode":"autoReview","modelReasoningEffort":null,"collaborationMode":"plan"})).await.unwrap();
    assert_eq!(applied["applied"]["permissionMode"], "autoReview");
    assert_eq!(
        root.replay_settings()[0]["params"]["threadSettings"]["effort"],
        "high"
    );
    assert!(root.bind("other", json!({}), false).await.is_err());
    let turns = json!([{"id":"one","status":"completed","items":[{"type":"userMessage","id":"u","content":[{"type":"text","text":"hello"}]}]}]);
    f.engine.state.lock().unwrap()["turns"] = turns.clone();
    child.engine.state.lock().unwrap()["turns"] = turns;
    root.refresh().await.unwrap();
    let result = root
        .control("fork-conversation", json!({"latestCompleted":true}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["completedTurnId"], "one");
    assert_eq!(f.host.created.lock().unwrap()[0][1]["lastTurnId"], "one");
    root.control(
        "fork-conversation",
        json!({"messageLocalId":"codex:main:user:u"}),
    )
    .await
    .unwrap();
    assert_eq!(f.host.created.lock().unwrap()[1][1]["beforeTurnId"], "one");
    root.control("clear-conversation", json!({})).await.unwrap();
    assert_eq!(f.host.created.lock().unwrap()[2][0], "thread/start");
    let result = f
        .op("rpc", json!({"method":"s:listCodexModels","params":"{}"}))
        .await;
    assert!(result.as_str().unwrap().contains("priority"));
    f.engine.emit(
        "turn/started",
        json!({"threadId":"main","turn":{"id":"live"}}),
    );
    until(|| root.state.lock().unwrap().turn.is_some()).await;
    root.control("abort", json!({})).await.unwrap();
    assert!(f
        .engine
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c["method"] == "turn/interrupt" && c["params"]["turnId"] == "live"));
    root.close(true).await;
    assert_eq!(
        f.op("snapshot", Value::Null).await["row"]["metadata"]["lifecycleState"],
        "archived"
    );
    child.root.close(false).await;
}
#[tokio::test]
async fn live_permissions_reconnect_suspension() {
    let mut f = Fixture::new("thread", false).await;
    let r = f.root.clone();
    f.engine.packets.send(json!({"id":"approval","method":"item/tool/requestUserInput","params":{"threadId":"thread","turnId":"turn","itemId":"item","questions":[{"id":"q"}]}})).unwrap();
    let key = "gen:thread:string:approval";
    until(|| {
        r.session.agent_state.get().value["requests"]
            .get(key)
            .is_some()
    })
    .await;
    r.control(
        "permission",
        json!({"id":key,"approved":true,"answers":{"q":["A"]}}),
    )
    .await
    .unwrap();
    until(|| {
        f.engine.calls.lock().unwrap().iter().any(|v| {
            v["id"] == "approval" && v["result"]["answers"]["q"]["answers"] == json!(["A"])
        })
    })
    .await;
    f.engine.emit(
        "serverRequest/resolved",
        json!({"threadId":"thread","requestId":"approval"}),
    );
    until(|| r.session.agent_state.get().value["completedRequests"][key]["status"] == "resolved")
        .await;
    let _ = f.engine.packets.send(Value::Null);
    until(|| {
        f.engine.connections.load(Ordering::Acquire) > 1 && !r.state.lock().unwrap().reconnecting
    })
    .await;
    assert!(f
        .engine
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c["method"] == "thread/resume" && c["params"]["excludeTurns"] == true));
    let ready = |v: &Value| {
        v["emitted"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event"] == "session-ready")
            .count()
    };
    let before = ready(&f.op("snapshot", Value::Null).await);
    f.op("drop", Value::Null).await;
    timeout(Duration::from_secs(8), async {
        loop {
            if ready(&f.op("snapshot", Value::Null).await) > before {
                break;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    r.q()
        .unwrap()
        .enqueue(
            "row",
            vec![json!({"type":"text","text":"save"})],
            false,
            None,
        )
        .await
        .unwrap();
    r.suspend().await.unwrap();
    assert_eq!(r.q().unwrap().state("row"), Some(queue::State::Released));
    r.close(false).await;
    assert_ne!(
        f.op("snapshot", Value::Null).await["row"]["metadata"]["lifecycleState"],
        "archived"
    );
}
#[tokio::test]
async fn attached_root_controls() {
    let f = Fixture::new("external", true).await;
    assert_eq!(f.host.bridge.load(Ordering::Relaxed), 0);
    assert!(f.root.config(json!({})).is_err());
    for method in [
        "clear-conversation",
        "rewind-conversation",
        "fork-conversation",
    ] {
        assert!(f
            .root
            .control(method, json!({}))
            .await
            .unwrap_err()
            .contains("owning desktop"))
    }
    f.engine
        .emit("thread/reverted", json!({"threadId":"external"}));
    until(|| f.host.ended.load(Ordering::Relaxed) == 1).await;
    assert!(f.root.stopped());
    f.root.close(false).await;
}
#[test]
fn immutable_cutoff_and_mentions() {
    for turns in [
        json!([]),
        json!([{"id":"a","status":"unknown"}]),
        json!([{"id":"a","status":"completed"},{"id":"a","status":"completed"}]),
    ] {
        assert!(latest_completed_cutoff(turns.as_array().unwrap()).is_err());
    }
    assert_eq!(latest_completed_cutoff(json!([{"id":"a","status":"completed"},{"id":"b","status":"failed"},{"id":"live","status":"inProgress"}]).as_array().unwrap()).unwrap(), "a");
}

#[tokio::test]
async fn plan_requires_fresh_evidence_and_is_submitted_once() {
    let f = Fixture::new("root", false).await;
    let r = &f.root;
    let item = json!({"id":"p","type":"plan","text":"the plan"});
    let turn = json!({"id":"t","status":"completed","items":[item]});
    let plan_id = plan::proposal_id("root", "t", "p");
    r.apply_settings(&json!({"collaborationMode":"plan"}))
        .await
        .unwrap();
    let implement = || r.control("implement-codex-plan", json!({"planId":plan_id}));
    assert_eq!(implement().await.unwrap().unwrap()["code"], "stale_plan");
    f.engine.state.lock().unwrap()["turns"] = json!([turn]);
    r.refresh().await.unwrap();
    until(|| r.session.agent_state.get().value["codexPlanProposalId"] == plan_id).await;
    f.engine.state.lock().unwrap()["holdSettings"] = json!(true);
    let mut pending = Box::pin(implement());
    tokio::select! {result=&mut pending=>panic!("early response {result:?}"),_=until(||f.engine.state.lock().unwrap()["settings"]["collaborationMode"]["mode"]=="default")=>{}}
    f.engine.emit(
        "turn/started",
        json!({"threadId":"root","turn":{"id":"competing"}}),
    );
    let settings = f.engine.state.lock().unwrap()["settings"].clone();
    f.engine.emit(
        "thread/settings/updated",
        json!({"threadId":"root","threadSettings":settings}),
    );
    assert_eq!(pending.await.unwrap().unwrap()["code"], "stale_plan");
    assert!(f.engine.state.lock().unwrap()["queue"]
        .as_array()
        .unwrap()
        .is_empty());
    f.engine.state.lock().unwrap()["holdSettings"] = json!(false);
    r.apply_settings(&json!({"collaborationMode":"plan"}))
        .await
        .unwrap();
    assert_eq!(implement().await.unwrap().unwrap(), json!({"ok":true}));
    assert_eq!(implement().await.unwrap().unwrap(), json!({"ok":true}));
    assert_eq!(
        f.engine.state.lock().unwrap()["queue"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        f.engine.state.lock().unwrap()["queue"][0]["clientUserMessageId"],
        plan::implementation_id(&plan_id)
    );
    r.close(false).await;
}
#[tokio::test]
async fn compaction_only_marks_recently_dequeued_members() {
    let f = Fixture::new("root", false).await;
    let root = &f.root;
    let q = root.q().unwrap();
    let a = vec![json!({"type":"text","text":"A"})];
    let b = vec![json!({"type":"text","text":"B"})];
    q.enqueue("a", a, false, Some("mode")).await.unwrap();
    q.enqueue("b", b, false, Some("mode")).await.unwrap();
    let batch = f.engine.state.lock().unwrap()["queue"][0].clone();
    f.engine.state.lock().unwrap()["queue"] = json!([]);
    f.engine
        .emit("thread/queue/changed", json!({"threadId":"root"}));
    f.engine
        .emit("turn/started", json!({"threadId":"root","turn":{"id":"t"}}));
    f.engine.emit(
        "item/started",
        json!({"threadId":"root","turnId":"t","item":{"id":"c","type":"contextCompaction"}}),
    );
    until(|| {
        root.session.agent_state.get().value["codexCompactionPendingMessageIds"]
            == json!(["a", "b"])
    })
    .await;
    f.engine.emit(
        "item/completed",
        json!({"threadId":"root","turnId":"old","item":{"id":"c","type":"contextCompaction"}}),
    );
    f.engine.emit("item/started", json!({"threadId":"root","turnId":"t","item":{"id":"user","type":"userMessage","clientId":batch["clientUserMessageId"],"content":batch["input"]}}));
    until(|| {
        q.state("a") == Some(queue::State::Consumed)
            && q.state("b") == Some(queue::State::Consumed)
            && root.session.agent_state.get().value["codexCompactionPendingMessageIds"] == json!([])
    })
    .await;
    root.close(false).await;
}
#[tokio::test]
async fn delayed_history_and_settings_acknowledgements() {
    let f = Fixture::new("t", false).await;
    let r = &f.root;
    f.engine.state.lock().unwrap()["defer"] = json!("thread/read");
    let mut pending = Box::pin(r.refresh());
    tokio::select! {result=&mut pending=>panic!("refresh returned early {result:?}"),_=until(||!f.engine.state.lock().unwrap()["heldReply"].is_null())=>{}}
    f.engine.emit(
        "turn/completed",
        json!({"threadId":"t","turn":{"id":"first","status":"completed"}}),
    );
    f.engine
        .emit("turn/started", json!({"threadId":"t","turn":{"id":"next"}}));
    until(|| r.state.lock().unwrap().turn.as_deref() == Some("next")).await;
    let reply = f.engine.state.lock().unwrap()["heldReply"].clone();
    f.engine.packets.send(reply).unwrap();
    pending.await.unwrap();
    assert_eq!(r.state.lock().unwrap().turn.as_deref(), Some("next"));
    until(|| {
        r.session.metadata.get().value["capabilities"]["conversationHistory"]["forkLatestCompleted"]
            == true
    })
    .await;
    f.engine.state.lock().unwrap()["holdSettings"] = json!(true);
    let config = json!({"model":"new-model"});
    let mut change = Box::pin(r.apply_settings(&config));
    tokio::select! {_= &mut change=>panic!("settings require a native update"),_=until(||f.engine.state.lock().unwrap()["settings"]["model"]=="new-model")=>{}}
    f.engine.emit(
        "thread/settings/updated",
        json!({"threadId":"t","threadSettings":{"model":"other"}}),
    );
    assert!(timeout(Duration::from_millis(20), &mut change)
        .await
        .is_err());
    f.engine.emit(
        "thread/settings/updated",
        json!({"threadId":"t","threadSettings":{"model":"new-model"}}),
    );
    assert_eq!(change.await.unwrap()["applied"]["model"], "new-model");
    r.close(false).await;
}
#[tokio::test]
async fn child_discovery_paginated_history_and_optional_question() {
    let f = Fixture::new("r", false).await;
    let r = &f.root;
    let item =
        json!({"id":"qi","type":"agentMessage","delivery":"async","questions":[{"title":"what?"}]});
    {
        let mut s = f.engine.state.lock().unwrap();
        s["children"] = json!([{"id":"child","parentThreadId":"r","historyMode":"full","turns":[{"id":"c","status":"inProgress","items":[item]}]}]);
        s["loaded"] = json!(["child"]);
        s["historyMode"] = json!("paginated");
    }
    r.children(true).await.unwrap();
    let questions = crate::codex::async_questions::questions("child", &item);
    let key = questions[0]["id"].as_str().unwrap();
    let qi = questions[0]["questionItemId"].as_str().unwrap();
    until(|| {
        r.session.agent_state.get().value["requests"]
            .get(key)
            .is_some()
    })
    .await;
    r.control(
        "permission",
        json!({"id":key,"approved":true,"answers":{qi:["user_note: answer"]}}),
    )
    .await
    .unwrap();
    assert!(f
        .engine
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|v| v["method"] == "turn/steer"
            && v["params"]["threadId"] == "child"
            && v["params"]["expectedTurnId"] == "c"));
    assert!(r.session.agent_state.get().value["requests"]
        .get(key)
        .is_some());
    r.refresh().await.unwrap();
    assert!(f
        .engine
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|v| v["method"] == "thread/turns/list" && v["params"]["sortDirection"] == "asc"));
    let _ = f.engine.packets.send(Value::Null);
    until(|| {
        f.engine.connections.load(Ordering::Relaxed) > 1 && !r.state.lock().unwrap().reconnecting
    })
    .await;
    assert!(
        f.engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "thread/resume" && v["params"]["threadId"] == "child")
            .count()
            >= 2
    );
    r.close(false).await;
}

#[tokio::test]
async fn hub_user_delivery_serializes_settings_and_queueing() {
    let mut f = Fixture::new("t", false).await;
    for (seq, id, msg) in [(1, "m1", "hello"), (2, "cmd", "/plan"), (3, "m2", "done")] {
        f.op("update", json!({"t":"new-message","message":{"id":id,"localId":id,"seq":seq,"createdAt":seq,"content":{"role":"user","content":{"type":"text","text":msg},"meta":{"sentFrom":"web"}}}})).await;
    }
    until(|| {
        f.engine.state.lock().unwrap()["queue"]
            .as_array()
            .unwrap()
            .len()
            == 2
    })
    .await;
    let rows = f.engine.state.lock().unwrap()["queue"].clone();
    assert_eq!(rows[0]["input"][0]["text"], "hello");
    assert_eq!(rows[1]["input"][0]["text"], "done");
    assert_eq!(
        f.root.q().unwrap().state("cmd"),
        Some(queue::State::Consumed)
    );
    f.root.control("abort", json!({})).await.unwrap();
    assert!(!f
        .engine
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c["method"] == "turn/interrupt"));
    let combined = json!([{"type":"text","text":"hello"}]);
    let first = rows[0]["clientUserMessageId"].as_str().unwrap();
    assert_eq!(
        f.root
            .q()
            .unwrap()
            .original_boundary_id(first, &combined)
            .as_deref(),
        Some("m1")
    );
    f.root.close(false).await;
}
#[tokio::test]
async fn archived_latch_and_attached_validation() {
    for method in [
        "thread/reverted",
        "thread/rolledBack",
        "thread/closed",
        "thread/deleted",
    ] {
        let f = Fixture::new("external", true).await;
        f.engine.emit(method, json!({"threadId":"external"}));
        until(|| f.host.ended.load(Ordering::Relaxed) == 1).await;
        assert!(f.root.stopped());
        assert!(!f
            .engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "thread/archive"));
        f.root.close(false).await;
    }
    let mut f = Fixture::new("owned", false).await;
    f.op("update", json!({"t":"update-session","id":"s","metadata":{"version":100,"value":{"path":"/tmp","host":"test","lifecycleState":"archived","archivedBy":"hub"}}})).await;
    until(|| f.host.ended.load(Ordering::Relaxed) > 0).await;
    assert!(f.root.stopped());
    f.root.close(true).await;
}
#[tokio::test]
async fn lost_plan_dispatch_and_failed_turns() {
    let f = Fixture::new("t", false).await;
    let r = &f.root;
    r.apply_settings(&json!({"collaborationMode":"plan"}))
        .await
        .unwrap();
    let id = plan::proposal_id("t", "one", "p");
    let item = json!({"id":"p","type":"plan","text":"do it"});
    for status in ["failed", "interrupted", "inProgress", "completed"] {
        f.engine.state.lock().unwrap()["turns"] =
            json!([{"id":"one","status":status,"items":[item]}]);
        r.refresh().await.unwrap();
        let allowed = r.available(&r.state.lock().unwrap());
        assert_eq!(
            allowed.as_deref(),
            (status == "completed").then_some(id.as_str())
        );
    }
    f.engine.state.lock().unwrap()["lostAdd"] = json!(true);
    for _ in 0..2 {
        let res = r
            .control("implement-codex-plan", json!({"planId":id}))
            .await
            .unwrap()
            .unwrap();
        if r.q().unwrap().state(&plan::implementation_id(&id)) == Some(queue::State::Unknown) {
            assert_eq!(res["code"], "indeterminate");
        } else {
            assert_eq!(res["ok"], true);
        }
    }
    assert_eq!(
        f.engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "thread/queue/add")
            .count(),
        1
    );
    r.close(false).await;
}

#[tokio::test]
async fn reconnect_installs_permission_generation_before_initial_packets() {
    let f = Fixture::new("root", false).await;
    f.root.withdraw();
    let request = |id| json!({"method":"item/tool/requestUserInput","id":id,"params":{"threadId":"root","turnId":"t","itemId":"q","questions":[]}});
    f.engine.state.lock().unwrap()["opening"] = json!([
        {"method":"serverRequest/resolved","params":{"threadId":"root","requestId":1}}, request(1),request(2)
    ]);
    f.root.connect().await.unwrap();
    until(|| {
        f.root.session.agent_state.get().value["requests"]
            .as_object()
            .is_some_and(|m| !m.is_empty())
    })
    .await;
    until(|| f.root.session.agent_state.get().value["requests"].is_object()).await;
    let state = f.root.session.agent_state.get().value;
    let keys = state["requests"]
        .as_object()
        .unwrap()
        .keys()
        .collect::<Vec<_>>();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].ends_with(":root:number:2") && keys[0].split(':').count() == 5);
    f.root.close(false).await;
}
#[tokio::test]
async fn retries_disconnect_during_child_recovery() {
    let f = Fixture::new("root", false).await;
    {
        let mut s = f.engine.state.lock().unwrap();
        s["children"] = json!([{"id":"child","parentThreadId":"root"}]);
        s["loaded"] = json!(["child"]);
        s["dropMethod"] = json!("thread/resume");
        s["dropThread"] = json!("child");
    }
    f.engine.packets.send(Value::Null).unwrap();
    until(|| {
        f.engine.connections.load(Ordering::Relaxed) >= 3
            && !f.root.state.lock().unwrap().reconnecting
            && f.root.connected()
    })
    .await;
    f.root.close(false).await;
}
#[tokio::test]
async fn hub_resync_child_failure_ends_attached_view() {
    let mut f = Fixture::new("root", true).await;
    f.engine.state.lock().unwrap()["reject"] = json!("thread/list");
    f.op("drop", Value::Null).await;
    until(|| f.host.ended.load(Ordering::Relaxed) > 0).await;
    assert!(f.root.stopped());
    f.root.close(false).await;
}
#[tokio::test]
async fn archive_closes_admission_before_host_end() {
    let f = Fixture::new("root", false).await;
    f.root
        .receive("thread/archived".into(), json!({"threadId":"root"}));
    assert!(f.root.stopped());
    assert!(f
        .root
        .control("clear-conversation", json!({}))
        .await
        .is_err());
    assert!(f.host.created.lock().unwrap().is_empty());
    until(|| f.host.ended.load(Ordering::Relaxed) > 0).await;
    assert!(f.root.stopped());
    f.root.close(true).await;
}
#[tokio::test]
async fn sibling_traffic_cannot_project_or_answer() {
    let mut f = Fixture::new("root", false).await;
    until(|| f.root.session.agent_state.get().value["requests"].is_object()).await;
    let state = f.root.session.agent_state.get().value;
    f.engine.emit("item/completed",json!({"threadId":"sibling","turnId":"turn","item":{"type":"agentMessage","id":"sibling-item","text":"sibling-only"}}));
    f.engine.packets.send(json!({"id":"sibling-call","method":"item/tool/requestUserInput","params":{"threadId":"sibling","questions":[]}})).unwrap();
    f.engine.emit("item/completed",json!({"threadId":"root","item":{"type":"agentMessage","id":"parent-item","text":"parent-seen"}}));
    timeout(Duration::from_secs(4), async {
        loop {
            let snapshot = f.op("snapshot", Value::Null).await;
            let messages = snapshot["emitted"].to_string();
            if messages.contains("parent-seen") {
                assert!(!messages.contains("sibling-only"));
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        state["requests"],
        f.root.session.agent_state.get().value["requests"]
    );
    assert!(f.root.responses.lock().unwrap().is_empty());
    assert!(!f
        .engine
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "sibling-call"));
    f.root.close(false).await;
}
#[tokio::test]
async fn overlapping_refreshes_share_one_pass_and_result() {
    let f = Fixture::new("root", false).await;
    f.engine.calls.lock().unwrap().clear();
    f.engine.state.lock().unwrap()["defer"] = json!("thread/read");
    let calls = (0..5).map(|_| f.root.refresh()).collect::<Vec<_>>();
    let all = futures_util::future::join_all(calls);
    tokio::pin!(all);
    tokio::select! {_= &mut all=>panic!("unexpected completion"),_=until(||!f.engine.state.lock().unwrap()["heldReply"].is_null())=>{}}
    let reply = f.engine.state.lock().unwrap()["heldReply"].clone();
    f.engine.packets.send(reply).unwrap();
    assert!(all.await.iter().all(|r| r.is_ok()));
    assert_eq!(
        f.engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p["method"] == "thread/queue/list")
            .count(),
        1
    );
    f.root.close(false).await;
}
#[tokio::test]
async fn plan_reports_indeterminate_refresh_and_settings_dispatch() {
    for method in ["thread/read", "thread/queue/list", "thread/settings/update"] {
        let f = Fixture::new("root", false).await;
        f.engine.state.lock().unwrap()["turns"] = json!([{"id":"T","status":"completed","items":[{"id":"p","type":"plan","text":"plan"}]}]);
        f.root
            .apply_settings(&json!({"collaborationMode":"plan"}))
            .await
            .unwrap();
        f.engine.state.lock().unwrap()["dropMethod"] = json!(method);
        let result = f
            .root
            .control(
                "implement-codex-plan",
                json!({"planId":plan::proposal_id("root","T","p")}),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result["code"], "indeterminate", "{method}: {result}");
        f.root.close(false).await;
    }
}
