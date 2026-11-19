use hapi_wrapper_core::lifecycle::start_marker::{alive, start_marker};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    num::NonZeroU32,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub thread_id: String,
    pub namespace: String,
    pub active: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Kind {
    #[serde(rename = "attached")]
    Attached,
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub id: String,
    pub pid: NonZeroU32,
    pub marker: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub kind: Option<Kind>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub native_endpoint: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub attached_at: Option<f64>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub supersedes: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub server_pid: Option<NonZeroU32>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub server_marker: Option<String>,
    pub endpoint: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub token: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub codex_home: String,
    pub hub: String,
    pub auth_hash: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub pending_creations: Option<Vec<String>>,
    pub sessions: IndexMap<String, Binding>,
}

pub fn auth_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn owner_directory(home: &Path) -> PathBuf {
    home.join("hapi-runtime-owners")
}
pub(super) fn mkdir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}
pub fn save_runtime(record: &Record, hapi_home: &Path) -> io::Result<()> {
    save_with_sync(record, hapi_home, fs::File::sync_all)
}
fn save_with_sync(
    record: &Record,
    hapi_home: &Path,
    mut sync: impl FnMut(&fs::File) -> io::Result<()>,
) -> io::Result<()> {
    if Path::new(&record.id).file_name().and_then(|p| p.to_str()) != Some(&record.id) {
        return Err(io::Error::other("Invalid Codex runtime id"));
    }
    let bytes = serde_json::to_vec(record)?;
    for dir in [
        owner_directory(Path::new(&record.codex_home)),
        hapi_home.join("codex-runtimes"),
    ] {
        mkdir(&dir)?;
        let target = dir.join(format!("{}.json", record.id));
        atomic_write(&target, &bytes, &mut sync)?;
    }
    Ok(())
}
pub(super) fn atomic_write(
    target: &Path,
    bytes: &[u8],
    mut sync: impl FnMut(&fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let temp = target.with_added_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    if let Err(e) = f
        .write_all(bytes)
        .and_then(|_| sync(&f))
        .and_then(|_| fs::rename(&temp, target))
    {
        let _ = fs::remove_file(temp);
        return Err(e);
    }
    sync(&fs::File::open(
        target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?)
}
fn read_record(path: &Path) -> io::Result<Record> {
    let r: Record = serde_json::from_slice(&fs::read(path)?)?;
    if r.kind.is_some()
        && (r.server_pid.is_some()
            || r.server_marker.as_deref().is_some_and(|s| !s.is_empty())
            || !r.command.is_empty()
            || !r.args.is_empty())
    {
        return Err(io::Error::other(
            "An attached worker must not record an owned server or executable",
        ));
    }
    Ok(r)
}
pub(super) fn read_records(dir: &Path, strict: bool) -> io::Result<Vec<Record>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if !strict || e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut records = vec![];
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) if e.file_name().as_encoded_bytes().ends_with(b".json") => paths.push(e.path()),
            Err(e) if strict => return Err(e),
            _ => {}
        }
    }
    paths.sort();
    for path in paths {
        match read_record(&path){Ok(r)=>records.push(r),Err(_) if strict=>return Err(io::Error::other(format!("Cannot verify Codex ownership record {}. Inspect it and its processes before recovery.",path.display()))),_=>{}}
    }
    Ok(records)
}
pub fn read_runtimes(hapi_home: &Path, strict: bool) -> io::Result<Vec<Record>> {
    read_records(&hapi_home.join("codex-runtimes"), strict)
}
pub async fn runtime_alive(r: &Record) -> bool {
    start_marker(r.pid.get()).await.as_deref() == Some(&r.marker)
}
#[derive(Copy, Clone, Default)]
pub enum Probe<'a> {
    #[default]
    System,
    Snapshot(&'a HashMap<u32, Option<String>>),
    Unavailable,
}
impl Probe<'_> {
    pub(super) async fn may_be_alive(self, pid: NonZeroU32, marker: Option<&str>) -> bool {
        let actual = match self {
            Self::Unavailable => return true,
            Self::Snapshot(s) => match s.get(&pid.get()) {
                Some(v) => v.clone(),
                None => return false,
            },
            Self::System => {
                if !alive(pid.get()) {
                    return false;
                }
                start_marker(pid.get()).await
            }
        };
        actual.as_deref().is_none_or(str::is_empty)
            || marker.is_none_or(str::is_empty)
            || actual.as_deref() == marker
    }
}
pub async fn runtime_may_be_alive(r: &Record, p: Probe<'_>) -> bool {
    if p.may_be_alive(r.pid, Some(&r.marker)).await {
        return true;
    }
    if let Some(pid) = r.server_pid.filter(|_| r.kind.is_none()) {
        return p.may_be_alive(pid, r.server_marker.as_deref()).await;
    }
    false
}
pub async fn find_runtime(
    home: &Path,
    hub: &str,
    token: &str,
    id: &str,
) -> io::Result<Option<Record>> {
    let hash = auth_hash(token);
    for r in read_runtimes(home, false)? {
        if r.hub == hub
            && r.auth_hash == hash
            && r.sessions.get(id).is_some_and(|s| s.active)
            && runtime_alive(&r).await
        {
            return Ok(Some(r));
        }
    }
    Ok(None)
}

#[cfg(test)]
pub(super) fn fixture(home: &Path) -> Record {
    serde_json::from_value(serde_json::json!({"id":"owner","pid":1111,"marker":"wrapper","serverPid":2222,"serverMarker":"engine","endpoint":"unix://private","command":"codex","args":[],"codexHome":home,"hub":"hub","authHash":auth_hash("token"),"sessions":{"sid":{"threadId":"thread","namespace":"ns","active":true}}})).unwrap()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn snapshots() {
        let r = fixture(Path::new("/tmp"));
        for (rows, want) in [
            (vec![], false),
            (vec![(1111, Some("wrapper"))], true),
            (vec![(2222, Some("engine"))], true),
            (vec![(1111, None)], true),
            (vec![(1111, Some("new")), (2222, Some("other"))], false),
        ] {
            let s = rows
                .into_iter()
                .map(|(pid, m)| (pid, m.map(str::to_owned)))
                .collect();
            assert_eq!(runtime_may_be_alive(&r, Probe::Snapshot(&s)).await, want);
        }
        assert!(runtime_may_be_alive(&r, Probe::Unavailable).await);
    }
    #[tokio::test]
    async fn strict_read_and_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        let mut r = fixture(&h.join("native"));
        r.pid = std::process::id().try_into().unwrap();
        r.marker = start_marker(r.pid.get()).await.unwrap();
        save_runtime(&r, h).unwrap();
        assert_eq!(
            find_runtime(h, "hub", "token", "sid").await.unwrap(),
            Some(r.clone())
        );
        assert!(find_runtime(h, "hub", "other", "sid")
            .await
            .unwrap()
            .is_none());
        let path = h.join("codex-runtimes/broken.json");
        fs::write(&path, "{").unwrap();
        assert_eq!(read_runtimes(h, false).unwrap(), vec![r]);
        assert!(read_runtimes(h, true)
            .unwrap_err()
            .to_string()
            .contains("Cannot verify"));
    }
}

#[cfg(test)]
#[test]
fn registry_typescript_round_trip() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let r = fixture(&h.join("native"));
    save_runtime(&r, h).unwrap();
    let mut attached = r.clone();
    attached.id = "rust-attached".into();
    attached.kind = Some(Kind::Attached);
    attached.server_pid = None;
    attached.server_marker = None;
    attached.command.clear();
    attached.args.clear();
    attached.native_endpoint = Some("unix:///attached.sock".into());
    attached.attached_at = Some(1234.);
    attached.token = Some("attached-token".into());
    attached.supersedes = Some("old".into());
    attached.pending_creations = Some(vec!["reservation".into()]);
    save_runtime(&attached, h).unwrap();
    let status = std::process::Command::new("bun")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/flavors/codex/registry_compat.ts"
        ))
        .env("HAPI_HOME", h)
        .env("CLI_API_TOKEN", "token")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let owners = owner_directory(Path::new(&r.codex_home));
    let hub = h.join("codex-runtimes");
    assert_eq!(
        read_records(&owners, true).unwrap(),
        read_records(&hub, true).unwrap()
    );
    for dir in [owners, hub] {
        assert_eq!(dir.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        for id in ["owner", "rust-attached", "ts"] {
            assert_eq!(
                dir.join(format!("{id}.json"))
                    .metadata()
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let records = read_records(&dir, true).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0], r);
        assert_eq!(records[1], attached);
        let ts = &records[2];
        assert_eq!(ts.kind, Some(Kind::Attached));
        assert_eq!(ts.token.as_deref(), Some("secret"));
        assert_eq!(ts.attached_at, Some(1234.));
        assert_eq!(ts.sessions, r.sessions);
        assert!(ts.server_pid.is_none());
        assert_eq!(ts.pending_creations.as_ref().unwrap(), &["pending"]);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    Owned,
    Attached,
}

async fn preflight(home: &Path, thread: &str, own_id: &str, intent: Intent) -> io::Result<()> {
    for owner in read_records(&owner_directory(home), true)? {
        if owner.id == own_id || Some(owner.codex_home.as_str()) != home.to_str() {
            continue;
        }
        if owner
            .pending_creations
            .as_ref()
            .is_some_and(|p| !p.is_empty())
            && runtime_may_be_alive(&owner, Probe::System).await
        {
            return Err(io::Error::other(format!("Codex runtime {} has an unconfirmed creation. Inspect/stop that runtime before cold resume; its thread ID may be unknown.",owner.id)));
        }
        let attached = owner.kind.is_some();
        let Some((id, binding)) = owner.sessions.iter().find(|(_, s)| {
            s.thread_id == thread && (s.active || attached || intent == Intent::Attached)
        }) else {
            continue;
        };
        if attached && intent == Intent::Owned {
            return Err(io::Error::other("Externally attached thread: use hapi attach, never cold-resume it into an owned runtime"));
        }
        if !attached && intent == Intent::Attached {
            return Err(io::Error::other("Thread has an owned HAPI binding. Fork it in the external desktop before attaching; no automatic migration."));
        }
        if !binding.active {
            continue;
        }
        let orphan = if let Some(pid) = owner.server_pid {
            Probe::System
                .may_be_alive(pid, owner.server_marker.as_deref())
                .await
        } else {
            false
        };
        if Probe::System
            .may_be_alive(owner.pid, Some(&owner.marker))
            .await
            || orphan
        {
            let suffix = if orphan && !runtime_alive(&owner).await {
                " Its wrapper exited; stop the orphaned runtime before recovery."
            } else {
                ""
            };
            return Err(io::Error::other(format!(
                "Thread belongs to another Codex runtime. Use hapi resume {id}.{suffix}"
            )));
        }
    }
    Ok(())
}

pub async fn with_thread_ownership<
    T,
    F: std::future::Future<Output = io::Result<T>>,
    R: std::future::Future<Output = io::Result<T>>,
>(
    home: &Path,
    thread: &str,
    own_id: &str,
    intent: Intent,
    work: impl FnOnce() -> F,
    on_preflight_rejected: impl FnOnce(io::Error) -> R,
) -> io::Result<T> {
    super::ownership_lock::with_lock(&owner_directory(home), async {
        match preflight(home, thread, own_id, intent).await {
            Ok(()) => work().await,
            Err(e) => on_preflight_rejected(e).await,
        }
    })
    .await
}
pub fn find_binding(
    hapi_home: &Path,
    home: &Path,
    hub: &str,
    token: &str,
    thread: &str,
    intent: Intent,
) -> io::Result<Option<String>> {
    let hash = auth_hash(token);
    let attached = intent == Intent::Attached;
    let owners: Vec<_> = read_runtimes(hapi_home, attached)?
        .into_iter()
        .filter(|o| {
            o.kind.is_some() == attached
                && Some(o.codex_home.as_str()) == home.to_str()
                && o.hub == hub
                && o.auth_hash == hash
        })
        .collect();
    let retired: std::collections::HashSet<_> = owners
        .iter()
        .filter(|_| attached)
        .filter_map(|o| o.supersedes.as_ref())
        .collect();
    Ok(owners
        .iter()
        .flat_map(|o| o.sessions.iter())
        .find(|(id, b)| b.thread_id == thread && !retired.contains(id))
        .map(|(id, _)| id.clone()))
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    async fn check(home: &Path, intent: Intent) -> io::Result<()> {
        with_thread_ownership(
            home,
            "thread",
            "new",
            intent,
            || async { Ok(()) },
            |e| async move {
                assert!(owner_directory(home).join("ownership.hapi.lock").is_dir());
                Err(e)
            },
        )
        .await
    }
    #[tokio::test]
    async fn protects_store_across_hapi_homes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex");
        let h = tmp.path().join("hapi");
        let mut r = fixture(&home);
        r.pid = std::process::id().try_into().unwrap();
        r.marker = start_marker(r.pid.get()).await.unwrap();
        r.server_pid = None;
        save_runtime(&r, &h).unwrap();
        assert_eq!(
            check(&home, Intent::Owned).await.unwrap_err().to_string(),
            "Thread belongs to another Codex runtime. Use hapi resume sid."
        );
        r.server_pid = Some(r.pid);
        r.server_marker = Some(r.marker.clone());
        r.marker = "reused".into();
        save_runtime(&r, &h).unwrap();
        assert!(check(&home, Intent::Owned)
            .await
            .unwrap_err()
            .to_string()
            .ends_with("Its wrapper exited; stop the orphaned runtime before recovery."));
        r.pending_creations = Some(vec!["unbound".into()]);
        save_runtime(&r, &h).unwrap();
        assert!(check(&home, Intent::Owned)
            .await
            .unwrap_err()
            .to_string()
            .contains("unconfirmed creation"));
        r.server_marker = Some("reused".into());
        save_runtime(&r, &h).unwrap();
        check(&home, Intent::Owned).await.unwrap();
        fs::write(owner_directory(&home).join("broken.json"), "{").unwrap();
        assert!(check(&home, Intent::Owned)
            .await
            .unwrap_err()
            .to_string()
            .contains("Cannot verify"));
    }
    #[tokio::test]
    async fn attached_bindings_and_retirement() {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        let home = h.join("native");
        let mut r = fixture(&home);
        r.sessions["sid"].active = false;
        save_runtime(&r, h).unwrap();
        assert!(check(&home, Intent::Attached)
            .await
            .unwrap_err()
            .to_string()
            .contains("no automatic migration"));
        r.kind = Some(Kind::Attached);
        r.command.clear();
        r.server_pid = None;
        r.server_marker = None;
        save_runtime(&r, h).unwrap();
        assert!(check(&home, Intent::Owned)
            .await
            .unwrap_err()
            .to_string()
            .contains("use hapi attach"));
        assert!(
            find_binding(h, &home, "hub", "token", "thread", Intent::Owned)
                .unwrap()
                .is_none()
        );
        check(&home, Intent::Attached).await.unwrap();
        r.sessions["sid"].active = true;
        r.pid = std::process::id().try_into().unwrap();
        r.marker = start_marker(r.pid.get()).await.unwrap();
        save_runtime(&r, h).unwrap();
        assert!(check(&home, Intent::Attached)
            .await
            .unwrap_err()
            .to_string()
            .contains("another Codex runtime"));
        let original = r.sessions.shift_remove("sid").unwrap();
        r.sessions.insert("fresh".into(), original);
        r.id = "replacement".into();
        r.supersedes = Some("sid".into());
        save_runtime(&r, h).unwrap();
        assert_eq!(
            find_binding(h, &home, "hub", "token", "thread", Intent::Attached)
                .unwrap()
                .as_deref(),
            Some("fresh")
        );
        assert!(
            find_binding(h, &home, "hub", "other", "thread", Intent::Attached)
                .unwrap()
                .is_none()
        );
        r.server_pid = Some(r.pid);
        save_runtime(&r, h).unwrap();
        assert!(read_runtimes(h, true).is_err());
    }
}

#[cfg(test)]
#[test]
fn rejects_unsafe_ids_and_present_null_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let mut r = fixture(&tmp.path().join("native"));
    for id in ["../outside", "/tmp/outside", "nested/owner", "", ".", ".."] {
        r.id = id.into();
        assert!(save_runtime(&r, tmp.path()).is_err());
    }
    r.id = "owner".into();
    let original = serde_json::to_value(r).unwrap();
    for key in [
        "kind",
        "nativeEndpoint",
        "attachedAt",
        "supersedes",
        "serverPid",
        "serverMarker",
        "token",
        "pendingCreations",
    ] {
        let mut v = original.clone();
        v[key] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Record>(v).is_err(), "{key}");
    }
}

#[cfg(test)]
#[test]
fn syncs_each_file_before_rename_and_its_directory_afterwards() {
    for failed_step in [None, Some(0), Some(1), Some(2), Some(3)] {
        let tmp = tempfile::tempdir().unwrap();
        let r = fixture(&tmp.path().join("native"));
        let dirs = [
            owner_directory(Path::new(&r.codex_home)),
            tmp.path().join("codex-runtimes"),
        ];
        let mut step = 0;
        let result = save_with_sync(&r, tmp.path(), |f| {
            use std::os::unix::fs::MetadataExt;
            let dir = &dirs[step / 2];
            let dir_sync = step % 2 == 1;
            let path = dir.join("owner.json");
            assert_eq!(path.exists(), dir_sync);
            let entry = fs::read_dir(dir)?.next().unwrap()?.path();
            let expected = if dir_sync {
                dir
            } else {
                assert_eq!(entry.extension().unwrap(), "tmp");
                &entry
            };
            assert_eq!(f.metadata()?.ino(), fs::metadata(expected)?.ino());
            step += 1;
            if failed_step == Some(step - 1) {
                return Err(io::Error::other("injected sync failure"));
            }
            f.sync_all()
        });
        assert_eq!(result.is_err(), failed_step.is_some());
        assert_eq!(step, failed_step.map_or(4, |n| n + 1));
        for dir in dirs.iter().filter(|p| p.exists()) {
            assert!(fs::read_dir(dir)
                .unwrap()
                .all(|e| e.unwrap().path().extension().unwrap() == "json"));
        }
    }
}
