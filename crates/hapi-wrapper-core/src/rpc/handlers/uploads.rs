use super::*;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, DirBuilder, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    sync::{LazyLock, Mutex, Once},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MIB: u64 = 1024 * 1024;
const TTL: Duration = Duration::from_secs(900);
struct Active {
    key: String,
    path: PathBuf,
    part: PathBuf,
    file: File,
    total: u64,
    received: u64,
    index: u64,
    timer: tokio::task::JoinHandle<()>,
}
#[derive(Default)]
struct State {
    dirs: HashMap<String, PathBuf>,
    ids: HashMap<(String, PathBuf), (u64, u64)>,
    retained: HashSet<String>,
    active: HashMap<String, Active>,
}
static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static BUSY: LazyLock<Mutex<HashSet<(String, String)>>> = LazyLock::new(Mutex::default);
static EXIT: Once = Once::new();
fn key(raw: &str) -> &str {
    if raw.trim().is_empty() {
        "unknown"
    } else {
        raw.trim()
    }
}
fn fit(raw: &str, max: usize) -> String {
    let safe = raw.replace(['/', '\\'], "_").replace("..", "_");
    let mut name = String::new();
    let mut space = false;
    for c in safe.chars() {
        if !c.is_whitespace() {
            name.push(c)
        } else if !space {
            name.push('_')
        }
        space = c.is_whitespace();
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
    fn dir(&mut self, key: &str) -> Result<PathBuf, String> {
        if let Some(p) = self.dirs.get(key) {
            return Ok(p.clone());
        }
        let base = blobs();
        fs::create_dir_all(&base).map_err(|e| fs_error(e, "mkdir", &base))?;
        let path = base.join(format!("{}-{}", fit(key, 120), Uuid::new_v4()));
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| fs_error(e, "mkdir", &path))?;
        self.dirs.insert(key.into(), path.clone());
        Ok(path)
    }
    fn abort(&mut self, id: &str) {
        if let Some(active) = self.active.remove(id) {
            active.timer.abort();
            drop(active.file);
            let _ = fs::remove_file(active.part);
        }
    }
    fn within(&self, key: &str, path: &Path) -> bool {
        if path.components().any(|c| c == Component::ParentDir) {
            return false;
        }
        if let Some(dir) = self.dirs.get(key) {
            path != dir && path.starts_with(dir)
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
pub fn is_authorized_upload_file(path: &Path, session_id: &str, dev: u64, ino: u64) -> bool {
    STATE
        .lock()
        .unwrap()
        .ids
        .get(&(key(session_id).into(), path.to_owned()))
        == Some(&(dev, ino))
}
pub fn preserve_upload_dir_on_exit(session_id: &str) {
    STATE
        .lock()
        .unwrap()
        .retained
        .insert(key(session_id).into());
}
pub fn cleanup_upload_dir(session_id: &str) {
    let key = key(session_id);
    let mut s = STATE.lock().unwrap();
    let ids = s
        .active
        .iter()
        .filter(|(_, a)| a.key == key)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        s.abort(&id)
    }
    s.retained.remove(key);
    s.ids.retain(|(k, _), _| k != key);
    if let Some(p) = s.dirs.remove(key) {
        let _ = fs::remove_dir_all(p);
    }
}
extern "C" fn cleanup_exit() {
    if let Ok(s) = STATE.lock() {
        for (k, p) in &s.dirs {
            if !s.retained.contains(k) {
                let _ = fs::remove_dir_all(p);
            }
        }
    }
}
fn expiration(id: &str, ttl: Duration, index: u64) -> tokio::task::JoinHandle<()> {
    let id = id.to_owned();
    tokio::spawn(async move {
        tokio::time::sleep(ttl).await;
        let _ = tokio::task::spawn_blocking(move || {
            let mut state = STATE.lock().unwrap();
            if state.active.get(&id).is_some_and(|a| a.index == index) {
                state.abort(&id)
            }
        })
        .await;
    })
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
            s.trim()
                .parse::<u64>()
                .ok()
                .filter(|_| s.trim().bytes().all(|b| b.is_ascii_digit()))
        })
        .filter(|n| (1..=9007199254740991).contains(n))
        .unwrap_or(512 * MIB)
        .min(10240 * MIB)
}
fn open_new(p: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(p)
        .map_err(|e| fs_error(e, "open", p))
}
fn handle(method: &str, v: Value, ttl: Duration) -> Result<Value, String> {
    let session = key(field(&v, "sessionId"));
    let id = field(&v, "uploadId");
    let content = field(&v, "content");
    let filename = field(&v, "filename");
    let invalid = |s: &str| upload_error("upload_invalid_chunk", s);
    let missing = || upload_error("upload_not_found", "Upload not found");
    let mut s = STATE.lock().unwrap();
    match method {
        "deleteUpload" => {
            let path = field(&v, "path").trim();
            if path.is_empty() {
                return Err("Path is required".into());
            }
            let p = Path::new(path);
            if !s.within(session, p) {
                return Err("Invalid upload path".into());
            }
            if let Err(e) = fs::remove_file(p) {
                if e.kind() != io::ErrorKind::NotFound {
                    return Err(fs_error(e, "unlink", p));
                }
            }
            s.ids.remove(&(session.into(), p.to_owned()));
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
                if s.active.len() >= 16 {
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
            let dir = s.dir(session)?;
            let prefix = format!(
                "{}-{}-",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                Uuid::new_v4()
            );
            let path = dir.join(format!(
                "{prefix}{}",
                fit(filename, 255 - prefix.len() - if init { 5 } else { 0 })
            ));
            if init {
                let part = path.with_added_extension("part");
                let file = open_new(&part)?;
                let id = Uuid::new_v4().to_string();
                let timer = expiration(&id, ttl, 0);
                s.active.insert(
                    id.clone(),
                    Active {
                        key: session.into(),
                        file,
                        part,
                        path,
                        total,
                        received: 0,
                        index: 0,
                        timer,
                    },
                );
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
            s.ids
                .insert((session.into(), path.clone()), (info.dev(), info.ino()));
            return Ok(json!({"success":true,"path":path}));
        }
        _ => {
            if id.is_empty() {
                return Ok(invalid("uploadId is required"));
            }
            let Some(a) = s.active.get_mut(id) else {
                return Ok(if method == "uploadAbort" {
                    json!({"success":true})
                } else {
                    missing()
                });
            };
            if a.key != session {
                return Ok(missing());
            }
            match method {
                "uploadAbort" => s.abort(id),
                "uploadChunk" => {
                    if v["index"].as_f64() != Some(a.index as f64) {
                        let got = v
                            .get("index")
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| v.to_string())
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
                        s.abort(id);
                        return Ok(upload_error(
                            "upload_size_mismatch",
                            "Uploaded bytes exceed declared file size",
                        ));
                    }
                    if let Err(e) = a.file.write_all(&bytes) {
                        let msg = fs_error(e, "write", &a.part);
                        s.abort(id);
                        return Err(msg);
                    }
                    a.received += bytes.len() as u64;
                    a.index += 1;
                    a.timer.abort();
                    a.timer = expiration(id, ttl, a.index);
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
                    let a = s.active.remove(id).unwrap();
                    a.timer.abort();
                    drop(a.file);
                    let info = fs::rename(&a.part, &a.path)
                        .and_then(|_| fs::symlink_metadata(&a.path))
                        .inspect_err(|_| {
                            let _ = fs::remove_file(&a.part);
                            let _ = fs::remove_file(&a.path);
                        })
                        .map_err(|e| fs_error(e, "rename", &a.part))?;
                    if !info.is_file() || info.len() != a.total {
                        let _ = fs::remove_file(a.path);
                        return Ok(if info.is_file() {
                            upload_error("upload_size_mismatch", "Finalized upload size mismatch")
                        } else {
                            upload_error("upload_conflict", "Upload aborted during finalization")
                        });
                    }
                    s.ids
                        .insert((session.into(), a.path.clone()), (info.dev(), info.ino()));
                    return Ok(json!({"success":true,"path":a.path}));
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(json!({"success":true}))
}
pub(super) fn register(manager: &RpcHandlerManager) {
    EXIT.call_once(|| unsafe {
        nix::libc::atexit(cleanup_exit);
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
            let token = matches!(method, "uploadChunk" | "uploadFinalize").then(|| {
                (
                    key(field(&v, "sessionId")).to_string(),
                    field(&v, "uploadId").to_owned(),
                )
            });
            if let Some(t) = &token {
                if !BUSY.lock().unwrap().insert(t.clone()) {
                    return Ok(Some(upload_error(
                        "upload_conflict",
                        "Upload operation already in progress",
                    )));
                }
            }
            let reply = tokio::task::spawn_blocking(move || {
                let reply = handle(method, v, TTL).unwrap_or_else(error);
                if let Some(t) = token {
                    BUSY.lock().unwrap().remove(&t);
                }
                reply
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok(Some(reply))
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn request(m: &str, k: &str, mut v: Value) -> Value {
        v["sessionId"] = json!(k);
        handle(m, v, TTL).unwrap_or_else(error)
    }
    #[test]
    fn filename_budget() {
        for name in ["a".repeat(300), "😀".repeat(300)] {
            let n = fit(&format!("{name}.png"), 200);
            assert!(n.len() <= 200 && n.ends_with(".png"))
        }
        assert_eq!(fit("x.abc", 4), ".abc");
        assert_eq!(fit("../a b\\c", 200), "__a_b_c");
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
        let meta = path.metadata().unwrap();
        assert!(is_authorized_upload_file(path, k, meta.dev(), meta.ino()));
        assert!(!is_authorized_upload_file(
            path,
            "other",
            meta.dev(),
            meta.ino()
        ));
        assert_eq!(fs::read(path).unwrap(), [1, 2, 3, 4]);
        let replacement = path.with_file_name("replacement");
        fs::write(&replacement, "x").unwrap();
        fs::rename(&replacement, path).unwrap();
        let meta = path.metadata().unwrap();
        assert!(!is_authorized_upload_file(path, k, meta.dev(), meta.ino()));
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
        assert_eq!(chunk(0, "!!!!")["error"], "Chunk content is invalid");
        assert_eq!(chunk(0, "AQI=")["receivedBytes"], 2);
        assert_eq!(chunk(1, "Aw==")["receivedBytes"], 3);
        let finish = request("uploadFinalize", k, req);
        let path = Path::new(finish["path"].as_str().unwrap());
        assert!(path.file_name().unwrap().len() <= 250);
        assert_eq!(fs::read(path).unwrap(), [1, 2, 3]);
        let meta = path.metadata().unwrap();
        assert!(is_authorized_upload_file(path, k, meta.dev(), meta.ino()));
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
