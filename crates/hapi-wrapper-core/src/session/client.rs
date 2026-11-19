use super::{
    metadata::{now, spread},
    versioned_ack::{Document, Kind, Snapshot},
};
use crate::{
    config::Config,
    rest::Rest,
    sio::client::{Client as Socket, Connection, Event, ServerAck},
};
use futures_util::{future::BoxFuture, FutureExt};
use serde_json::{json, Value};
use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};

#[derive(PartialEq, Eq)]
pub(super) enum Delivery {
    Live,
    Retry,
    Backfill,
}

pub enum SessionEvent {
    ConnectionStart,
    Connected,
    Disconnected,
    ConnectError { message: String, fatal: bool },
    HubArchived,
    Message(Value),
    Socket(String, Vec<Value>, Option<ServerAck>),
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cancellation {
    Missing,
    Removed,
    InFlight,
    Indeterminate,
    Consumed,
}
pub type CancelHandler =
    Arc<dyn Fn(String) -> BoxFuture<'static, io::Result<Cancellation>> + Send + Sync>;
pub type RetryHandler = Arc<dyn Fn(String) -> BoxFuture<'static, io::Result<bool>> + Send + Sync>;
pub type UserHandler = Arc<dyn Fn(Value, Option<String>) + Send + Sync>;
#[derive(Default)]
struct Incoming {
    delivering: bool,
    pending: std::collections::VecDeque<(Value, Option<String>)>,
    user: Option<UserHandler>,
    cancel: Option<CancelHandler>,
    retry: Option<RetryHandler>,
}
pub struct SessionClient {
    pub id: String,
    pub rpc: crate::rpc::manager::RpcHandlerManager,
    pub generated_images: crate::rpc::handlers::GeneratedImages,
    pub metadata: Document,
    pub agent_state: Document,
    pub(crate) socket: Socket,
    pub(crate) rest: Rest,
    events: mpsc::UnboundedSender<SessionEvent>,
    archive: watch::Sender<bool>,
    heartbeat_enabled: watch::Sender<bool>,
    closed: watch::Sender<bool>,
    pub(super) filter: Mutex<super::incoming_filter::IncomingFilter>,
    pub(super) backfill_lock: tokio::sync::Mutex<()>,
    pub echoes: Mutex<super::echo::Echoes>,
    pub(super) latest: Mutex<Value>,
    incoming: Mutex<Incoming>,
    terminals: Mutex<crate::terminal::TerminalManager>,
}
impl SessionClient {
    pub fn start(
        config: &Config,
        rest: Rest,
        session: &Value,
    ) -> io::Result<(Arc<Self>, mpsc::UnboundedReceiver<SessionEvent>)> {
        Self::start_with_workspace(config, rest, session, None)
    }

    pub(super) fn start_with_workspace(
        config: &Config,
        rest: Rest,
        session: &Value,
        workspace: Option<&std::path::Path>,
    ) -> io::Result<(Arc<Self>, mpsc::UnboundedReceiver<SessionEvent>)> {
        let id = session["id"]
            .as_str()
            .ok_or_else(|| io::Error::other("Missing session id"))?
            .to_string();
        for (kind, key, version) in [
            (Kind::Metadata, "metadata", "metadataVersion"),
            (Kind::AgentState, "agentState", "agentStateVersion"),
        ] {
            if !session[version].is_u64() || (!session[key].is_null() && !kind.valid(&session[key]))
            {
                return Err(io::Error::other(format!("Invalid session {key}")));
            }
        }
        let (socket, mut rx) = Socket::start_session(
            config.api_url.clone(),
            config.extra_headers.clone(),
            config.cli_api_token.clone(),
            id.clone(),
        );
        let (events, output) = mpsc::unbounded_channel();
        let (archive, archived) = watch::channel(false);
        let (heartbeat_enabled, mut heartbeat_control) = watch::channel(true);
        let (closed, mut closing) = watch::channel(false);
        let latch = {
            let heartbeat_enabled = heartbeat_enabled.clone();
            let archive = archive.clone();
            let socket = socket.clone();
            let events = events.clone();
            Arc::new(move || {
                heartbeat_enabled.send_replace(false);
                if !archive.send_replace(true) {
                    socket.disable_reconnect();
                    let _ = events.send(SessionEvent::HubArchived);
                }
            })
        };
        let doc = |kind, key: &str, version: &str| {
            Document::new(
                kind,
                Snapshot {
                    value: session[key].clone(),
                    version: session[version].as_u64().unwrap_or(0),
                },
                id.clone(),
                socket.clone(),
                latch.clone(),
                archived.clone(),
            )
        };
        let rpc = crate::rpc::manager::RpcHandlerManager::new(&id);
        let generated_images = workspace
            .map(|p| crate::rpc::handlers::register_common_handlers(&rpc, p))
            .unwrap_or_default();
        let this = Arc::new(Self {
            rpc,
            generated_images,
            metadata: doc(Kind::Metadata, "metadata", "metadataVersion"),
            agent_state: doc(Kind::AgentState, "agentState", "agentStateVersion"),
            terminals: Mutex::new(crate::terminal::TerminalManager::new(&id, |_, _| {})),
            id,
            rest,
            socket,
            events,
            archive,
            heartbeat_enabled,
            closed,
            filter: Mutex::default(),
            backfill_lock: tokio::sync::Mutex::default(),
            echoes: Mutex::default(),
            latest: Mutex::new(json!({"thinking":false})),
            incoming: Mutex::default(),
        });
        let weak = Arc::downgrade(&this);
        tokio::spawn(async move {
            let mut generation = 0;
            let period = Duration::from_secs(10);
            let mut heartbeat =
                tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut heartbeat = Some(heartbeat);
            loop {
                let event = tokio::select! {
                    biased;
                    _=closing.changed()=>break,
                    _=heartbeat_control.changed(),if heartbeat.is_some()=>{heartbeat=None;continue},
                    e=rx.recv()=>match e{Some(e)=>e,None=>break},
                    _=async{if let Some(timer)=&mut heartbeat{timer.tick().await;}},if heartbeat.is_some()=>{if let Some(c)=weak.upgrade(){c.alive(true);}else{break} continue}
                };
                let Some(client) = weak.upgrade() else { break };
                if client.is_closed() {
                    break;
                }
                match event {
                    Event::Connected => {
                        generation += 1;
                        let current = Connection::Online(generation);
                        let mut link = client.socket.connection();
                        if *link.borrow() != current {
                            continue;
                        }
                        let _ = client.events.send(SessionEvent::ConnectionStart);
                        client
                            .terminals
                            .lock()
                            .unwrap()
                            .connected(client.socket.clone(), generation);
                        client.rpc.on_connect(&client.socket);
                        client.alive(false);
                        if generation > 1 {
                            let start_cursor = client.filter.lock().unwrap().start_backfill();
                            tokio::spawn(async move {
                                let mut closed = client.closed.subscribe();
                                tokio::select! {biased; _=link.wait_for(|v|*v!=current)=>return,_=closed.wait_for(|v|*v)=>return,_=client.refresh_metadata()=>{}}
                                if !client.hub_archived() && !client.is_closed() {
                                    let result = tokio::select! {biased; _=link.wait_for(|v|*v!=current)=>return,_=closed.wait_for(|v|*v)=>return,result=client.backfill(start_cursor)=>result};
                                    let active = link.borrow();
                                    if *active == current && result.is_ok() {
                                        client.filter.lock().unwrap().finish_backfill();
                                    }
                                }
                                let active = link.borrow();
                                if *active == current
                                    && !client.hub_archived()
                                    && !client.is_closed()
                                {
                                    let _ = client.events.send(SessionEvent::Connected);
                                }
                            });
                        } else {
                            let active = link.borrow();
                            if *active == current && !client.hub_archived() && !client.is_closed() {
                                let _ = client.events.send(SessionEvent::Connected);
                            }
                        }
                    }
                    Event::ConnectError { message, fatal } => {
                        client.rpc.on_disconnect();
                        let _ = client
                            .events
                            .send(SessionEvent::ConnectError { message, fatal });
                    }
                    Event::Disconnected(_) => {
                        client.terminals.lock().unwrap().disconnected();
                        client.filter.lock().unwrap().start_backfill();
                        client.rpc.on_disconnect();

                        let _ = client.events.send(SessionEvent::Disconnected);
                    }
                    Event::Message { name, args, ack } if name == "rpc-request" => {
                        tokio::spawn(async move {
                            let result = client
                                .rpc
                                .handle_request(args.first().unwrap_or(&Value::Null))
                                .await;
                            if let Some(ack) = ack {
                                ack.send(vec![json!(result)]);
                            }
                        });
                    }
                    Event::Message { name, args, ack } if name == "update" => {
                        if let Some(body) = args.first().and_then(|v| v.get("body")) {
                            client.update(body, ack);
                        }
                    }
                    Event::Message { name, args, .. }
                        if matches!(
                            name.as_str(),
                            "terminal:open"
                                | "terminal:write"
                                | "terminal:resize"
                                | "terminal:close"
                        ) =>
                    {
                        let meta = client.metadata.get();
                        let path = meta.value["path"].as_str().map(std::path::Path::new);
                        client.terminals.lock().unwrap().handle(
                            &name,
                            args.first().unwrap_or(&Value::Null),
                            path,
                        );
                    }
                    Event::Message { name, args, ack } => {
                        let _ = client.events.send(SessionEvent::Socket(name, args, ack));
                    }
                }
            }
        });
        Ok((this, output))
    }
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }
    pub fn hub_archived(&self) -> bool {
        *self.archive.borrow()
    }
    pub fn closed_notifications(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }
    pub fn archive_notifications(&self) -> watch::Receiver<bool> {
        self.archive.subscribe()
    }
    pub async fn refresh_metadata(&self) -> bool {
        let Ok(row) = self.rest.refresh(&self.id).await else {
            return false;
        };
        let Some(version) = row["metadataVersion"].as_u64() else {
            return false;
        };
        let Some(meta) = row.get("metadata") else {
            return false;
        };
        if !meta.is_null() && !Kind::Metadata.valid(meta) {
            return false;
        }
        self.metadata.remote(version, meta);
        true
    }
    pub fn stop_keep_alive(&self) {
        self.heartbeat_enabled.send_replace(false);
    }
    pub(super) fn alive(&self, volatile: bool) {
        let enabled = self.heartbeat_enabled.borrow();
        if !*enabled {
            return;
        }
        let mut body = json!({"sid":self.id,"time":now()});
        spread(&mut body, &self.latest.lock().unwrap());
        if volatile {
            self.socket.volatile_emit("session-alive", vec![body]);
        } else {
            self.socket.emit("session-alive", vec![body]);
        }
    }
    pub async fn flush_metadata(&self, timeout: Duration) -> bool {
        !self.is_closed()
            && !timeout.is_zero()
            && tokio::time::timeout(timeout, self.metadata.drain())
                .await
                .unwrap_or(false)
    }
    pub async fn flush(&self, timeout: Duration) -> bool {
        if self.is_closed() || timeout.is_zero() {
            return false;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut closing = self.closed.subscribe();
        let run = async {
            let mut link = self.socket.connection();
            if *link.borrow() == Connection::Offline {
                self.socket.request_connect();
            }
            if !link
                .wait_for(|s| *s != Connection::Offline)
                .await
                .is_ok_and(|s| matches!(*s, Connection::Online(_)))
            {
                return false;
            }
            if !self.metadata.drain().await || !self.agent_state.drain().await {
                return false;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            !left.is_zero()
                && self
                    .socket
                    .emit_with_ack("ping", vec![], Some(left))
                    .await
                    .is_ok()
        };
        tokio::select! {
            _=closing.wait_for(|v|*v)=>false,
            done=tokio::time::timeout_at(deadline,run)=>done.unwrap_or(false),
        }
    }
    pub(crate) fn stop_terminals(&self) -> watch::Sender<bool> {
        let mut m = self.terminals.lock().unwrap();
        m.shutdown.send_replace(true);
        m.disconnected();
        m.shutdown.clone()
    }
    pub fn close(&self) {
        self.close_transport();
        self.stop_terminals();
    }
    pub(crate) fn close_transport(&self) {
        if self.closed.send_replace(true) {
            return;
        }
        self.rpc.on_disconnect();
        self.metadata.close();
        self.agent_state.close();
        *self.incoming.lock().unwrap() = Incoming::default();
        *self.echoes.lock().unwrap() = Default::default();
        *self.latest.lock().unwrap() = Value::Null;
        self.stop_keep_alive();
        self.socket.stop(Duration::from_millis(500));
    }
    pub fn on_user_message(&self, f: UserHandler) {
        let mut incoming = self.incoming.lock().unwrap();
        if self.is_closed() {
            return;
        }
        incoming.user = Some(f);
        self.deliver(incoming);
    }
    fn deliver<'a>(&'a self, mut state: std::sync::MutexGuard<'a, Incoming>) {
        if state.delivering || state.user.is_none() {
            return;
        }
        state.delivering = true;
        while !self.is_closed() {
            let Some(callback) = state.user.clone() else {
                break;
            };
            let Some((value, id)) = state.pending.pop_front() else {
                break;
            };
            drop(state);
            callback(value, id);
            state = self.incoming.lock().unwrap();
        }
        state.delivering = false;
    }
    pub fn on_cancel(&self, f: CancelHandler) {
        self.incoming.lock().unwrap().cancel = Some(f);
    }
    pub fn on_retry(&self, f: RetryHandler) {
        self.incoming.lock().unwrap().retry = Some(f);
    }
    async fn cancel(&self, id: &str) -> io::Result<Cancellation> {
        let callback = {
            let mut state = self.incoming.lock().unwrap();
            let count = state.pending.len();
            state
                .pending
                .retain(|(_, local)| local.as_deref() != Some(id));
            if state.pending.len() != count {
                return Ok(Cancellation::Removed);
            }
            state.cancel.clone()
        };
        match callback {
            Some(f) => f(id.into()).await,
            None => Ok(Cancellation::Missing),
        }
    }
    pub(super) fn incoming_message(&self, row: &Value, source: Delivery) {
        if self.is_closed() {
            return;
        }
        if !self.filter.lock().unwrap().accept_delivery(
            row["id"].as_str(),
            row["seq"].as_i64(),
            source == Delivery::Backfill,
        ) && source != Delivery::Retry
        {
            return;
        }
        let Some(content) = super::incoming_filter::user_message(&row["content"]) else {
            let _ = self
                .events
                .send(SessionEvent::Message(row["content"].clone()));
            return;
        };
        let id = row["localId"].as_str().map(str::to_owned);
        if content["meta"]["sentFrom"] == "cli"
            && !(content["meta"]["isNativeQueuedMessage"] == true
                && self.metadata.get().value["capabilities"]["concurrentClients"] == true
                && id.as_ref().is_some_and(|s| !s.is_empty()))
        {
            return;
        }
        let mut state = self.incoming.lock().unwrap();
        if self.is_closed() {
            return;
        }
        state.pending.push_back((content, id));
        self.deliver(state);
    }
    async fn queue_update(&self, body: &Value, retry: bool) -> io::Result<Value> {
        let id = body["localId"].as_str().filter(|s| !s.is_empty());
        let result = match id {
            Some(id) => self.cancel(id).await?,
            None => Cancellation::Missing,
        };
        if !retry {
            return Ok(
                json!({"removed":result==Cancellation::Removed,"inFlight":result==Cancellation::InFlight,"indeterminate":result==Cancellation::Indeterminate,"consumed":result==Cancellation::Consumed}),
            );
        }
        if result == Cancellation::Consumed {
            return Ok(json!({"removed":false,"accepted":false,"consumed":true}));
        }
        let accepted = match result {
            Cancellation::Indeterminate => {
                let callback = self.incoming.lock().unwrap().retry.clone();
                match (callback, id) {
                    (Some(f), Some(id)) => f(id.into()).await?,
                    _ => false,
                }
            }
            Cancellation::InFlight => false,
            _ => true,
        };
        if accepted {
            self.incoming_message(&body["message"], Delivery::Retry);
        }
        Ok(json!({"removed":false,"accepted":accepted}))
    }
    fn update(self: &Arc<Self>, body: &Value, ack: Option<ServerAck>) {
        match body["t"].as_str() {
            Some("new-message") => self.incoming_message(&body["message"], Delivery::Live),
            Some("update-session") => {
                for (key, doc) in [
                    ("metadata", &self.metadata),
                    ("agentState", &self.agent_state),
                ] {
                    if let Some(v) = body[key]["version"].as_u64() {
                        doc.remote(v, &body[key]["value"]);
                    }
                }
            }
            Some(kind @ ("cancel-queued-message" | "retry-queued-message")) => {
                let retry = kind == "retry-queued-message";
                let client = self.clone();
                let body = body.clone();
                let mut response = Box::pin(async move {
                    let result = client.queue_update(&body, retry).await.unwrap_or_else(
                        |_| json!({"removed":false,"accepted":false,"indeterminate":true}),
                    );
                    if let Some(ack) = ack {
                        ack.send(vec![result]);
                    }
                });
                if response.as_mut().now_or_never().is_none() {
                    tokio::spawn(response);
                }
            }
            _ => {
                let _ = self.events.send(SessionEvent::Message(body.clone()));
            }
        }
    }
}
impl Drop for SessionClient {
    fn drop(&mut self) {
        self.socket.stop(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    #[tokio::test]
    async fn first_connection_respects_latched_archive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = Config {
            api_url: format!("http://{}", listener.local_addr().unwrap()),
            cli_api_token: "token".into(),
            home: Default::default(),
            extra_headers: vec![],
            machine_id: None,
        };
        let rest = Rest::new(&config).await.unwrap();
        let (client, mut events) = SessionClient::start(
            &config,
            rest,
            &json!({"id":"s","metadataVersion":0,"agentStateVersion":0}),
        )
        .unwrap();
        client.archive.send_replace(true);

        tokio::time::timeout(Duration::from_secs(5), async {
            let mut ws = accept_async(listener.accept().await.unwrap().0).await.unwrap();
            ws.send(Message::text(r#"0{"sid":"e","pingInterval":60000,"pingTimeout":30000}"#)).await.unwrap();
            assert!(ws.next().await.unwrap().unwrap().into_text().unwrap().starts_with("40/cli,"));
            ws.send(Message::text(r#"40/cli,{"sid":"s"}"#)).await.unwrap();
            ws.send(Message::text(r#"42/cli,["update",{"body":{"t":"barrier"}}]"#)).await.unwrap();
            assert!(matches!(events.recv().await,Some(SessionEvent::ConnectionStart)));
            assert!(matches!(events.recv().await,Some(SessionEvent::Message(body)) if body["t"]=="barrier"));
        }).await.unwrap();
        client.close();
    }
}
