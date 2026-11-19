use hapi_wrapper_core::{
    config::Config,
    rest::Rest,
    session::{
        bootstrap::{bootstrap, Options},
        client::{Cancellation, SessionClient, SessionEvent},
    },
};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdout, Command},
    sync::mpsc,
    time::timeout,
};

const LIMIT: Duration = Duration::from_secs(5);
struct Hub {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    config: Config,
    _dir: tempfile::TempDir,
}
impl Hub {
    async fn new() -> Self {
        let mut child = Command::new("bun")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/session_hub.cjs"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let port = timeout(LIMIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("runner.state.json"),
            json!({"pid":child.id(),"httpPort":port}).to_string(),
        )
        .unwrap();
        let config = Config {
            home: dir.path().into(),
            api_url: format!("http://127.0.0.1:{port}"),
            cli_api_token: "token".into(),
            extra_headers: vec![
                ("x-fixture".into(), "extra".into()),
                ("authorization".into(), "conflict".into()),
            ],
            machine_id: Some("m".into()),
        };
        Self {
            child,
            lines,
            config,
            _dir: dir,
        }
    }
    async fn command(&mut self, op: &str, value: Value) -> Value {
        let line = format!("{}\n", json!({"op":op,"value":value}));
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(line.as_bytes())
            .await
            .unwrap();
        let answer = timeout(LIMIT, self.lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&answer).unwrap()
    }
    async fn start(&self) -> (Arc<SessionClient>, mpsc::UnboundedReceiver<SessionEvent>) {
        SessionClient::start(&self.config,Rest::new(&self.config).await.unwrap(),&json!({"id":"s","metadata":{"host":"test","path":"/tmp"},"metadataVersion":1,"agentState":{},"agentStateVersion":1})).unwrap()
    }
}
async fn wait(
    events: &mut mpsc::UnboundedReceiver<SessionEvent>,
    predicate: impl Fn(&SessionEvent) -> bool,
) {
    timeout(LIMIT, async {
        while let Some(e) = events.recv().await {
            if predicate(&e) {
                return;
            }
        }
        panic!("client closed")
    })
    .await
    .unwrap();
}
fn message(id: &str, n: i64, from: &str) -> Value {
    json!({"id":id,"seq":n,"createdAt":n,"localId":id,"content":{"role":"user","content":{"type":"text","text":id},"meta":{"sentFrom":from}}})
}
#[tokio::test]
async fn create_and_reopen_through_webhook() {
    if std::env::var_os("HAPI_BOOTSTRAP_TEST_CHILD").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "create_and_reopen_through_webhook"])
            .env("HAPI_BOOTSTRAP_TEST_CHILD", "1")
            .env("HAPI_SESSION_ID", "parent")
            .env("HAPI_WRAPPER_CLI_VERSION", "test-version")
            .env("HAPI_WRAPPER_RUNTIME_PATH", "/runtime")
            .status()
            .await
            .unwrap();
        assert!(status.success());
        return;
    }
    let mut h = Hub::new().await;
    let id = uuid::Uuid::new_v4().to_string();
    let opts = Options {
        flavor: "claude".into(),
        reserved_id: Some(id.clone()),
        runner: true,
        overrides: hapi_wrapper_core::session::metadata::Overrides {
            fields: [("name".into(), Some("preserved".into()))].into(),
            ..Default::default()
        },
        ..Default::default()
    };
    h.command("contract", true.into()).await;
    let created = bootstrap(&h.config, &opts, true, true, |_| false)
        .await
        .unwrap();
    assert_eq!(created.session_summary_contract, Some(true));
    assert_eq!(std::env::var("HAPI_SESSION_ID").unwrap(), "parent");
    let child = Command::new("printenv")
        .arg("HAPI_SESSION_ID")
        .envs(&created.child_env)
        .output()
        .await
        .unwrap();
    assert_eq!(String::from_utf8(child.stdout).unwrap().trim(), id);
    assert!(created.client.flush(LIMIT).await);
    let state = h.command("snapshot", Value::Null).await;
    let req = state["requests"].as_array().unwrap();
    assert_eq!(
        req.iter()
            .map(|r| r["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["/cli/machines", "/cli/sessions", "/session-started"]
    );
    assert_eq!(req[1]["body"]["id"], id);
    assert_eq!(req[1]["body"]["adopt"], true);
    assert_eq!(
        req[2]["body"],
        json!({"sessionId":id,"metadata":created.metadata})
    );
    assert_eq!(created.metadata["wrapperRuntime"], "rust");
    assert!(created.metadata["hostStartMarker"].is_string());
    assert!(req[0]["body"]["metadata"].get("capabilities").is_none());
    created.client.close();
    let mut stale = created.metadata.clone();
    stale["capabilities"]["conversationHistory"] = json!({"rewindToMessage":true});
    stale["summary"] = json!({"text":"previous","updatedAt":1});
    h.command("metadata", json!({"metadata":stale})).await;
    let mut opts = Options {
        existing_id: Some(id.clone()),
        runner: false,
        ..opts
    };
    opts.overrides.fields.insert("summary".into(), None);
    opts.overrides
        .capabilities
        .insert("conversationHistory".into(), None);
    h.command("contract", false.into()).await;
    let opened = bootstrap(&h.config, &opts, false, true, |_| true)
        .await
        .unwrap();
    let snapshot = h.command("snapshot", Value::Null).await;
    for metadata in [&opened.metadata, &snapshot["row"]["metadata"]] {
        assert!(metadata.get("summary").is_none());
        assert_eq!(metadata["capabilities"], json!({"terminal":true}));
    }
    assert!(opened.child_env.is_empty());
    assert_eq!(opened.session_summary_contract, Some(false));
    assert_eq!(opened.client.metadata.get().value["name"], "preserved");
    assert!(opened
        .client
        .metadata
        .get()
        .value
        .get("hostStartMarker")
        .is_none());
    opened.client.close();
    for (fields, flavor, expected) in [
        (
            json!({"lifecycleState":"archived","archivedBy":"hub"}),
            "claude",
            "is hub-archived",
        ),
        (
            json!({"codexRuntime":{"kind":"attached"}}),
            "codex",
            "require hapi attach",
        ),
        (
            json!({"codexArchivedFork":{}}),
            "codex",
            "pending archived fork",
        ),
    ] {
        h.command("metadata", json!({"metadata":{"path":"/","host":"x"}}))
            .await;
        let mut value = json!({"host":"test","path":"/"});
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        h.command("metadata", json!({"metadata":value})).await;
        let opts = Options {
            existing_id: Some(id.clone()),
            flavor: flavor.into(),
            ..Default::default()
        };
        assert!(bootstrap(&h.config, &opts, false, true, |_| false)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains(expected));
    }
    h.command("metadata",json!({"metadata":{"host":"test","path":"/","codexRuntime":{"kind":"attached"},"conversationHistoryDiverged":true}})).await;
    let attached = Options {
        existing_id: Some(id.clone()),
        flavor: "codex".into(),
        overrides: hapi_wrapper_core::session::metadata::Overrides {
            fields: [("codexRuntime".into(), Some(json!({"kind":"attached"})))].into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let err = bootstrap(&h.config, &attached, false, false, |_| false)
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("Native history changed"));
    let mut fresh = Options {
        flavor: "claude".into(),
        reserved_id: Some("legacy-stamp".into()),
        ..Default::default()
    };
    let rest = Rest::new(&h.config).await.unwrap();
    h.command("contract", Value::Null).await;
    let prepared = hapi_wrapper_core::session::bootstrap::prepare(&rest, &h.config, &fresh)
        .await
        .unwrap();
    assert!(prepared.session_summary_contract.is_none());
    let state = h.command("snapshot", Value::Null).await;
    let body = &state["requests"].as_array().unwrap().last().unwrap()["body"];
    assert!(body.get("adopt").is_none());
    assert!(body.get("id").is_none());
    fresh.reserved_id = Some(id.clone());
    h.command("forceId", "unexpected".into()).await;
    let error = hapi_wrapper_core::session::bootstrap::prepare(&rest, &h.config, &fresh)
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        format!("Hub returned unexpected session id unexpected (reserved {id})")
    );
}
#[tokio::test]
async fn archive_from_success_mismatch_live_or_rest() {
    for source in ["archive-success", "archive-mismatch", "live", "rest"] {
        let mut h = Hub::new().await;
        let (client, mut events) = h.start().await;
        wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
        let meta =
            json!({"path":"/tmp","host":"test","lifecycleState":"archived","archivedBy":"hub"});
        match source {
            "live" => {
                h.command(
                    "update",
                    json!({"t":"update-session","metadata":{"version":2,"value":meta}}),
                )
                .await;
            }
            "rest" => {
                h.command("metadata", json!({"metadataVersion":2,"metadata":meta}))
                    .await;
                h.command("drop", Value::Null).await;
            }
            _ => {
                h.command("ackMode", source.into()).await;
                client.metadata.update(Clone::clone);
            }
        }
        wait(&mut events, |e| matches!(e, SessionEvent::HubArchived)).await;
        assert!(client.hub_archived());
        assert!(*client.archive_notifications().borrow());
        client.metadata.update(|_| panic!("archived CAS"));
        assert!(client.flush_metadata(LIMIT).await);
        client.close();
    }
}
#[tokio::test]
async fn reconnect_reconciles_backfills_replays_and_flushes() {
    let mut h = Hub::new().await;
    let (client, mut events) = h.start().await;
    client.keep_alive(true, Some("remote"), &json!({"effort":"high"}));
    let (tx, mut users) = mpsc::unbounded_channel();
    client.on_user_message(Arc::new(move |v, _| {
        tx.send(v).unwrap();
    }));
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    h.command(
        "update",
        json!({"t":"new-message","message":message("local",1,"cli")}),
    )
    .await;
    h.command(
        "update",
        json!({"t":"new-message","message":message("first",2,"web")}),
    )
    .await;
    assert_eq!(
        timeout(LIMIT, users.recv()).await.unwrap().unwrap()["content"]["text"],
        "first"
    );
    let mut rows: Vec<_> = (3..204)
        .map(|i| message(&i.to_string(), i, "cli"))
        .collect();
    rows.push(message("last", 204, "web"));
    h.command("rows", rows.into()).await;
    h.command("drop", Value::Null).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Disconnected)).await;
    client.session_end(Some("completed"));
    assert!(client.flush(LIMIT).await);
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    assert_eq!(users.try_recv().unwrap()["content"]["text"], "last");
    assert!(users.try_recv().is_err());
    let state = h.command("snapshot", Value::Null).await;
    let req = state["requests"].as_array().unwrap();
    assert_eq!(req[0]["path"], "/cli/sessions/s");
    assert_eq!(req[1]["query"], "?afterSeq=2&limit=200");
    assert_eq!(req[2]["query"], "?afterSeq=202&limit=200");
    let events = state["emitted"].as_array().unwrap();
    let alive = events
        .iter()
        .filter(|e| e["event"] == "session-alive")
        .collect::<Vec<_>>();
    assert_eq!(alive.len(), 1);
    for e in alive {
        assert_eq!(e["payload"]["thinking"], true);
        assert_eq!(e["payload"]["effort"], "high");
    }
    assert!(events.iter().any(|e| e["event"] == "session-end"));
    h.command("ping", false.into()).await;
    assert!(!client.flush(Duration::from_millis(40)).await);
    client.close();
    assert!(!client.flush(LIMIT).await);
}
#[tokio::test]
async fn disconnect_during_cas_and_invalid_rest() {
    let mut h = Hub::new().await;
    let (c, mut e) = h.start().await;
    wait(&mut e, |e| matches!(e, SessionEvent::Connected)).await;
    h.command("ackMode", "drop".into()).await;
    c.metadata.update(|v| {
        let mut v = v.clone();
        v["name"] = "updated".into();
        v
    });
    assert!(c.flush_metadata(LIMIT).await);
    assert_eq!(c.metadata.get().version, 2);
    h.command(
        "metadata",
        json!({"metadataVersion":1,"metadata":{"path":"/","host":"old"}}),
    )
    .await;
    assert!(c.refresh_metadata().await);
    assert_eq!(c.metadata.get().value["name"], "updated");
    h.command("malformed", true.into()).await;
    assert!(!c.refresh_metadata().await);
    h.child.kill().await.unwrap();
    wait(&mut e, |e| matches!(e, SessionEvent::Disconnected)).await;
    assert!(!c.flush(Duration::from_millis(30)).await);
    assert!(!c.refresh_metadata().await);
    c.close();
}
#[tokio::test]
async fn cancel_retry_and_wire_payloads() {
    let mut h = Hub::new().await;
    let (c, mut e) = h.start().await;
    wait(&mut e, |e| matches!(e, SessionEvent::Connected)).await;
    let row = message("row", 1, "web");
    h.command("update", json!({"t":"new-message","message":row}))
        .await;
    assert_eq!(
        h.command("ack", json!({"t":"cancel-queued-message","localId":"row"}))
            .await,
        json!({"removed":true,"inFlight":false,"indeterminate":false,"consumed":false})
    );
    for (result, expected) in [
        (
            Cancellation::Consumed,
            json!({"removed":false,"accepted":false,"consumed":true}),
        ),
        (
            Cancellation::InFlight,
            json!({"removed":false,"accepted":false}),
        ),
        (
            Cancellation::Indeterminate,
            json!({"removed":false,"accepted":true}),
        ),
    ] {
        c.on_cancel(Arc::new(move |_| Box::pin(async move { Ok(result) })));
        c.on_retry(Arc::new(|_| Box::pin(async { Ok(true) })));
        assert_eq!(
            h.command(
                "ack",
                json!({"t":"retry-queued-message","localId":"row","message":row})
            )
            .await,
            expected
        );
    }
    let (tx, mut users) = mpsc::unbounded_channel();
    c.on_user_message(Arc::new(move |_, id| {
        tx.send(id).unwrap();
    }));
    assert_eq!(users.try_recv().unwrap().as_deref(), Some("row"));
    assert!(users.try_recv().is_err());
    c.send_session_event(json!({"type":"ready"}), Some("inner"), Some("outer"));
    c.send_user_message("hello", &Value::Null, Some("local"));
    c.send_agent_message(json!({"test":true}), None);
    assert!(c.steer_state(&["local".into()], "dispatching").await);
    let state = h.command("snapshot", Value::Null).await;
    let events = state["emitted"].as_array().unwrap();
    let messages: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "message")
        .map(|e| &e["payload"])
        .collect();
    assert_eq!(messages[0]["localId"], "outer");
    assert_eq!(messages[0]["message"]["content"]["id"], "inner");
    assert_eq!(messages[1]["message"]["role"], "user");
    assert_eq!(messages[2]["message"]["content"]["type"], "codex");
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "messages-consumed"
                && e["payload"]["localIds"] == json!(["local"]))
    );
    c.close();
}

#[tokio::test]
async fn rpc_registers_before_alive_and_replies_with_json_strings() {
    let mut h = Hub::new().await;
    let (c, mut events) = h.start().await;
    c.rpc.register("echo", |v| async move { Ok(Some(v)) });
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    c.rpc.register("switch", |_| async { Ok(None) });
    assert_eq!(
        h.command(
            "rpc",
            json!({"method":"s:echo","params":"{\"accepted\":true}"})
        )
        .await,
        json!("{\"accepted\":true}")
    );
    assert_eq!(
        h.command("rpc", json!({"method":"s:switch","params":"{}"}))
            .await,
        Value::Null
    );
    assert_eq!(
        h.command("rpc", json!({"method":"wrong:echo","params":"{}"}))
            .await,
        json!("{\"error\":\"Method not found\"}")
    );
    h.command("drop", Value::Null).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Disconnected)).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    assert!(c.flush(LIMIT).await);
    let snapshot = h.command("snapshot", Value::Null).await;
    let wire: Vec<_> = snapshot["emitted"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| match e["event"].as_str() {
            Some("rpc-register") => e["payload"]["method"].as_str(),
            Some("session-alive") => Some("alive"),
            _ => None,
        })
        .collect();
    assert_eq!(
        wire,
        ["s:echo", "alive", "s:switch", "s:echo", "s:switch", "alive"]
    );
    c.close();
}

#[tokio::test]
async fn final_flush_explicitly_reconnects_after_namespace_disconnect() {
    let mut hub = Hub::new().await;
    let (client, mut events) = hub.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    hub.command("disconnect", Value::Null).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Disconnected)).await;
    client.session_end(Some("completed"));
    assert!(client.flush(LIMIT).await);
    let state = hub.command("snapshot", Value::Null).await;
    assert!(state["emitted"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event"] == "session-end"));
    client.close();
}

#[tokio::test]
async fn webhook_is_best_effort_and_does_not_send_to_a_stale_port() {
    let mut hub = Hub::new().await;
    let state = hub.config.home.join("runner.state.json");
    let rest = Rest::new(&hub.config).await.unwrap();
    for invalid in [None, Some("not json"), Some(r#"{"pid":0,"httpPort":1}"#)] {
        if let Some(data) = invalid {
            std::fs::write(&state, data).unwrap();
        } else {
            std::fs::remove_file(&state).unwrap();
        }
        assert!(rest
            .report_started(&hub.config.home, "s", &json!({}))
            .await
            .is_err());
    }
    assert!(hub.command("snapshot", Value::Null).await["requests"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn native_queued_cli_rows_require_concurrent_clients_and_local_id() {
    let mut hub = Hub::new().await;
    let (client, mut events) = hub.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    let (tx, mut users) = mpsc::unbounded_channel();
    client.on_user_message(Arc::new(move |v, id| {
        tx.send((v, id)).unwrap();
    }));
    let mut row = message("first", 1, "cli");
    row["content"]["meta"]["isNativeQueuedMessage"] = true.into();
    hub.command("update", json!({"t":"new-message","message":row}))
        .await;
    hub.command("update",json!({"t":"update-session","metadata":{"version":2,"value":{"host":"test","path":"/","capabilities":{"concurrentClients":true}}}})).await;
    row["id"] = "second".into();
    row["localId"] = Value::Null;
    hub.command("update", json!({"t":"new-message","message":row}))
        .await;
    row["id"] = "third".into();
    row["localId"] = "deliver".into();
    hub.command("update", json!({"t":"new-message","message":row}))
        .await;
    assert_eq!(
        timeout(LIMIT, users.recv())
            .await
            .unwrap()
            .unwrap()
            .1
            .as_deref(),
        Some("deliver")
    );
    assert!(users.try_recv().is_err());
    client.close();
}

async fn advance_heartbeat() {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
}

#[tokio::test]
async fn heartbeat_drops_offline_and_stays_stopped_after_end() {
    let mut hub = Hub::new().await;
    let (client, mut events) = hub.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    advance_heartbeat().await;
    assert!(client.flush(LIMIT).await);
    let before = hub.command("snapshot", Value::Null).await;
    let count = |state: &Value| {
        state["emitted"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event"] == "session-alive")
            .count()
    };
    assert_eq!(count(&before), 2);
    hub.command("disconnect", Value::Null).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Disconnected)).await;
    client.keep_alive(true, Some("remote"), &json!({"model":"offline"}));
    advance_heartbeat().await;
    assert!(client.flush(LIMIT).await);
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    client.session_end(Some("completed"));
    client.keep_alive(false, Some("remote"), &Value::Null);
    advance_heartbeat().await;
    assert!(client.flush(LIMIT).await);
    let after = hub.command("snapshot", Value::Null).await;
    assert_eq!(count(&after), 3);
    let emitted = after["emitted"].as_array().unwrap();
    let end = emitted
        .iter()
        .position(|e| e["event"] == "session-end")
        .unwrap();
    assert!(emitted[end..].iter().all(|e| e["event"] != "session-alive"));
    assert_eq!(
        emitted[..end]
            .iter()
            .rfind(|e| e["event"] == "session-alive")
            .unwrap()["payload"]["model"],
        "offline"
    );
    client.close();
}

#[tokio::test]
async fn archived_drop_stops_cas_and_reconnect() {
    let mut hub = Hub::new().await;
    let (client, mut events) = hub.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    hub.command("ackMode", "hold".into()).await;
    client.agent_state.update(Clone::clone);
    timeout(LIMIT, async {
        loop {
            let state = hub.command("snapshot", Value::Null).await;
            if state["emitted"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["event"] == "update-state")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    hub.command("update",json!({"t":"update-session","metadata":{"version":2,"value":{"path":"/","host":"test","lifecycleState":"archived","archivedBy":"hub"}}})).await;
    wait(&mut events, |e| matches!(e, SessionEvent::HubArchived)).await;
    let before = hub.command("snapshot", Value::Null).await;
    advance_heartbeat().await;
    client.keep_alive(true, Some("remote"), &Value::Null);
    hub.command("drop", Value::Null).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Disconnected)).await;
    assert!(
        !timeout(Duration::from_millis(200), client.agent_state.drain())
            .await
            .unwrap()
    );
    assert!(
        !timeout(Duration::from_millis(200), client.metadata.drain())
            .await
            .unwrap()
    );
    assert!(!timeout(Duration::from_millis(200), client.flush(LIMIT))
        .await
        .unwrap());
    advance_heartbeat().await;
    let after = hub.command("snapshot", Value::Null).await;
    assert_eq!(after["connections"], before["connections"]);
    assert_eq!(after["emitted"], before["emitted"]);
    client.close();
}

#[tokio::test]
async fn stale_refresh_cannot_announce_connected() {
    let mut hub = Hub::new().await;
    let (client, mut events) = hub.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    hub.command("holdRefresh", Value::Null).await;
    hub.command("drop", Value::Null).await;
    wait(&mut events, |e| matches!(e, SessionEvent::ConnectionStart)).await;
    timeout(LIMIT, async {
        while hub.command("snapshot", Value::Null).await["refreshPending"] != true {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    hub.command("disconnect", Value::Null).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Disconnected)).await;
    assert!(client.flush(LIMIT).await);
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    hub.command("releaseRefresh", Value::Null).await;
    assert!(timeout(Duration::from_millis(150), events.recv())
        .await
        .is_err());
    hub.command("malformed", "missing".into()).await;
    let before = client.metadata.get();
    assert!(!client.refresh_metadata().await);
    assert_eq!(client.metadata.get().version, before.version);
    assert_eq!(client.metadata.get().value, before.value);
    client.close();
}

#[tokio::test]
async fn failed_dial_is_a_connect_error() {
    let mut h = Hub::new().await;
    h.child.kill().await.unwrap();
    let (client, mut e) = h.start().await;
    for _ in 0..2 {
        assert!(matches!(
            timeout(LIMIT, e.recv()).await.unwrap(),
            Some(SessionEvent::ConnectError { fatal: false, .. })
        ));
    }
    client.close();
}

#[tokio::test]
async fn replay_completes_before_live_callback() {
    let mut h = Hub::new().await;
    let (c, mut events) = h.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    for (n, id) in [(1, "first"), (2, "second")] {
        h.command(
            "update",
            json!({"t":"new-message","message":message(id,n,"web")}),
        )
        .await;
    }
    h.command("update", json!({"t":"barrier"})).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Message(_))).await;
    let (tx, mut seen) = mpsc::unbounded_channel();
    let (resume, blocked) = std::sync::mpsc::sync_channel(1);
    let blocked = std::sync::Mutex::new(blocked);
    let replay = c.clone();
    let task = tokio::task::spawn_blocking(move || {
        replay.on_user_message(Arc::new(move |_, id| {
            let id = id.unwrap();
            tx.send(id.clone()).unwrap();
            if id == "first" {
                blocked.lock().unwrap().recv_timeout(LIMIT).unwrap();
            }
        }))
    });
    assert_eq!(timeout(LIMIT, seen.recv()).await.unwrap().unwrap(), "first");
    h.command(
        "update",
        json!({"t":"new-message","message":message("third",3,"web")}),
    )
    .await;
    h.command("update", json!({"t":"barrier"})).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Message(_))).await;
    assert!(seen.try_recv().is_err());
    resume.send(()).unwrap();
    task.await.unwrap();
    assert_eq!(seen.try_recv().unwrap(), "second");
    assert_eq!(seen.try_recv().unwrap(), "third");
    c.close();
}

#[tokio::test]
async fn queue_errors_invalid_user_and_steer_summary_payloads() {
    let mut h = Hub::new().await;
    let (c, mut events) = h.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    let row = message("id", 1, "web");
    c.on_cancel(Arc::new(|_| {
        Box::pin(async { Err(std::io::Error::other("cancel failed")) })
    }));
    for kind in ["retry-queued-message", "cancel-queued-message"] {
        let ack = h
            .command("ack", json!({"t":kind,"localId":"id","message":row}))
            .await;
        assert_eq!(
            ack,
            json!({"removed":false,"accepted":false,"indeterminate":true})
        );
    }
    c.on_cancel(Arc::new(|_| {
        Box::pin(async { Ok(Cancellation::Indeterminate) })
    }));
    c.on_retry(Arc::new(|_| Box::pin(async { Ok(false) })));
    assert_eq!(
        h.command(
            "ack",
            json!({"t":"retry-queued-message","localId":"id","message":row})
        )
        .await,
        json!({"removed":false,"accepted":false})
    );
    c.on_retry(Arc::new(|_| {
        Box::pin(async { Err(std::io::Error::other("retry failed")) })
    }));
    assert_eq!(
        h.command(
            "ack",
            json!({"t":"retry-queued-message","localId":"id","message":row})
        )
        .await,
        json!({"removed":false,"accepted":false,"indeterminate":true})
    );
    for (i, content) in [
        json!({"role":"agent"}),
        json!({"role":"user","content":{"type":"text","text":"hello"},"localKey":5}),
    ]
    .into_iter()
    .enumerate()
    {
        h.command(
            "update",
            json!({"t":"new-message","message":{"seq":i+1,"content":content}}),
        )
        .await;
        wait(
            &mut events,
            |e| matches!(e,SessionEvent::Message(v) if *v==content),
        )
        .await;
    }
    c.send_claude_session_message(&json!({"type":"summary","summary":"skip"}));
    c.send_claude_session_message(&json!({"type":"summary","summary":"save","leafUuid":"leaf"}));
    c.send_agent_message(json!({"x":42}), Some("agent-id"));
    c.steer_indeterminate(&["row".into()]);
    h.command("steerReply", false.into()).await;
    assert!(!c.steer_state(&["row".into()], "queued").await);
    assert!(!c.steer_state(&[], "queued").await);
    assert!(c.flush(LIMIT).await);
    let state = h.command("snapshot", Value::Null).await;
    let emitted = state["emitted"].as_array().unwrap();
    assert_eq!(state["row"]["metadataVersion"], 2);
    assert_eq!(state["row"]["metadata"]["summary"]["text"], "save");
    assert!(state["row"]["metadata"]["summary"]["updatedAt"].is_u64());
    assert!(emitted.iter().any(|e| e["event"] == "message"
        && e["payload"]["localId"] == "agent-id"
        && e["payload"]["message"]["content"] == json!({"type":"codex","data":{"x":42}})));
    assert!(emitted
        .iter()
        .any(|e| e["event"] == "messages-consumed"
            && e["payload"]["localIds"] == json!(["agent-id"])));
    assert!(emitted
        .iter()
        .any(|e| e["event"] == "messages-indeterminate"
            && e["payload"]["localIds"] == json!(["row"])));
    c.close();
}

#[tokio::test]
async fn plain_rest_and_webhook_work_without_tls_roots() {
    if std::env::var_os("HAPI_TEST_EMPTY_ROOTS").is_none() {
        let empty = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "plain_rest_and_webhook_work_without_tls_roots"])
            .env("HAPI_TEST_EMPTY_ROOTS", "1")
            .env("SSL_CERT_DIR", empty.path())
            .env("SSL_CERT_FILE", empty.path().join("missing.pem"))
            .env_remove("NODE_EXTRA_CA_CERTS")
            .status()
            .await
            .unwrap();
        assert!(status.success());
        return;
    }
    let mut hub = Hub::new().await;
    let rest = Rest::new(&hub.config).await.unwrap();
    assert_eq!(rest.session("s").await.unwrap().session["id"], "s");
    rest.report_started(&hub.config.home, "s", &json!({}))
        .await
        .unwrap();
    hub.config.api_url = "https://localhost".into();
    assert!(Rest::new(&hub.config).await.is_err());
}

#[tokio::test]
async fn replacement_backfill_resumes_pages_while_live_messages_advance() {
    for abort in [false, true] {
        let mut hub = Hub::new().await;
        let (client, mut events) = hub.start().await;
        wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
        let (tx, mut users) = mpsc::unbounded_channel();
        client.on_user_message(Arc::new(move |_, id| {
            tx.send(id.unwrap()).unwrap();
        }));
        hub.command(
            "update",
            json!({"t":"new-message","message":message("1",1,"web")}),
        )
        .await;
        assert_eq!(timeout(LIMIT, users.recv()).await.unwrap().unwrap(), "1");
        let rows: Vec<_> = (2..=440)
            .map(|seq| message(&seq.to_string(), seq, "web"))
            .collect();
        hub.command("rows", json!(rows)).await;
        hub.command("holdRefresh", Value::Null).await;
        hub.command(if abort { "holdPage" } else { "failPage" }, 201.into())
            .await;
        hub.command("drop", Value::Null).await;
        wait(&mut events, |e| matches!(e, SessionEvent::ConnectionStart)).await;
        timeout(LIMIT, async {
            while hub.command("snapshot", Value::Null).await["refreshPending"] != true {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        hub.command(
            "update",
            json!({"t":"new-message","message":message("440",440,"web")}),
        )
        .await;
        assert_eq!(timeout(LIMIT, users.recv()).await.unwrap().unwrap(), "440");
        hub.command("releaseRefresh", Value::Null).await;
        if abort {
            timeout(LIMIT, async {
                while hub.command("snapshot", Value::Null).await["pagePending"] != true {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        } else {
            wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
        }
        hub.command("drop", Value::Null).await;
        wait(&mut events, |e| matches!(e, SessionEvent::Disconnected)).await;
        wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
        if abort {
            hub.command("releasePage", Value::Null).await;
        }
        for seq in 2..440 {
            assert_eq!(users.try_recv().unwrap(), seq.to_string());
        }
        assert!(users.try_recv().is_err());
        let state = hub.command("snapshot", Value::Null).await;
        let queries: Vec<_> = state["requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["path"] == "/cli/sessions/s/messages")
            .map(|r| r["query"].as_str().unwrap())
            .collect();
        assert_eq!(
            queries,
            [
                "?afterSeq=1&limit=200",
                "?afterSeq=201&limit=200",
                "?afterSeq=201&limit=200",
                "?afterSeq=401&limit=200"
            ]
        );
        client.close();
    }
}

#[tokio::test]
async fn close_during_callback_discards_pending_delivery_and_late_registration() {
    let mut hub = Hub::new().await;
    let (client, mut events) = hub.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    for seq in 1..=2 {
        hub.command(
            "update",
            json!({"t":"new-message","message":message(&seq.to_string(),seq,"web")}),
        )
        .await;
    }
    hub.command("update", json!({"t":"barrier"})).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Message(_))).await;
    let (entered, mut seen) = mpsc::unbounded_channel();
    let (resume, blocked) = std::sync::mpsc::sync_channel(1);
    let blocked = std::sync::Mutex::new(blocked);
    let replay = client.clone();
    let task = tokio::task::spawn_blocking(move || {
        replay.on_user_message(Arc::new(move |_, id| {
            entered.send(id.unwrap()).unwrap();
            blocked.lock().unwrap().recv_timeout(LIMIT).unwrap();
        }))
    });
    assert_eq!(timeout(LIMIT, seen.recv()).await.unwrap().unwrap(), "1");
    hub.command(
        "update",
        json!({"t":"new-message","message":message("3",3,"web")}),
    )
    .await;
    hub.command("update", json!({"t":"barrier"})).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Message(_))).await;
    client.close();
    let callback: hapi_wrapper_core::session::client::UserHandler =
        Arc::new(|_, _| panic!("delivery after close"));
    client.on_user_message(callback.clone());
    assert_eq!(Arc::strong_count(&callback), 1);
    resume.send(()).unwrap();
    task.await.unwrap();
    assert!(seen.try_recv().is_err());
}

#[tokio::test]
async fn https_hub_and_plain_runner_webhook_share_rest_client() {
    if std::env::var_os("HAPI_TEST_HTTPS_HUB").is_none() {
        let ca = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-ca.pem");
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "https_hub_and_plain_runner_webhook_share_rest_client",
            ])
            .env("HAPI_TEST_HTTPS_HUB", "1")
            .env("NODE_EXTRA_CA_CERTS", ca)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        return;
    }

    let mut hub = Hub::new().await;
    let port = hub.command("startTls", Value::Null).await.as_u64().unwrap();
    hub.config.api_url = format!("https://localhost:{port}");
    let rest = Rest::new(&hub.config).await.unwrap();
    let session = rest.session("s").await.unwrap().session;
    assert_eq!(session["id"], "s");
    rest.report_started(&hub.config.home, "s", &session["metadata"])
        .await
        .unwrap();
    let snapshot = hub.command("snapshot", Value::Null).await;
    let requests = snapshot["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["path"], "/cli/sessions/s");
    assert_eq!(requests[1]["path"], "/session-started");
    assert_eq!(
        requests[1]["body"],
        json!({"sessionId":"s","metadata":session["metadata"]})
    );
}

#[tokio::test]
async fn mcp_tools_use_session_metadata_media_and_skill_registry() {
    use bytes::Bytes;
    use hapi_wrapper_core::mcp::tools;
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};
    let mut hub = Hub::new().await;
    let (client, mut events) = hub.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    let cwd = hub._dir.path();
    let skill = cwd.join(".agents/skills/sample");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: sample\ndescription: sample skill\n---\nUse this skill.",
    )
    .unwrap();
    let media = cwd.join("file.wav");
    std::fs::write(&media, b"RIFF1234WAVE").unwrap();
    let server = tools::start(
        client.clone(),
        tools::Options {
            skill_lookup: Some((cwd.into(), "claude".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let http = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let request = |sid: &str, method: &str, params: Value| {
        let mut req = Request::post(format!("http://127.0.0.1:{}/", server.port))
            .header("content-type", "application/json");
        if !sid.is_empty() {
            req = req.header("mcp-session-id", sid)
        }
        http.request(
            req.body(Full::new(Bytes::from(
                json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
            )))
            .unwrap(),
        )
    };
    let init = request("", "initialize", json!({"protocolVersion":"2025-11-25"}))
        .await
        .unwrap();
    let sid = init.headers()["mcp-session-id"].to_str().unwrap();
    let call = |name: &str, args: Value| {
        let res = request(sid, "tools/call", json!({"name":name,"arguments":args}));
        async move {
            let data = res.await.unwrap().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&data).unwrap()["result"].clone()
        }
    };
    assert!(client.flush_metadata(LIMIT).await);
    assert_eq!(
        call("change_title", json!({"title":" New   title "})).await["isError"],
        false
    );
    assert_eq!(client.metadata.get().value["name"], "New title");
    hub.command("metadata",json!({"metadataVersion":99,"metadata":{"host":"test","path":"/tmp","name":"Locked","titleLocked":true}})).await;
    let locked = call("change_title", json!({"title":"Wrong"})).await;
    assert_eq!(locked["isError"], true);
    assert!(locked["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("locked by the user"));
    assert_eq!(client.metadata.get().value["name"], "Locked");
    assert_eq!(
        call("skill_lookup", json!({"name":" sample "})).await["content"][0]["text"],
        "Skill: sample\nDescription: sample skill\n\nUse this skill."
    );
    assert_eq!(
        call("display_media", json!({"path":media,"title":"Clip"})).await["content"][0]["text"],
        "Displayed media: Clip.wav"
    );
    assert_eq!(
        call("display_image", json!({"path":media})).await["isError"],
        true
    );
    client.close();
}

#[tokio::test]
async fn terminal_routes_are_scoped_to_a_live_connection_and_shell() {
    async fn send(h: &mut Hub, event: &str, mut payload: Value) {
        payload["sessionId"] = "s".into();
        payload["terminalId"] = "same".into();
        h.command("terminal", json!({"event":event,"payload":payload}))
            .await;
    }
    async fn shell(h: &mut Hub, path: &Path) -> i32 {
        send(h, "terminal:open", json!({"cols":80,"rows":24})).await;
        send(h, "terminal:resize", json!({"cols":102,"rows":37})).await;
        let cmd = format!(
            "trap '' HUP TERM; stty size > '{0}.size'; echo $$ > '{0}'\n",
            path.display()
        );
        send(h, "terminal:write", json!({"data":cmd})).await;
        let pid = timeout(LIMIT, async {
            loop {
                if let Ok(s) = tokio::fs::read_to_string(path).await {
                    if let Ok(pid) = s.trim().parse() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(path.with_extension("size"))
                .unwrap()
                .trim(),
            "37 102"
        );
        pid
    }
    async fn gone(pid: i32) {
        timeout(LIMIT, async {
            while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
                != Err(nix::errno::Errno::ESRCH)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    let mut h = Hub::new().await;
    let (session, mut events) = h.start().await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    let dir = tempfile::tempdir().unwrap();
    let first = shell(&mut h, &dir.path().join("old")).await;
    h.command("drop", Value::Null).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Disconnected)).await;
    wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
    let second = shell(&mut h, &dir.path().join("second")).await;
    gone(first).await;
    send(&mut h, "terminal:close", json!({})).await;
    let third = shell(&mut h, &dir.path().join("third")).await;
    gone(second).await;
    let snapshot = h.command("snapshot", Value::Null).await;
    let wire = snapshot["emitted"].as_array().unwrap();
    assert_eq!(
        wire.iter()
            .filter(|e| e["event"] == "terminal:ready")
            .count(),
        3
    );
    assert!(wire.iter().any(|e| e["event"] == "terminal:output"));
    assert!(wire
        .iter()
        .all(|e| e["event"] != "terminal:exit" && e["event"] != "terminal:error"));
    session.close();
    gone(third).await;
}

#[tokio::test]
async fn sigterm_with_real_terminal_and_stalled_flush() {
    use hapi_wrapper_core::lifecycle::runner_lifecycle::{RunnerLifecycle, STOP_BUDGET};
    use nix::{
        errno::Errno,
        sys::signal::{kill, Signal},
        unistd::Pid,
    };
    const URL: &str = "HAPI_SIGTERM_TEST_HUB_URL";
    if let Ok(api_url) = std::env::var(URL) {
        let home = tempfile::tempdir().unwrap();
        let config = Config {
            home: home.path().into(),
            api_url,
            cli_api_token: "token".into(),
            extra_headers: vec![("x-fixture".into(), "extra".into())],
            machine_id: None,
        };
        let (session,mut events)=SessionClient::start(&config,Rest::new(&config).await.unwrap(),&json!({"id":"s","metadata":{"host":"test","path":"/tmp"},"metadataVersion":1,"agentState":{},"agentStateVersion":1})).unwrap();
        let lifecycle = RunnerLifecycle::new(session, Box::new(|_| Box::pin(async {})));
        lifecycle.register_process_handlers().unwrap();
        wait(&mut events, |e| matches!(e, SessionEvent::Connected)).await;
        let mut agent = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; echo ready; exec sleep 60"])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        assert_eq!(
            BufReader::new(agent.stdout.take().unwrap())
                .lines()
                .next_line()
                .await
                .unwrap()
                .as_deref(),
            Some("ready")
        );
        tokio::spawn(async move {
            agent.wait().await.unwrap();
        });
        println!("wrapper-ready");
        tokio::time::sleep(Duration::from_secs(12)).await;
        panic!("wrapper did not exit");
    }
    let mut hub = Hub::new().await;
    hub.command("ping", false.into()).await;
    let mut wrapper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "--nocapture",
            "sigterm_with_real_terminal_and_stalled_flush",
        ])
        .env(URL, &hub.config.api_url)
        .env("SHELL", "/bin/bash")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = Pid::from_raw(wrapper.id().unwrap() as i32);
    let mut lines = BufReader::new(wrapper.stdout.take().unwrap()).lines();
    timeout(LIMIT, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line == "wrapper-ready" {
                return;
            }
        }
        panic!("wrapper exited during startup")
    })
    .await
    .unwrap();
    let shell_file = hub._dir.path().join("shell.pid");
    hub.command("terminal",json!({"event":"terminal:open","payload":{"sessionId":"s","terminalId":"stop","rows":24,"cols":80}})).await;
    hub.command("terminal",json!({"event":"terminal:write","payload":{"sessionId":"s","terminalId":"stop","data":format!("trap '' HUP TERM; echo $$ > '{}'\n",shell_file.display())}})).await;
    let shell = timeout(LIMIT, async {
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(&shell_file).await {
                if let Ok(pid) = contents.trim().parse() {
                    break Pid::from_raw(pid);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let start = tokio::time::Instant::now();
    kill(pid, Signal::SIGTERM).unwrap();
    assert!(timeout(STOP_BUDGET, wrapper.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    assert!(start.elapsed() >= Duration::from_secs(1));
    assert_eq!(kill(shell, None), Err(Errno::ESRCH));
    assert_eq!(kill(Pid::from_raw(-pid.as_raw()), None), Err(Errno::ESRCH));
    let snapshot = hub.command("snapshot", Value::Null).await;
    assert!(snapshot["emitted"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["event"] == "session-end" && v["payload"]["reason"] == "terminated"));
}
