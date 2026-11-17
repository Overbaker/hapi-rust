use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dashmap::DashMap;
use flate2::write::GzEncoder;
use flate2::Compression;
use futures_util::stream;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use uuid::Uuid;

const CONTENT_FLAG_RAW: u8 = 0x00;
const CONTENT_FLAG_ZSTD: u8 = 0x01;
const ZSTD_THRESHOLD_BYTES: usize = 1024;
const SSE_REPLAY_LIMIT: usize = 4096;

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn encode_content_blob(content: &Value) -> rusqlite::Result<Vec<u8>> {
    let raw = serde_json::to_vec(content)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    if raw.len() >= ZSTD_THRESHOLD_BYTES {
        if let Ok(compressed) = zstd::stream::encode_all(raw.as_slice(), 3) {
            if compressed.len() < raw.len() {
                let mut out = Vec::with_capacity(1 + compressed.len());
                out.push(CONTENT_FLAG_ZSTD);
                out.extend_from_slice(&compressed);
                return Ok(out);
            }
        }
    }
    let mut out = Vec::with_capacity(1 + raw.len());
    out.push(CONTENT_FLAG_RAW);
    out.extend_from_slice(&raw);
    Ok(out)
}

fn decode_content_blob(blob: &[u8]) -> Value {
    if blob.is_empty() {
        return Value::Null;
    }
    match blob[0] {
        CONTENT_FLAG_ZSTD => {
            if let Ok(raw) = zstd::stream::decode_all(&blob[1..]) {
                serde_json::from_slice(&raw).unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        CONTENT_FLAG_RAW => serde_json::from_slice(&blob[1..]).unwrap_or(Value::Null),
        _ => serde_json::from_slice(blob).unwrap_or(Value::Null),
    }
}

#[derive(Debug, Clone)]
pub struct SseFrame {
    pub id: u64,
    pub event: &'static str,
    /// Pre-serialized JSON payload shared via `Bytes` across all live and replayed subscribers.
    pub data: Bytes,
}

pub struct SseBroker {
    next_id: AtomicU64,
    replay: Mutex<VecDeque<SseFrame>>,
    tx: broadcast::Sender<SseFrame>,
}

impl SseBroker {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self {
            next_id: AtomicU64::new(1),
            replay: Mutex::new(VecDeque::with_capacity(512)),
            tx,
        }
    }

    /// Serializes once into an immutable `Bytes` frame and shares the handle zero-copy across subscribers.
    pub fn publish(&self, event: &'static str, payload: &Value) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let serialized = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
        let frame = SseFrame {
            id,
            event,
            data: Bytes::from(serialized),
        };
        {
            let mut ring = self.replay.lock().expect("replay mutex");
            if ring.len() >= SSE_REPLAY_LIMIT {
                ring.pop_front();
            }
            ring.push_back(frame.clone());
        }
        let _ = self.tx.send(frame);
        id
    }

    pub fn replay_since(&self, last_event_id: Option<u64>) -> (Vec<SseFrame>, broadcast::Receiver<SseFrame>) {
        let rx = self.tx.subscribe();
        let frames = match last_event_id {
            Some(since) => {
                let ring = self.replay.lock().expect("replay mutex");
                ring.iter()
                    .filter(|f| f.id > since)
                    .cloned()
                    .collect()
            }
            None => Vec::new(),
        };
        (frames, rx)
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for SseBroker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
struct LiveReasoningState {
    row_id: String,
    text: String,
}

#[derive(Debug, Default)]
struct SessionTailIndex {
    next_seq: i64,
    live_tool_calls: HashMap<String, String>,
    live_reasoning: HashMap<String, LiveReasoningState>,
}

pub struct HubState {
    db: Mutex<Connection>,
    tails: DashMap<String, SessionTailIndex>,
    sse: SseBroker,
}

impl HubState {
    pub fn open(path: &str) -> rusqlite::Result<Arc<Self>> {
        let conn = if path == ":memory:" {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = MEMORY;
            PRAGMA mmap_size = 268435456;
            PRAGMA cache_size = -32768;
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                local_id TEXT,
                content BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                invoked_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session_seq
                ON messages(session_id, seq);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_session_local
                ON messages(session_id, local_id)
                WHERE local_id IS NOT NULL;
            ",
        )?;

        Ok(Arc::new(Self {
            db: Mutex::new(conn),
            tails: DashMap::new(),
            sse: SseBroker::new(),
        }))
    }

    pub fn subscriber_count(&self) -> usize {
        self.sse.subscriber_count()
    }

    fn ensure_tail_seq(&self, conn: &Connection, session_id: &str) -> rusqlite::Result<i64> {
        if let Some(mut entry) = self.tails.get_mut(session_id) {
            entry.next_seq += 1;
            return Ok(entry.next_seq);
        }
        let now = now_millis();
        conn.prepare_cached(
            "INSERT OR IGNORE INTO sessions (id, created_at, updated_at) VALUES (?1, ?2, ?2)",
        )?
        .execute(params![session_id, now])?;
        let current_max: i64 = conn
            .prepare_cached("SELECT COALESCE(MAX(seq), 0) FROM messages WHERE session_id = ?1")?
            .query_row(params![session_id], |r| r.get(0))?;
        let next = current_max + 1;
        self.tails.insert(
            session_id.to_string(),
            SessionTailIndex {
                next_seq: next,
                ..Default::default()
            },
        );
        Ok(next)
    }

    pub fn ingest_messages(&self, session_id: &str, items: Vec<IncomingMessage>) -> rusqlite::Result<Vec<StoredMessageDto>> {
        let mut results = Vec::with_capacity(items.len());
        let mut events_to_publish: Vec<(&'static str, Value)> = Vec::with_capacity(items.len());

        {
            let mut conn = self.db.lock().expect("db lock");
            let tx = conn.transaction()?;
            let now = now_millis();
            tx.prepare_cached(
                "INSERT OR IGNORE INTO sessions (id, created_at, updated_at) VALUES (?1, ?2, ?2)",
            )?
            .execute(params![session_id, now])?;

            for item in items {
                let created_at = item.created_at.unwrap_or(now);
                let content = item.content;
                let data = content.get("content").and_then(|c| c.get("data"));
                let msg_type = data.and_then(|d| d.get("type")).and_then(|v| v.as_str());

                // Fast O(1) tool-call upsert without table scan
                if msg_type == Some("tool-call") {
                    if let Some(call_id) = data.and_then(|d| d.get("callId")).and_then(|v| v.as_str()) {
                        let existing_id = self
                            .tails
                            .get(session_id)
                            .and_then(|t| t.live_tool_calls.get(call_id).cloned());
                        if let Some(row_id) = existing_id {
                            let blob = encode_content_blob(&content)?;
                            tx.prepare_cached("UPDATE messages SET content = ?1 WHERE id = ?2")?
                                .execute(params![blob, row_id])?;
                            let seq: i64 = tx
                                .prepare_cached("SELECT seq FROM messages WHERE id = ?1")?
                                .query_row(params![row_id], |r| r.get(0))?;
                            let dto = StoredMessageDto {
                                id: row_id,
                                session_id: session_id.to_string(),
                                seq,
                                local_id: item.local_id,
                                content: content.clone(),
                                created_at,
                                invoked_at: Some(created_at),
                            };
                            events_to_publish.push(("message", json!({ "sessionId": session_id, "message": dto })));
                            results.push(dto);
                            continue;
                        }
                    }
                }

                // Clear completed tool-call / turn boundaries in O(1)
                if msg_type == Some("tool-call-result") {
                    if let Some(call_id) = data.and_then(|d| d.get("callId")).and_then(|v| v.as_str()) {
                        if let Some(mut t) = self.tails.get_mut(session_id) {
                            t.live_tool_calls.remove(call_id);
                        }
                    }
                } else if content.get("role").and_then(|v| v.as_str()) == Some("user") {
                    if let Some(mut t) = self.tails.get_mut(session_id) {
                        t.live_tool_calls.clear();
                    }
                }

                // O(1) live reasoning replacement + compact delta emission
                let mut delta_event: Option<Value> = None;
                if msg_type == Some("reasoning") {
                    let is_live = data.and_then(|d| d.get("live")).and_then(|v| v.as_bool()) == Some(true);
                    let stream_id = data.and_then(|d| d.get("id")).and_then(|v| v.as_str());
                    let text = data.and_then(|d| d.get("message")).and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(sid) = stream_id {
                        let prev = self
                            .tails
                            .get(session_id)
                            .and_then(|t| t.live_reasoning.get(sid).map(|r| (r.row_id.clone(), r.text.clone())));
                        if let Some((prev_row_id, prev_text)) = prev {
                            tx.prepare_cached("DELETE FROM messages WHERE id = ?1")?
                                .execute(params![prev_row_id])?;
                            if is_live && !prev_text.is_empty() {
                                if let Some(suffix) = text.strip_prefix(&prev_text) {
                                    delta_event = Some(json!({
                                        "sessionId": session_id,
                                        "streamId": sid,
                                        "delta": suffix
                                    }));
                                }
                            }
                        }
                    }
                }

                let seq = self.ensure_tail_seq(&tx, session_id)?;
                let id = Uuid::new_v4().to_string();
                let blob = encode_content_blob(&content)?;
                let invoked_at = Some(created_at);
                tx.prepare_cached(
                    "INSERT INTO messages (id, session_id, seq, local_id, content, created_at, invoked_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?
                .execute(params![id, session_id, seq, item.local_id, blob, created_at, invoked_at])?;

                if let Some(mut tail) = self.tails.get_mut(session_id) {
                    if msg_type == Some("tool-call") {
                        if let Some(call_id) = data.and_then(|d| d.get("callId")).and_then(|v| v.as_str()) {
                            tail.live_tool_calls.insert(call_id.to_string(), id.clone());
                        }
                    } else if msg_type == Some("reasoning") {
                        if let Some(sid) = data.and_then(|d| d.get("id")).and_then(|v| v.as_str()) {
                            let is_live = data.and_then(|d| d.get("live")).and_then(|v| v.as_bool()) == Some(true);
                            if is_live {
                                let text = data.and_then(|d| d.get("message")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                                tail.live_reasoning.insert(sid.to_string(), LiveReasoningState { row_id: id.clone(), text });
                            } else {
                                tail.live_reasoning.remove(sid);
                            }
                        }
                    }
                }

                let dto = StoredMessageDto {
                    id,
                    session_id: session_id.to_string(),
                    seq,
                    local_id: item.local_id,
                    content,
                    created_at,
                    invoked_at,
                };
                if let Some(mut delta) = delta_event {
                    delta["id"] = Value::String(dto.id.clone());
                    delta["seq"] = json!(dto.seq);
                    events_to_publish.push(("delta", delta));
                } else {
                    events_to_publish.push(("message", json!({ "sessionId": session_id, "message": dto })));
                }
                results.push(dto);
            }

            tx.commit()?;
        }

        for (event_name, payload) in events_to_publish {
            self.sse.publish(event_name, &payload);
        }

        Ok(results)
    }

    pub fn list_messages(&self, session_id: &str, after_seq: i64, limit: i64) -> rusqlite::Result<Vec<StoredMessageDto>> {
        let conn = self.db.lock().expect("db lock");
        let mut stmt = conn.prepare_cached(
            "SELECT id, session_id, seq, local_id, content, created_at, invoked_at
             FROM messages
             WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_id, after_seq, limit], |row| {
            let blob: Vec<u8> = row.get(4)?;
            Ok(StoredMessageDto {
                id: row.get(0)?,
                session_id: row.get(1)?,
                seq: row.get(2)?,
                local_id: row.get(3)?,
                content: decode_content_blob(&blob),
                created_at: row.get(5)?,
                invoked_at: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn count_messages(&self, session_id: &str) -> rusqlite::Result<i64> {
        let conn = self.db.lock().expect("db lock");
        let count = conn
            .prepare_cached("SELECT COUNT(*) FROM messages WHERE session_id = ?1")?
            .query_row(params![session_id], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        Ok(count)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingMessage {
    pub content: Value,
    #[serde(default)]
    pub local_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum IngestRequest {
    Batch { messages: Vec<IncomingMessage> },
    Single(IncomingMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMessageDto {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    pub local_id: Option<String>,
    pub content: Value,
    pub created_at: i64,
    pub invoked_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagesQuery {
    #[serde(default)]
    pub after_seq: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsQuery {
    #[serde(default)]
    pub last_event_id: Option<u64>,
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').any(|part| part.trim().starts_with("gzip") && !part.contains("q=0")))
        .unwrap_or(false)
}

fn json_maybe_gzip(headers: &HeaderMap, payload: &Value) -> Response {
    let raw = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    if raw.len() >= 512 && accepts_gzip(headers) {
        let mut encoder = GzEncoder::new(Vec::with_capacity(raw.len() / 4), Compression::fast());
        if encoder.write_all(&raw).is_ok() {
            if let Ok(compressed) = encoder.finish() {
                let mut res = (StatusCode::OK, compressed).into_response();
                let h = res.headers_mut();
                h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
                h.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
                h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
                return res;
            }
        }
    }
    let mut res = (StatusCode::OK, raw).into_response();
    let h = res.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    res
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "engine": "hapi-hub-rs" }))
}

async fn post_messages(
    State(state): State<Arc<HubState>>,
    Path(session_id): Path<String>,
    Json(body): Json<IngestRequest>,
) -> Response {
    let items = match body {
        IngestRequest::Batch { messages } => messages,
        IngestRequest::Single(one) => vec![one],
    };
    match state.ingest_messages(&session_id, items) {
        Ok(stored) => Json(json!({ "ok": true, "messages": stored })).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": err.to_string() }))).into_response(),
    }
}

async fn get_messages(
    State(state): State<Arc<HubState>>,
    Path(session_id): Path<String>,
    Query(query): Query<MessagesQuery>,
    headers: HeaderMap,
) -> Response {
    let after_seq = query.after_seq.unwrap_or(0);
    let limit = query.limit.unwrap_or(200).clamp(1, 2000);
    match state.list_messages(&session_id, after_seq, limit) {
        Ok(messages) => {
            let next_after = messages.last().map(|m| m.seq);
            json_maybe_gzip(&headers, &json!({
                "messages": messages,
                "nextAfterSeq": next_after
            }))
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": err.to_string() }))).into_response(),
    }
}

async fn get_events(
    State(state): State<Arc<HubState>>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let header_cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let cursor = header_cursor.or(query.last_event_id);
    let (replay, rx) = state.sse.replay_since(cursor);

    let replay_stream = stream::iter(replay.into_iter().map(|frame| {
        let data_str = std::str::from_utf8(&frame.data).unwrap_or("{}");
        Ok::<Event, Infallible>(Event::default().id(frame.id.to_string()).event(frame.event).data(data_str))
    }));

    let live_stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let data_str = std::str::from_utf8(&frame.data).unwrap_or("{}");
                    let ev = Event::default().id(frame.id.to_string()).event(frame.event).data(data_str);
                    return Some((Ok::<Event, Infallible>(ev), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    let combined = futures_util::StreamExt::chain(replay_stream, live_stream);
    Sse::new(combined).keep_alive(KeepAlive::new().interval(Duration::from_secs(25)))
}

pub fn build_router(state: Arc<HubState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/cli/sessions/{id}/messages", post(post_messages).get(get_messages))
        .route("/api/sessions/{id}/messages", get(get_messages))
        .route("/api/events", get(get_events))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn collapses_live_reasoning_and_upserts_tool_calls_with_one_row_each() {
        let state = HubState::open(":memory:").unwrap();

        let filler: Vec<IncomingMessage> = (0..500)
            .map(|i| IncomingMessage {
                content: json!({ "role": "agent", "content": { "type": "codex", "data": { "type": "message", "message": format!("hist-{i}") } } }),
                local_id: None,
                created_at: Some(i),
            })
            .collect();
        state.ingest_messages("s1", filler).unwrap();

        for i in 0..25 {
            state
                .ingest_messages(
                    "s1",
                    vec![IncomingMessage {
                        content: json!({ "role": "agent", "content": { "type": "codex", "data": { "type": "tool-call", "callId": "call-1", "status": if i == 24 { "completed" } else { "in_progress" } } } }),
                        local_id: None,
                        created_at: Some(1000 + i),
                    }],
                )
                .unwrap();
        }

        let mut text = String::new();
        for i in 0..50 {
            text.push_str(&format!("chunk-{i} "));
            state
                .ingest_messages(
                    "s1",
                    vec![IncomingMessage {
                        content: json!({ "role": "agent", "content": { "type": "codex", "data": { "type": "reasoning", "id": "stream-1", "live": i < 49, "message": text } } }),
                        local_id: None,
                        created_at: Some(2000 + i),
                    }],
                )
                .unwrap();
        }

        // 500 history + 1 upserted tool call + 1 settled reasoning row
        assert_eq!(state.count_messages("s1").unwrap(), 502);
        let (frames, _) = state.sse.replay_since(Some(500));
        assert!(frames.iter().any(|f| f.event == "delta"));
    }

    #[tokio::test]
    async fn sse_broker_fans_out_zero_copy_frames_to_multiple_subscribers() {
        let state = HubState::open(":memory:").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (_, mut rx1) = state.sse.replay_since(None);
        let (_, mut rx2) = state.sse.replay_since(None);

        let url = format!("http://{addr}/health");
        let stream_resp = tokio::net::TcpStream::connect(addr).await.unwrap();
        drop(stream_resp);
        let _ = url;

        state
            .ingest_messages(
                "s-live",
                vec![IncomingMessage {
                    content: json!({ "role": "user", "content": { "type": "text", "text": "ping" } }),
                    local_id: Some("l1".into()),
                    created_at: Some(1),
                }],
            )
            .unwrap();

        let f1 = rx1.recv().await.unwrap();
        let f2 = rx2.recv().await.unwrap();
        assert_eq!(f1.data.as_ptr(), f2.data.as_ptr());

        server.abort();
    }
}
