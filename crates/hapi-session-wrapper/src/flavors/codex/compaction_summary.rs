use super::value::text;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
#[derive(Default)]
struct Scan {
    file: Option<(String, u64, u64)>,
    offset: u64,
    mtime: (i64, i64),
    partial: Vec<u8>,
    verified: Option<bool>,
    turn: Option<String>,
    pending: Option<Checkpoint>,
    records: HashMap<(String, String), Option<Value>>,
}
struct Checkpoint {
    turn: String,
    info: Value,
    ambiguous: bool,
    context: bool,
    allowed: bool,
}
impl Checkpoint {
    fn invalidate(&mut self) {
        self.allowed = false;
        self.info
            .as_object_mut()
            .unwrap()
            .remove("estimatedTokensAfter");
    }
}
#[derive(Default)]
pub struct Lookup {
    scan: Scan,
    thread: String,
}
impl Lookup {
    pub fn new(thread: String) -> Self {
        Self {
            thread,
            ..Default::default()
        }
    }
    fn line(&mut self, bytes: &[u8]) {
        let state = &mut self.scan;
        let Ok(event) = serde_json::from_slice::<Value>(bytes) else {
            state.verified.get_or_insert(false);
            state.pending = None;
            return;
        };
        let p = &event["payload"];
        if state.verified.is_none() {
            let ids: Vec<_> = ["id", "session_id"]
                .iter()
                .filter_map(|k| text(p, k))
                .collect();
            state.verified = Some(
                event["type"] == "session_meta"
                    && !ids.is_empty()
                    && ids.iter().all(|v| *v == self.thread),
            );
            return;
        }
        if state.verified != Some(true) {
            return;
        }
        if event["type"] == "compacted" {
            let old = state.pending.take();
            let Some(turn) = text(&p["resume_metadata"], "last_started_turn_id")
                .filter(|t| Some(*t) == state.turn.as_deref())
            else {
                return;
            };
            let mut info = json!({});
            if let Some(s) = text(p, "message").filter(|s| !s.trim().is_empty()) {
                info["summary"] = json!(s);
            }
            state.pending = Some(Checkpoint {
                turn: turn.into(),
                info,
                ambiguous: old.is_some(),
                context: false,
                allowed: true,
            });
            return;
        }
        if let Some(c) = &mut state.pending {
            match text(&event, "type") {
                Some("turn_context") => {
                    c.context = p["turn_id"] == c.turn;
                    if !c.context {
                        c.invalidate();
                    }
                }
                Some("response_item" | "token_usage_record") => c.invalidate(),
                _ => {}
            }
        }
        if event["type"] != "event_msg" {
            return;
        }
        match text(p, "type") {
            Some("task_started") => {
                state.turn = text(p, "turn_id").map(str::to_owned);
                state.pending = None;
            }
            Some("task_complete" | "turn_aborted" | "task_failed") => state.pending = None,
            Some("token_count") => {
                if let Some(c) = state.pending.as_mut().filter(|_| !p["info"].is_null()) {
                    let info = &p["info"];
                    let last = &info["last_token_usage"];
                    let n = last["total_tokens"]
                        .as_u64()
                        .filter(|n| *n <= 9_007_199_254_740_991);
                    if c.context
                        && c.allowed
                        && info["last_usage_includes_reasoning"] == true
                        && [
                            "input_tokens",
                            "cached_input_tokens",
                            "cache_write_input_tokens",
                            "output_tokens",
                            "reasoning_output_tokens",
                        ]
                        .iter()
                        .all(|k| last[k] == 0)
                        && c.info.get("estimatedTokensAfter").is_none()
                        && n.is_some()
                    {
                        c.info["estimatedTokensAfter"] = json!(n);
                    } else {
                        c.invalidate();
                    }
                }
            }
            Some("item_completed") => {
                let Some(c) = state.pending.take() else {
                    return;
                };
                let item = &p["item"];
                if item["type"] == "ContextCompaction"
                    && p["thread_id"] == self.thread
                    && p["turn_id"] == c.turn
                {
                    if let Some(id) = text(item, "id") {
                        state
                            .records
                            .entry((c.turn, id.into()))
                            .and_modify(|v| *v = None)
                            .or_insert_with(|| {
                                (!c.ambiguous && !c.info.as_object().unwrap().is_empty())
                                    .then_some(c.info)
                            });
                    }
                }
            }
            _ => {}
        }
    }
    fn read(&mut self, path: &str) -> io::Result<bool> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() > 256 << 20 {
            self.scan = Scan::default();
            return Ok(false);
        }
        let identity = Some((path.into(), meta.dev(), meta.ino()));
        let mtime = (meta.mtime(), meta.mtime_nsec());
        if identity != self.scan.file
            || meta.len() < self.scan.offset
            || meta.len() == self.scan.offset && mtime != self.scan.mtime
        {
            self.scan = Scan::default();
        }
        self.scan.file = identity;
        self.scan.mtime = mtime;
        file.seek(SeekFrom::Start(self.scan.offset))?;
        let mut reader = file.take(meta.len() - self.scan.offset);
        let mut buf = [0u8; 65536];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            self.scan.offset += n as u64;
            for part in buf[..n].split_inclusive(|b| *b == b'\n') {
                self.scan.partial.extend_from_slice(part);
                if part.ends_with(b"\n") {
                    let line = std::mem::take(&mut self.scan.partial);
                    self.line(&line);
                }
            }
        }
        Ok(self.scan.verified != Some(false))
    }
    pub(super) fn probe(
        &mut self,
        thread: &Value,
        turn: &str,
        id: &str,
    ) -> Result<Option<Value>, ()> {
        let Some(path) = text(thread, "path").filter(|p| {
            Path::new(p).is_absolute()
                && thread["id"] == self.thread
                && !turn.is_empty()
                && !id.is_empty()
        }) else {
            return Ok(None);
        };
        match self.read(path) {
            Ok(true) => match self.scan.records.get(&(turn.into(), id.into())) {
                Some(v) => Ok(v.clone()),
                None => Err(()),
            },
            Ok(false) => Ok(None),
            Err(_) => {
                self.scan = Scan::default();
                Ok(None)
            }
        }
    }
    pub async fn find(
        &mut self,
        thread: &Value,
        turn: &str,
        id: &str,
        attempts: usize,
    ) -> Option<Value> {
        for attempt in 0..attempts {
            if let Ok(result) = self.probe(thread, turn, id) {
                return result;
            }
            if attempt + 1 < attempts {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        None
    }
}
