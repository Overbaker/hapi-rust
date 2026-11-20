use super::*;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, DirBuilder, File, OpenOptions},
    io::Write,
    sync::{Arc, LazyLock, Mutex, Once, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[cfg(unix)]
fn file_identity_from_meta(meta: &fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

#[cfg(unix)]
fn file_identity_from_file(file: &File, meta: &fs::Metadata) -> (u64, u64) {
    let _ = file;
    file_identity_from_meta(meta)
}

#[cfg(unix)]
fn file_identity_from_path(path: &Path) -> io::Result<(u64, u64)> {
    Ok(file_identity_from_meta(&fs::symlink_metadata(path)?))
}

#[cfg(windows)]
fn file_identity_from_handle(file: &File) -> io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), index))
}

#[cfg(windows)]
fn file_identity_from_file(file: &File, _meta: &fs::Metadata) -> (u64, u64) {
    file_identity_from_handle(file).unwrap_or((0, 0))
}

#[cfg(windows)]
fn file_identity_from_path(path: &Path) -> io::Result<(u64, u64)> {
    let file = File::open(path)?;
    file_identity_from_handle(&file)
}

#[cfg(not(any(unix, windows)))]
fn file_identity_from_file(_file: &File, _meta: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

#[cfg(not(any(unix, windows)))]
fn file_identity_from_path(_path: &Path) -> io::Result<(u64, u64)> {
    Ok((0, 0))
}

fn write_chunk_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        file.write_all_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt as WinFileExt;
        let (mut buf, mut offset) = (buf, offset);
        while !buf.is_empty() {
            let n = file.seek_write(buf, offset)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write upload chunk",
                ));
            }
            buf = &buf[n..];
            offset += n as u64;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positional writes are not supported on this platform",
        ))
    }
}

const MIB: u64 = 1024 * 1024;
const TTL: Duration = Duration::from_secs(900);
struct Active {
    key: String,
    path: PathBuf,
    part: PathBuf,
    file: Option<Arc<File>>,
    busy: bool,
    aborted: bool,
    total: u64,
    received: u64,
    index: u64,
    deadline: u64,
    timer: tokio::task::JoinHandle<()>,
}
struct Directory {
    path: PathBuf,
    created: OnceLock<Result<(), String>>,
}
#[derive(Default)]
struct State {
    pending: usize,
    dirs: HashMap<String, Arc<Directory>>,
    ids: HashMap<String, HashMap<PathBuf, (u64, u64)>>,
    retained: HashSet<String>,
    active: HashMap<String, Active>,
    generations: HashMap<String, u64>,
}
static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
type Listener = Arc<dyn Fn() + Send + Sync>;
static STARTED: LazyLock<Mutex<HashMap<String, Listener>>> = LazyLock::new(Mutex::default);
/// Whether `session_id` has a chunked upload in progress (TS `hasActiveChunkedUpload`).
pub fn has_active_upload(session_id: &str) -> bool {
    let session = key(session_id);
    STATE
        .lock()
        .unwrap()
        .active
        .values()
        .any(|a| a.key == session)
}
/// Calls `listener` whenever a chunked upload for `session_id` starts
/// (TS `onUploadStarted`); replaces any earlier listener for that session.
pub fn on_upload_started(session_id: &str, listener: Listener) {
    STARTED
        .lock()
        .unwrap()
        .insert(key(session_id).into(), listener);
}
pub fn clear_upload_listener(session_id: &str) {
    STARTED.lock().unwrap().remove(key(session_id));
}
fn upload_started(session: &str) {
    let listener = STARTED.lock().unwrap().get(session).cloned();
    if let Some(listener) = listener {
        listener();
    }
}
static EXIT: Once = Once::new();
fn key(raw: &str) -> &str {
    let trimmed = raw.trim_matches(js_space);
    if trimmed.is_empty() {
        "unknown"
    } else {
        trimmed
    }
}
fn js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}
fn fit(raw: &str, max: usize) -> String {
    let safe = raw.replace(['/', '\\'], "_").replace("..", "_");
    let mut name = String::new();
    let mut space = false;
    for c in safe.chars() {
        let white = js_space(c);
        if !white {
            name.push(c)
        } else if !space {
            name.push('_')
        }
        space = white;
    }
    if name.is_empty() {
        return "upload".into();
    }
    if name.len() <= max {
        return name;
    }
    let ext = name
        .rfind('.')
        .filter(|i| *i > 0 && name.len() - i <= max)
        .map(|i| &name[i..])
        .unwrap_or("");
    format!(
        "{}{}",
        &name[..name.floor_char_boundary(max - ext.len())],
        ext
    )
}
fn blobs() -> PathBuf {
    std::env::temp_dir().join("hapi-blobs")
}
impl State {
    fn within(&self, key: &str, path: &Path) -> bool {
        if path.components().any(|c| c == Component::ParentDir) {
            return false;
        }
        if let Some(dir) = self.dirs.get(key) {
            path != dir.path && path.starts_with(&dir.path)
        } else {
            path.strip_prefix(blobs())
                .ok()
                .and_then(|p| p.components().next())
                .is_some_and(|c| {
                    c.as_os_str()
                        .to_string_lossy()
                        .starts_with(&format!("{}-", fit(key, 120)))
                })
        }
    }
}
pub fn is_path_within_upload_dir(path: &Path, session_id: &str) -> bool {
    STATE.lock().unwrap().within(key(session_id), path)
}
fn delete_path(p: &Path, session: &str) -> Result<(), String> {
    let invalid = "Invalid upload path";
    if !is_path_within_upload_dir(p, session) {
        return Err(invalid.into());
    }
    let base = blobs();
    let subdir = p
        .strip_prefix(&base)
        .ok()
        .and_then(|p| p.components().next())
        .ok_or(invalid)?;
    let parent = p.parent().ok_or(invalid)?;
    let parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(fs_error(e, "realpath", parent)),
    };
    let allowed = base
        .canonicalize()
        .map_err(|e| fs_error(e, "realpath", &base))?
        .join(subdir.as_os_str());
    if !parent.starts_with(allowed) {
        return Err(invalid.into());
    }
    let target = parent.join(p.file_name().ok_or(invalid)?);
    match fs::remove_file(&target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(fs_error(e, "unlink", &target)),
    }
}
pub fn is_authorized_upload_file(path: &Path, session_id: &str, dev: u64, ino: u64) -> bool {
    STATE
        .lock()
        .unwrap()
        .ids
        .get(key(session_id))
        .and_then(|files| files.get(path))
        == Some(&(dev, ino))
}
pub fn preserve_upload_dir_on_exit(session_id: &str) {
    STATE
        .lock()
        .unwrap()
        .retained
        .insert(key(session_id).into());
}
fn directory(session: &str, generation: u64) -> Result<Arc<Directory>, String> {
    let mut state = STATE.lock().unwrap();
    if state.generations.get(session).copied().unwrap_or(0) != generation {
        return Err("Upload directory cleanup requested".into());
    }
    let dir = state
        .dirs
        .entry(session.to_owned())
        .or_insert_with(|| {
            Arc::new(Directory {
                path: blobs().join(format!("{}-{}", fit(session, 120), Uuid::new_v4())),
                created: OnceLock::new(),
            })
        })
        .clone();
    drop(state);
    let created = dir
        .created
        .get_or_init(|| {
            let base = dir.path.parent().unwrap();
            fs::create_dir_all(base).map_err(|e| fs_error(e, "mkdir", base))?;
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            builder
                .create(&dir.path)
                .map_err(|e| fs_error(e, "mkdir", &dir.path))
        })
        .clone();
    let mut state = STATE.lock().unwrap();
    if !state
        .dirs
        .get(session)
        .is_some_and(|current| Arc::ptr_eq(current, &dir))
    {
        drop(state);
        if created.is_ok() {
            let _ = fs::remove_dir_all(&dir.path);
        }
        return Err("Upload directory cleanup requested".into());
    }
    if let Err(e) = created {
        state.dirs.remove(session);
        return Err(e);
    }
    Ok(dir)
}
fn dispose(mut active: Active) {
    active.timer.abort();
    // Drop open handle before removing the .part file (required on Windows).
    drop(active.file.take());
    let _ = fs::remove_file(active.part);
}
pub fn cleanup_upload_dir(session_id: &str) {
    let key = key(session_id);
    let (active, dir) = {
        let mut state = STATE.lock().unwrap();
        let active = state
            .active
            .extract_if(|_, a| a.key == key)
            .map(|(_, a)| a)
            .collect::<Vec<_>>();
        state.retained.remove(key);
        state.ids.remove(key);
        *state.generations.entry(key.into()).or_default() += 1;
        (active, state.dirs.remove(key))
    };
    for a in active {
        dispose(a)
    }
    if let Some(dir) = dir {
        let _ = fs::remove_dir_all(&dir.path);
    }
}
extern "C" fn cleanup_exit() {
    let dirs = match STATE.try_lock() {
        Ok(s) => s
            .dirs
            .iter()
            .filter(|(k, _)| !s.retained.contains(*k))
            .map(|(_, dir)| dir.path.clone())
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    for dir in dirs {
        let _ = fs::remove_dir_all(dir);
    }
}
fn abort_locked(s: &mut State, id: &str) -> Option<Active> {
    let a = s.active.get_mut(id)?;
    a.aborted = true;
    if a.busy {
        None
    } else {
        s.active.remove(id)
    }
}
fn expiration(id: &str, ttl: Duration, deadline: u64) -> tokio::task::JoinHandle<()> {
    let id = id.to_owned();
    tokio::spawn(async move {
        tokio::time::sleep(ttl).await;
        let active = {
            let mut s = STATE.lock().unwrap();
            if s.active.get(&id).is_some_and(|a| a.deadline == deadline) {
                abort_locked(&mut s, &id)
            } else {
                None
            }
        };
        if let Some(a) = active {
            let _ = tokio::task::spawn_blocking(move || dispose(a)).await;
        }
    })
}
fn refresh(a: &mut Active, id: &str, ttl: Duration) {
    a.timer.abort();
    a.deadline += 1;
    a.timer = expiration(id, ttl, a.deadline);
}
struct Slot(bool);
impl Slot {
    fn reserve() -> Option<Self> {
        let mut s = STATE.lock().unwrap();
        if s.pending + s.active.len() >= 16 {
            return None;
        }
        s.pending += 1;
        Some(Self(true))
    }
    fn publish(&mut self, state: &mut State) {
        state.pending -= 1;
        self.0 = false;
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        if self.0 {
            STATE.lock().unwrap().pending -= 1;
        }
    }
}
struct Operation {
    id: String,
    file: Option<Arc<File>>,
    path: PathBuf,
    part: PathBuf,
    total: u64,
    received: u64,
    index: u64,
}
fn missing() -> Value {
    upload_error("upload_not_found", "Upload not found")
}
impl Operation {
    fn begin(session: &str, id: &str) -> Result<Self, Value> {
        if id.is_empty() {
            return Err(upload_error("upload_invalid_chunk", "uploadId is required"));
        }
        let mut s = STATE.lock().unwrap();
        let a = s
            .active
            .get_mut(id)
            .filter(|a| a.key == session && !a.aborted)
            .ok_or_else(missing)?;
        if a.busy {
            return Err(upload_error(
                "upload_conflict",
                "Upload operation already in progress",
            ));
        }
        a.busy = true;
        Ok(Self {
            id: id.into(),
            file: a.file.clone(),
            path: a.path.clone(),
            part: a.part.clone(),
            total: a.total,
            received: a.received,
            index: a.index,
        })
    }
    fn abort(&self) {
        if let Some(a) = STATE.lock().unwrap().active.get_mut(&self.id) {
            a.aborted = true;
        }
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        let removed = {
            let mut s = STATE.lock().unwrap();
            let Some(a) = s.active.get_mut(&self.id) else {
                return;
            };
            a.busy = false;
            if a.aborted || std::thread::panicking() {
                s.active.remove(&self.id)
            } else {
                None
            }
        };
        if let Some(a) = removed {
            dispose(a)
        }
    }
}
fn estimate(content: &str) -> u64 {
    (content.encode_utf16().count() as u64 * 3 / 4).saturating_sub(if content.ends_with("==") {
        2
    } else if content.ends_with('=') {
        1
    } else {
        0
    })
}
fn upload_error(code: &str, msg: impl ToString) -> Value {
    json!({"success":false,"error":msg.to_string(),"code":code})
}
fn max_bytes() -> u64 {
    std::env::var("HAPI_MAX_UPLOAD_BYTES")
        .ok()
        .and_then(|s| {
            let s = s.trim_matches(js_space);
            s.parse::<u64>()
                .ok()
                .filter(|_| s.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter(|n| (1..=9007199254740991).contains(n))
        .unwrap_or(512 * MIB)
        .min(10240 * MIB)
}
fn open_new(p: &Path) -> Result<File, String> {
    let mut open = OpenOptions::new();
    open.create_new(true).write(true);
    #[cfg(unix)]
    open.mode(0o600);
    open.open(p).map_err(|e| fs_error(e, "open", p))
}
#[cfg(test)]
fn handle(method: &str, v: Value, ttl: Duration) -> Result<Value, String> {
    let op = match admission(method, &v) {
        Ok(op) => op,
        Err(response) => return Ok(response),
    };
    run(method, v, ttl, op)
}
enum Admitted {
    Other,
    Create(u64),
    Operation(Operation),
    Abort(Option<Active>),
}
fn admission(method: &str, v: &Value) -> Result<Admitted, Value> {
    let session = key(field(v, "sessionId"));
    let id = field(v, "uploadId");
    match method {
        "uploadChunk" | "uploadFinalize" => Operation::begin(session, id).map(Admitted::Operation),
        "uploadFile" | "uploadInit" => Ok(Admitted::Create(
            STATE
                .lock()
                .unwrap()
                .generations
                .get(session)
                .copied()
                .unwrap_or(0),
        )),
        "uploadAbort" => {
            if id.is_empty() {
                return Err(upload_error("upload_invalid_chunk", "uploadId is required"));
            }
            let mut s = STATE.lock().unwrap();
            if s.active.get(id).is_some_and(|a| a.key != session) {
                return Err(missing());
            }
            Ok(Admitted::Abort(abort_locked(&mut s, id)))
        }
        _ => Ok(Admitted::Other),
    }
}
fn run(method: &str, v: Value, ttl: Duration, admitted: Admitted) -> Result<Value, String> {
    let session = key(field(&v, "sessionId"));
    let id = field(&v, "uploadId");
    let content = field(&v, "content");
    let filename = field(&v, "filename");
    let invalid = |s: &str| upload_error("upload_invalid_chunk", s);
    match method {
        "deleteUpload" => {
            let path = field(&v, "path").trim_matches(js_space);
            if path.is_empty() {
                return Err("Path is required".into());
            }
            let p = Path::new(path);
            delete_path(p, session)?;
            if let Some(files) = STATE.lock().unwrap().ids.get_mut(session) {
                files.remove(p);
            }
        }
        "uploadFile" | "uploadInit" => {
            let init = method == "uploadInit";
            if filename.is_empty() {
                return Ok(if init {
                    invalid("Filename is required")
                } else {
                    error("Filename is required")
                });
            }
            let mut total = 0;
            let max = max_bytes();
            let mut slot = None;
            if init {
                let Some(n) = v["size"]
                    .as_f64()
                    .filter(|n| n.fract() == 0. && (0.0..=9007199254740991.).contains(n))
                else {
                    return Ok(invalid("Valid file size is required"));
                };
                total = n as u64;
                if total > max {
                    return Ok(upload_error(
                        "upload_too_large",
                        format!("File too large (max {}MB)", max / MIB),
                    ));
                }
                slot = Slot::reserve();
                if slot.is_none() {
                    return Ok(upload_error(
                        "upload_conflict",
                        "Too many concurrent uploads",
                    ));
                }
            } else {
                if content.is_empty() {
                    return Err("Content is required".into());
                }
                if estimate(content) > 50 * MIB {
                    return Err("File too large (max 50MB)".into());
                }
            }
            let Admitted::Create(generation) = admitted else {
                unreachable!("uploads reserve their generation at dispatch")
            };
            let dir = directory(session, generation)?;
            let prefix = format!(
                "{}-{}-",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                Uuid::new_v4()
            );
            let path = dir.path.join(format!(
                "{prefix}{}",
                fit(filename, 255 - prefix.len() - if init { 5 } else { 0 })
            ));
            if init {
                let part = path.with_added_extension("part");
                let file = open_new(&part)?;
                let id = Uuid::new_v4().to_string();
                let timer = expiration(&id, ttl, 0);
                let mut s = STATE.lock().unwrap();
                if !s
                    .dirs
                    .get(session)
                    .is_some_and(|current| Arc::ptr_eq(current, &dir))
                {
                    drop(s);
                    timer.abort();
                    let _ = fs::remove_file(part);
                    return Ok(upload_error(
                        "upload_conflict",
                        "Upload directory cleanup requested",
                    ));
                }
                slot.as_mut().unwrap().publish(&mut s);
                s.active.insert(
                    id.clone(),
                    Active {
                        key: session.into(),
                        file: Some(Arc::new(file)),
                        busy: false,
                        aborted: false,
                        part,
                        path,
                        total,
                        received: 0,
                        index: 0,
                        deadline: 0,
                        timer,
                    },
                );
                drop(s);
                upload_started(session);
                return Ok(
                    json!({"success":true,"uploadId":id,"chunkSize":4*MIB,"maxUploadBytes":max}),
                );
            }
            let bytes = files::decode(content);
            if bytes.len() as u64 > 50 * MIB {
                return Err("File too large (max 50MB)".into());
            }
            let mut file = open_new(&path)?;
            let info = file
                .write_all(&bytes)
                .and_then(|_| file.metadata())
                .inspect_err(|_| {
                    let _ = fs::remove_file(&path);
                })
                .map_err(|e| fs_error(e, "write", &path))?;
            let file_id = file_identity_from_file(&file, &info);
            drop(file);
            let mut s = STATE.lock().unwrap();
            if !s
                .dirs
                .get(session)
                .is_some_and(|current| Arc::ptr_eq(current, &dir))
            {
                drop(s);
                let _ = fs::remove_file(&path);
                return Err("Upload directory cleanup requested".into());
            }
            s.ids
                .entry(session.into())
                .or_default()
                .insert(path.clone(), file_id);
            return Ok(json!({"success":true,"path":path}));
        }
        "uploadAbort" => {
            if let Admitted::Abort(Some(a)) = admitted {
                dispose(a)
            }
        }
        _ => {
            let Admitted::Operation(mut a) = admitted else {
                unreachable!("chunk and finalize reserve the operation at dispatch")
            };
            match method {
                "uploadChunk" => {
                    if v["index"].as_f64() != Some(a.index as f64) {
                        let got = v
                            .get("index")
                            .map(|v| match v {
                                Value::String(s) => s.clone(),
                                Value::Number(n) => {
                                    crate::jsjson::number_string(n.as_f64().unwrap_or(f64::NAN))
                                }
                                v => v.to_string(),
                            })
                            .unwrap_or_else(|| "undefined".into());
                        return Ok(invalid(&format!(
                            "Expected chunk index {}, got {got}",
                            a.index
                        )));
                    }
                    if content.is_empty() {
                        return Ok(invalid("Chunk content is required"));
                    }
                    let large = || {
                        upload_error("upload_chunk_too_large", "Chunk exceeds maximum size (8MB)")
                    };
                    if estimate(content) > 8 * MIB {
                        return Ok(large());
                    }
                    let bytes = files::decode(content);
                    if bytes.is_empty() {
                        return Ok(invalid("Chunk content is invalid"));
                    }
                    if bytes.len() as u64 > 8 * MIB {
                        return Ok(large());
                    }
                    if a.received + bytes.len() as u64 > a.total {
                        a.abort();
                        return Ok(upload_error(
                            "upload_size_mismatch",
                            "Uploaded bytes exceed declared file size",
                        ));
                    }
                    if let Some(active) = STATE.lock().unwrap().active.get_mut(id) {
                        refresh(active, id, ttl);
                    }
                    let Some(file) = a.file.as_ref() else {
                        a.abort();
                        return Ok(upload_error("upload_not_found", "Upload aborted"));
                    };
                    if let Err(e) = write_chunk_at(file, &bytes, a.received) {
                        let msg = fs_error(e, "write", &a.part);
                        a.abort();
                        return Err(msg);
                    }
                    let mut s = STATE.lock().unwrap();
                    let Some(a) = s.active.get_mut(id).filter(|a| !a.aborted) else {
                        return Ok(upload_error("upload_not_found", "Upload aborted"));
                    };
                    a.received += bytes.len() as u64;
                    a.index += 1;
                    refresh(a, id, ttl);
                    return Ok(json!({"success":true,"receivedBytes":a.received}));
                }
                "uploadFinalize" => {
                    if a.received != a.total {
                        return Ok(upload_error(
                            "upload_incomplete",
                            format!(
                                "Incomplete upload: received {} of {} bytes",
                                a.received, a.total
                            ),
                        ));
                    }
                    let closed_handle = {
                        let mut s = STATE.lock().unwrap();
                        if let Some(active) = s.active.get_mut(id) {
                            active.timer.abort();
                            active.deadline += 1;
                            active.file.take()
                        } else {
                            None
                        }
                    };
                    // Close all handles to `.part` before renaming (required on Windows).
                    drop(closed_handle);
                    drop(a.file.take());
                    let info = fs::rename(&a.part, &a.path)
                        .and_then(|_| fs::symlink_metadata(&a.path))
                        .inspect_err(|_| {
                            a.abort();
                            let _ = fs::remove_file(&a.part);
                            let _ = fs::remove_file(&a.path);
                        })
                        .map_err(|e| fs_error(e, "rename", &a.part))?;
                    if !info.is_file() || info.len() != a.total {
                        a.abort();
                        let _ = fs::remove_file(&a.path);
                        return Ok(if info.is_file() {
                            upload_error("upload_size_mismatch", "Finalized upload size mismatch")
                        } else {
                            upload_error("upload_conflict", "Upload aborted during finalization")
                        });
                    }
                    let file_id = file_identity_from_path(&a.path).unwrap_or((0, 0));
                    let mut s = STATE.lock().unwrap();
                    if !s.active.get(id).is_some_and(|a| !a.aborted) {
                        drop(s);
                        let _ = fs::remove_file(&a.path);
                        return Ok(upload_error(
                            "upload_conflict",
                            "Upload aborted during finalization",
                        ));
                    }
                    let finished = s.active.remove(id).unwrap();
                    s.ids
                        .entry(session.into())
                        .or_default()
                        .insert(a.path.clone(), file_id);
                    drop(s);
                    finished.timer.abort();
                    return Ok(json!({"success":true,"path":a.path}));
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(json!({"success":true}))
}
fn validate_files(session: &str, data: &Value) -> bool {
    let Some(files) = data["files"].as_array() else {
        return false;
    };
    let session = key(session);
    if files.is_empty() {
        return STATE.lock().unwrap().ids.contains_key(session);
    }
    files.iter().all(|file| {
        let Some(path) = file["path"].as_str() else {
            return false;
        };
        let registered = STATE
            .lock()
            .unwrap()
            .ids
            .get(session)
            .and_then(|files| files.get_key_value(Path::new(path)))
            .is_some_and(|(registered, _)| registered.as_os_str() == std::ffi::OsStr::new(path));
        registered
            && fs::symlink_metadata(path).is_ok_and(|m| {
                m.is_file()
                    && Some(m.len() as f64) == file["size"].as_f64()
                    && file_identity_from_path(Path::new(path)).is_ok_and(|(dev, ino)| {
                        is_authorized_upload_file(Path::new(path), session, dev, ino)
                    })
            })
    })
}
pub(super) fn register_validation(manager: &RpcHandlerManager, session: &str) {
    let session = session.to_owned();
    manager.register("validate-session-uploads", move |data| {
        let session = session.clone();
        async move {
            let valid = tokio::task::spawn_blocking(move || validate_files(&session, &data))
                .await
                .map_err(|e| e.to_string())?;
            Ok(Some(valid.into()))
        }
    });
}
pub fn register(manager: &RpcHandlerManager) {
    EXIT.call_once(|| {
        #[cfg(unix)]
        unsafe {
            nix::libc::atexit(cleanup_exit);
        }
        #[cfg(windows)]
        {
            extern "C" {
                fn atexit(cb: extern "C" fn()) -> std::ffi::c_int;
            }
            unsafe {
                atexit(cleanup_exit);
            }
        }
    });
    for method in [
        "uploadFile",
        "deleteUpload",
        "uploadInit",
        "uploadChunk",
        "uploadFinalize",
        "uploadAbort",
    ] {
        manager.register(method, move |v| async move {
            let operation = match admission(method, &v) {
                Ok(op) => op,
                Err(response) => return Ok(Some(response)),
            };
            let reply = tokio::task::spawn_blocking(move || {
                run(method, v, TTL, operation).unwrap_or_else(error)
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok(Some(reply))
        });
    }
}
#[cfg(test)]
#[path = "tests_uploads.rs"]
mod tests_uploads;
#[cfg(test)]
mod tests {
    use super::*;
    fn request(m: &str, k: &str, mut v: Value) -> Value {
        v["sessionId"] = json!(k);
        handle(m, v, TTL).unwrap_or_else(error)
    }
    #[tokio::test]
    async fn validate_only_intact_session_uploads() {
        let owner = "validate-session-uploads-test";
        let upload = request(
            "uploadFile",
            owner,
            json!({"filename":"x","content":"eA=="}),
        );
        let path = upload["path"].as_str().unwrap();
        let files = json!({"files":[{"path":path,"size":1}]});
        assert!(validate_files(owner, &files));
        assert!(!validate_files("other-session", &files));
        assert!(!validate_files(
            owner,
            &json!({"files":[{"path":path,"size":2}]})
        ));
        fs::rename(path, format!("{path}.old")).unwrap();
        fs::write(path, "x").unwrap();
        assert!(!validate_files(owner, &files));
        cleanup_upload_dir(owner);
    }
    #[test]
    fn filename_budget() {
        for name in ["a".repeat(300), "😀".repeat(300)] {
            let n = fit(&format!("{name}.png"), 200);
            assert!(n.len() <= 200 && n.ends_with(".png"))
        }
        assert_eq!(fit("x.abc", 4), ".abc");
        assert_eq!(fit("../a b\\c", 200), "__a_b_c");
        assert_eq!(fit("a\u{feff}\u{a0}b\u{85}c", 200), "a_b\u{85}c");
        assert_eq!(key("\u{feff} k\u{85}"), "k\u{85}");
    }
    #[tokio::test]
    async fn active_upload_is_reported_and_announced() {
        let k = "upload-active-flag";
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = started.clone();
        on_upload_started(
            k,
            Arc::new(move || {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        );
        assert!(!has_active_upload(k));
        let id = request("uploadInit", k, json!({"filename":"f","size":1}))["uploadId"].clone();
        assert!(has_active_upload(&format!(" {k} ")));
        assert!(!has_active_upload("other"));
        assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            request("uploadAbort", k, json!({"uploadId":id}))["success"],
            true
        );
        assert!(!has_active_upload(k));
        clear_upload_listener(k);
        let id = request("uploadInit", k, json!({"filename":"f","size":1}))["uploadId"].clone();
        assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1);
        request("uploadAbort", k, json!({"uploadId":id}));
        cleanup_upload_dir(k);
    }
    #[tokio::test]
    async fn upload_lifecycle() {
        let k = "upload-lifecycle";
        let base = blobs().join(format!("{k}-random/file"));
        assert!(is_path_within_upload_dir(&base, k));
        assert!(!is_path_within_upload_dir(&base, "other"));
        let payload = json!({"filename":"😀".repeat(300)+".png","content":"AQIDBA=="});
        let one = request("uploadFile", k, payload.clone());
        let two = request("uploadFile", k, payload);
        assert_ne!(one["path"], two["path"]);
        let path = Path::new(one["path"].as_str().unwrap());
        let (dev, ino) = file_identity_from_path(path).unwrap();
        assert!(is_authorized_upload_file(path, k, dev, ino));
        assert!(!is_authorized_upload_file(path, "other", dev, ino));
        assert_eq!(fs::read(path).unwrap(), [1, 2, 3, 4]);
        let nel = request(
            "uploadFile",
            k,
            json!({"filename":"nel\u{85}","content":"AQ=="}),
        );
        let nel = Path::new(nel["path"].as_str().unwrap());
        assert!(nel.exists());
        assert_eq!(
            request("deleteUpload", k, json!({"path":nel}))["success"],
            true
        );
        assert!(!nel.exists());
        let replacement = path.with_file_name("replacement");
        fs::write(&replacement, "x").unwrap();
        fs::rename(&replacement, path).unwrap();
        let (dev, ino) = file_identity_from_path(path).unwrap();
        assert!(!is_authorized_upload_file(path, k, dev, ino));
        let init = |n: u64| {
            request(
                "uploadInit",
                k,
                json!({"filename":"😀".repeat(300)+".png","size":n}),
            )
        };
        assert_eq!(init(10240 * MIB + 1)["code"], "upload_too_large");
        let id = init(3)["uploadId"].clone();
        let req = json!({"uploadId":id});
        let chunk = |i: u64, s: &str| {
            request(
                "uploadChunk",
                k,
                json!({"uploadId":id,"index":i,"content":s}),
            )
        };
        assert_eq!(
            request("uploadAbort", "other", req.clone())["code"],
            "upload_not_found"
        );
        assert_eq!(
            request("uploadFinalize", k, req.clone())["error"],
            "Incomplete upload: received 0 of 3 bytes"
        );
        assert_eq!(chunk(1, "AQ==")["code"], "upload_invalid_chunk");
        for (index, got) in [(json!(1.0), "1"), (json!("x"), "x"), (json!(null), "null")] {
            assert_eq!(
                request(
                    "uploadChunk",
                    k,
                    json!({"uploadId":id,"index":index,"content":"AQ=="})
                )["error"],
                format!("Expected chunk index 0, got {got}")
            );
        }
        assert_eq!(
            request("uploadChunk", k, json!({"uploadId":id,"content":"AQ=="}))["error"],
            "Expected chunk index 0, got undefined"
        );
        assert_eq!(chunk(0, "!!!!")["error"], "Chunk content is invalid");
        assert_eq!(chunk(0, "AQI=")["receivedBytes"], 2);
        assert_eq!(chunk(1, "Aw==")["receivedBytes"], 3);
        let finish = request("uploadFinalize", k, req);
        let path = Path::new(finish["path"].as_str().unwrap());
        assert!(path.file_name().unwrap().len() <= 250);
        assert_eq!(fs::read(path).unwrap(), [1, 2, 3]);
        let (dev, ino) = file_identity_from_path(path).unwrap();
        assert!(is_authorized_upload_file(path, k, dev, ino));
        assert_eq!(
            request("deleteUpload", k, json!({"path":path}))["success"],
            true
        );
        assert!(!path.exists());
        let empty = init(0);
        assert_eq!(
            request("uploadFinalize", k, json!({"uploadId":empty["uploadId"]}))["success"],
            true
        );
        let empty = init(0);
        assert_eq!(
            request(
                "uploadChunk",
                k,
                json!({"uploadId":empty["uploadId"],"index":0,"content":"AQ=="})
            )["code"],
            "upload_size_mismatch"
        );
        assert_eq!(
            request("uploadAbort", k, json!({"uploadId":empty["uploadId"]}))["success"],
            true
        );
        let mut pending = tokio::task::JoinSet::new();
        for _ in 0..20 {
            pending.spawn_blocking(move || {
                request("uploadInit", k, json!({"filename":"file","size":1}))
            });
        }
        let mut ids = vec![];
        while let Some(result) = pending.join_next().await {
            let r = result.unwrap();
            if r["success"] == true {
                ids.push(r["uploadId"].clone())
            } else {
                assert_eq!(r["code"], "upload_conflict")
            }
        }
        assert_eq!(ids.len(), 16);
        assert_eq!(
            request("uploadAbort", k, json!({"uploadId":ids[0]}))["success"],
            true
        );
        assert_eq!(init(1)["success"], true);
        let dir = path.parent().unwrap();
        assert!(dir
            .read_dir()
            .unwrap()
            .flatten()
            .any(|e| e.path().extension().is_some_and(|s| s == "part")));
        preserve_upload_dir_on_exit(k);
        cleanup_exit();
        assert!(dir.exists());
        cleanup_upload_dir(k);
        assert!(!dir.exists());
        let during_panic = init(1);
        let id = during_panic["uploadId"].as_str().unwrap();
        assert!(std::panic::catch_unwind(|| {
            let _guard = Operation::begin(k, id).unwrap();
            panic!("cancel in-flight operation");
        })
        .is_err());
        assert_eq!(
            request("uploadFinalize", k, json!({"uploadId":id}))["code"],
            "upload_not_found"
        );
        let locked = STATE.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            cleanup_exit();
            tx.send(()).unwrap();
        });
        let skipped = rx.recv_timeout(Duration::from_secs(2)).is_ok();
        drop(locked);
        cleanup.join().unwrap();
        assert!(skipped);
        let short = handle(
            "uploadInit",
            json!({"sessionId":k,"filename":"file","size":10000}),
            Duration::from_millis(5),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            request("uploadFinalize", k, json!({"uploadId":short["uploadId"]}))["code"],
            "upload_not_found"
        );
        cleanup_upload_dir(k);
    }
}
