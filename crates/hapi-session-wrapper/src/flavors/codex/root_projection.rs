use super::*;
use crate::codex::permissions;
use std::{io::Read, os::unix::fs::OpenOptionsExt};
pub(super) struct QueuePort(pub Weak<Root>);
impl queue::Host for QueuePort {
    fn rpc<'a>(&'a self, method: &'a str, p: Value) -> BoxFuture<'a, queue::Result<Value>> {
        async move {
            self.0
                .upgrade()
                .ok_or_else(|| Error::Rejected("Codex execution is stopping".into()))?
                .request(method, p)
                .await
        }
        .boxed()
    }
    fn consumed(&self, ids: &[String], steered: bool) {
        if let Some(r) = self.0.upgrade() {
            r.session.messages_consumed(ids, false, steered);
            let ids = ids.to_vec();
            tokio::spawn(async move {
                {
                    let mut s = r.state.lock().unwrap();
                    for id in ids {
                        s.dequeued.remove(&id);
                    }
                }
                r.publish();
            });
        }
    }
    fn uncertain(&self, ids: &[String]) {
        if let Some(r) = self.0.upgrade() {
            r.session.steer_indeterminate(ids);
        }
    }
    fn mirror(&self, id: &str, input: Option<&queue::Input>) {
        if let Some(r) = self.0.upgrade() {
            let text = input.map(|v| projection::input_text(&json!(v)));
            r.session.native_queue_message(id, text.as_deref());
        }
    }
    fn requeued(&self, ids: Vec<String>) -> BoxFuture<'_, queue::Result<()>> {
        async move {
            if let Some(r) = self.0.upgrade() {
                r.session.steer_state(&ids, "queued").await;
            }
            Ok(())
        }
        .boxed()
    }
}
#[derive(Default)]
pub(super) struct Clocks {
    pub generation: GenerationLedger,
    duration: RoundDuration,
    waits: HashMap<String, u64>,
}
pub(super) struct Controls<'a>(pub &'a Root);
impl permissions::Host for Controls<'_> {
    fn root_thread(&self) -> &str {
        self.0.thread_id()
    }
    fn send(&mut self, id: &str, m: Value) {
        self.0.session.send_agent_message(m, Some(id));
    }
    fn state(&mut self, f: Box<dyn Fn(&Value) -> Value + Send + Sync>) {
        self.0.session.agent_state.update(f);
    }
    fn wait_started(&mut self, t: &str, key: &str) {
        if t == self.root_thread() {
            let mut c = self.0.clocks.lock().unwrap();
            let id = c.generation.begin_wait(now() as i64);
            c.waits.insert(key.into(), id);
        }
    }
    fn wait_ended(&mut self, _: &str, key: &str) {
        let mut c = self.0.clocks.lock().unwrap();
        if let Some(id) = c.waits.remove(key) {
            c.generation.end_wait(id, now() as i64);
        }
    }
}
fn mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if b.starts_with(&[255, 216, 255]) {
        return Some("image/jpeg");
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if b.get(4..8) == Some(b"ftyp") {
        let brand = std::str::from_utf8(b.get(8..12)?).ok()?;
        if b.starts_with(&[0, 0, 0]) && matches!(brand, "avif" | "avis") {
            return Some("image/avif");
        }
        return "isom iso2 iso3 iso4 iso5 iso6 mp41 mp42 mp71 avc1 avc3 hev1 hvc1 mmp4 dash msnv ndas ndsc ndsh ndsm ndsp ndss ndxc ndxh ndxm ndxp ndxs".split_whitespace().any(|s| s == brand).then_some("video/mp4");
    }
    if b.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        let prefix = &b[..b.len().min(128)];
        for n in 4..prefix.len().saturating_sub(3) {
            if prefix[n..].starts_with(&[0x42, 0x82]) {
                let len = prefix[n + 2];
                if len > 128 {
                    if let Some(t) = prefix.get(n + 3..n + 3 + (len & 127) as usize) {
                        return (t == b"webm").then_some("video/webm");
                    }
                }
            }
        }
    }
    None
}
struct Sink<'a> {
    root: &'a Root,
    compact: HashMap<(String, String), Value>,
}
impl projection::Host for Sink<'_> {
    fn current_metadata(&self) -> Value {
        self.root.session.metadata.get().value
    }
    fn agent(&mut self, id: &str, body: Value) {
        self.root.session.send_agent_message(body, Some(id));
    }
    fn user(&mut self, id: &str, s: &str) {
        self.root
            .session
            .send_user_message(s, &Value::Null, Some(id));
    }
    fn status(&mut self, body: Value) {
        self.root.session.send_session_event(body, None, None);
    }
    fn metadata(&mut self, update: Box<dyn Fn(&Value) -> Value + Send + Sync>) {
        self.root.session.metadata.update(update);
    }
    fn committed(&mut self, id: &str, c: &Value) -> Result<Option<Vec<Value>>> {
        Ok(self
            .root
            .q()?
            .committed(id, c)
            .map_err(|e| e.to_string())?
            .map(|v| v.iter().map(|m| json!(m)).collect()))
    }
    fn observed(&mut self, t: &str, item: &Value) {
        self.root
            .questions
            .lock()
            .unwrap()
            .observe(&mut Controls(self.root), t, item, now());
    }
    fn image(&mut self, path: &str, id: &str, name: Option<&str>) -> Option<Value> {
        image(&self.root.session.generated_images, path, id, name)
    }
    fn compaction(&mut self, _: &str, t: &str, id: &str, _: bool) -> Option<Value> {
        self.compact.get(&(t.into(), id.into())).cloned()
    }
    fn generation_event(&mut self, e: &Value, at: i64) {
        self.root.clocks.lock().unwrap().generation.observe(e, at);
    }
    fn turn_clock(&mut self, turn: &str, at: i64, complete: Option<bool>) -> Option<Value> {
        let mut c = self.root.clocks.lock().unwrap();
        match complete {
            None => {
                c.duration.start(turn, at);
                c.generation.start(turn, at);
                None
            }
            Some(ok) => {
                let extra = c.generation.settle(turn, at, ok);
                c.duration
                    .finish(turn, at, ok)
                    .map(|v| merge(v, &extra.unwrap_or(Value::Null)))
            }
        }
    }
}
impl Root {
    pub(super) fn ensure_projection(&self, id: &str) {
        self.projections
            .lock()
            .unwrap()
            .entry(id.into())
            .or_insert_with(|| {
                Projection::new(
                    id.into(),
                    (id != self.thread_id()).then(|| self.thread_id().into()),
                    &self.session.metadata.get().value,
                )
            });
    }
    pub(super) async fn history(&self, id: &str, thread: &Value) -> Result<()> {
        self.ensure_projection(id);
        let revision = self.projections.lock().unwrap()[id].title_revision();
        self.host.validate_history(id, thread).await?;
        if id == self.thread_id() {
            let ids = thread["turns"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|turn| turn["items"].as_array().into_iter().flatten())
                .filter(|item| item["type"] == "userMessage")
                .filter_map(|item| super::projection::user_id(id, item, text(item, "id")))
                .collect();
            let queue = self.q()?.clone();
            tokio::task::spawn_blocking(move || queue.commit_history(ids))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
        }
        let mut compact = HashMap::new();
        for turn in thread["turns"].as_array().into_iter().flatten() {
            for item in turn["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v["type"] == "contextCompaction")
            {
                if let (Some(t), Some(i)) = (text(turn, "id"), text(item, "id")) {
                    let mut summaries = self.summaries.lock().await;
                    let lookup = summaries
                        .entry(id.into())
                        .or_insert_with(|| Lookup::new(id.into()));
                    if let Some(v) = lookup.find(thread, t, i, 1).await {
                        compact.insert((t.into(), i.into()), v);
                    }
                }
            }
        }
        self.questions
            .lock()
            .unwrap()
            .history(&mut Controls(self), id, thread, now());
        self.projections
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .history_at(
                &mut Sink {
                    root: self,
                    compact,
                },
                thread,
                revision,
            )
    }
    pub(super) async fn project_live(
        &self,
        id: &str,
        method: &str,
        p: &Value,
        model: Option<&str>,
        at: i64,
    ) -> Result<()> {
        self.ensure_projection(id);
        let mut compact = HashMap::new();
        if method == "item/completed" && p["item"]["type"] == "contextCompaction" {
            if let (Some(t), Some(i)) = (text(p, "turnId"), text(&p["item"], "id")) {
                if let Ok(resp) = self
                    .rpc("thread/read", json!({"threadId":id,"includeTurns":false}))
                    .await
                {
                    let mut summaries = self.summaries.lock().await;
                    let lookup = summaries
                        .entry(id.into())
                        .or_insert_with(|| Lookup::new(id.into()));
                    if let Some(v) = lookup.find(&resp["thread"], t, i, 10).await {
                        compact.insert((t.into(), i.into()), v);
                    }
                }
            }
        }
        self.projections
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .notification(
                &mut Sink {
                    root: self,
                    compact,
                },
                method,
                p,
                model,
                at,
            )
    }
}

pub(crate) fn image(
    images: &hapi_wrapper_core::rpc::handlers::GeneratedImages,
    path: &str,
    id: &str,
    name: Option<&str>,
) -> Option<Value> {
    let path = Path::new(path);
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let stat = f.metadata().ok()?;
    if !stat.is_file() || stat.len() > 25 << 20 {
        return None;
    }
    let mut b = vec![0; stat.len() as usize];
    f.read_exact(&mut b).ok()?;
    let mime = mime(&b)?;
    let mut name = name
        .or_else(|| path.file_name()?.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{id}.png"));
    if Path::new(&name).extension().is_none() {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            name.push('.');
            name.push_str(ext);
        }
    }
    images
        .insert(id.into(), b, mime.into(), name.clone())
        .ok()?;
    Some(json!({"id":id,"fileName":name,"mimeType":mime}))
}
