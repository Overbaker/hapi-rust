use super::*;
use futures_util::{
    future::{BoxFuture, Shared},
    FutureExt,
};
use nix::{
    sys::signal::{killpg, Signal},
    unistd::Pid,
};
use regex_lite::Regex;
use std::{
    collections::HashMap,
    env,
    process::Stdio,
    sync::{LazyLock, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::{timeout, Duration, Instant},
};
const ID: &str = "hapi-claude-models-probe";
const ARGS: &[&str] = &[
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--setting-sources",
    "user",
    "--strict-mcp-config",
    "--settings",
    r#"{"disableAllHooks":true}"#,
];
fn string<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v[k].as_str().filter(|s| !s.trim().is_empty())
}
static FAMILY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(Opus|Sonnet|Haiku|Fable)(?:\s+(1M))?$").unwrap());
static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]+(?:-[0-9]+)*?)(?:-[0-9]{8})?(\[1m\])?$").unwrap());
fn display(row: &Value) -> Option<String> {
    let name = string(row, "displayName")?;
    if let (Some(family), Some(resolved)) = (FAMILY.captures(name), string(row, "resolvedModel")) {
        if let Some(version) = resolved
            .strip_prefix(&format!("claude-{}-", family[1].to_lowercase()))
            .and_then(|s| VERSION.captures(s))
        {
            return Some(format!(
                "{} {}{}",
                &family[1],
                version[1].replace('-', "."),
                if family.get(2).is_some() || version.get(2).is_some() {
                    " 1M"
                } else {
                    ""
                }
            ));
        }
        return Some(format!("{name} ({resolved})"));
    }
    Some(name.into())
}
fn parse(line: &str) -> Option<Result<Value, String>> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let r = &v["response"];
    if v["type"] != "control_response" || r["request_id"] != ID {
        return None;
    }
    if r["subtype"] != "success" {
        return Some(Err(string(r, "error")
            .unwrap_or("Claude rejected the model probe request")
            .into()));
    }
    let rows = r["response"]["models"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let effort = rows.iter().any(|r| r.get("supportsEffort").is_some());
    let models: Vec<_> = rows
        .iter()
        .filter_map(|r| {
            let mut m = json!({"value":string(r,"value")?});
            if let Some(d) = display(r) {
                m["displayName"] = json!(d)
            }
            if effort && r["supportsEffort"] != true {
                m["effortLevels"] = json!([])
            } else if let Some(levels) = r["supportedEffortLevels"].as_array() {
                m["effortLevels"] =
                    json!(levels.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            }
            Some(m)
        })
        .collect();
    Some(if models.is_empty() {
        Err("Claude reported no models".into())
    } else {
        Ok(json!({"success":true,"availableModels":models}))
    })
}
fn executable(home: &Path) -> Result<PathBuf, String> {
    resolve_executable(
        home,
        env::var("HAPI_CLAUDE_PATH").ok().as_deref(),
        &env::var_os("PATH").unwrap_or_default(),
    )
}
fn resolve_executable(
    home: &Path,
    configured: Option<&str>,
    path: &std::ffi::OsStr,
) -> Result<PathBuf, String> {
    if let Some(p) = configured.map(str::trim).filter(|p| !p.is_empty()) {
        return Ok(p.into());
    }
    env::split_paths(path)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| home.join(p).join("claude"))
        .find(|p| p.is_file() && nix::unistd::access(p, nix::unistd::AccessFlags::X_OK).is_ok())
        .ok_or_else(|| {
            "Claude Code CLI not found on PATH. Install Claude Code or set HAPI_CLAUDE_PATH.".into()
        })
}
async fn probe(exe: &Path, home: &Path, limit: Duration) -> Result<Value, String> {
    let mut child = Command::new(exe)
        .args(ARGS)
        .current_dir(home)
        .env_remove("BUN_BE_BUN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            format!(
                "spawn {} {:?}",
                exe.display(),
                nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(0))
            )
        })?;
    let pid = child.id().unwrap() as i32;
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stderr = child.stderr.take().unwrap();
    let mut log = tokio::spawn(async move {
        let mut bytes = vec![];
        let _ = stderr.read_to_end(&mut bytes).await;
        String::from_utf8_lossy(&bytes).trim().to_owned()
    });
    let result = timeout(limit, async {
        let request =
            json!({"type":"control_request","request_id":ID,"request":{"subtype":"initialize"}});
        let _ = stdin.write_all(format!("{request}\n").as_bytes()).await;
        while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
            if let Some(r) = parse(&line) {
                return r;
            }
        }
        let code = child
            .wait()
            .await
            .map_err(|e| e.to_string())?
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".into());
        let err = (&mut log).await.unwrap_or_default();
        Err(if err.is_empty() {
            format!("claude exited with code {code} before answering the model probe")
        } else {
            err
        })
    })
    .await
    .unwrap_or_else(|_| Err("Claude model discovery timed out".into()));
    log.abort();
    let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
    if !matches!(
        timeout(Duration::from_millis(300), child.wait()).await,
        Ok(Ok(_))
    ) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        wait_after_kill(pid, child.wait()).await?;
    }
    let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    result
}
async fn wait_after_kill(
    pid: i32,
    wait: impl std::future::Future<Output = io::Result<std::process::ExitStatus>>,
) -> Result<(), String> {
    match timeout(Duration::from_secs(1), wait).await {
        Ok(Ok(_)) => Ok(()),
        _ => Err(format!(
            "Claude model probe could not be stopped (pid {pid})"
        )),
    }
}
type Pending = Shared<BoxFuture<'static, Result<Value, String>>>;
enum Entry {
    Ready(Instant, Value),
    Probing(Pending),
}
static CACHE: LazyLock<Mutex<HashMap<PathBuf, Entry>>> = LazyLock::new(Mutex::default);
async fn cached(exe: PathBuf, home: PathBuf) -> Result<Value, String> {
    let pending = {
        let mut cache = CACHE.lock().unwrap();
        match cache.get(&exe) {
            Some(Entry::Ready(until, result)) if *until > Instant::now() => {
                return Ok(result.clone())
            }
            Some(Entry::Probing(p)) => p.clone(),
            _ => {
                let key = exe.clone();
                let task = tokio::spawn(async move {
                    let result = probe(&key, &home, Duration::from_secs(15)).await;
                    let mut cache = CACHE.lock().unwrap();
                    if let Ok(value) = &result {
                        cache.insert(
                            key,
                            Entry::Ready(Instant::now() + Duration::from_secs(300), value.clone()),
                        );
                    } else {
                        cache.remove(&key);
                    }
                    result
                });
                let pending = async move { task.await.map_err(|e| e.to_string())? }
                    .boxed()
                    .shared();
                cache.insert(exe, Entry::Probing(pending.clone()));
                pending
            }
        }
    };
    pending.await
}
pub(super) fn register(manager: &RpcHandlerManager) {
    manager.register("listClaudeModels", |_| async {
        let home = env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let search_home = home.clone();
        let executable = tokio::task::spawn_blocking(move || executable(&search_home))
            .await
            .map_err(|e| e.to_string())?;
        let result = match executable {
            Ok(p) => cached(p, home).await,
            Err(e) => Err(e),
        };
        Ok(Some(result.unwrap_or_else(error)))
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    fn line(rows: Value) -> String {
        json!({"type":"control_response","response":{"request_id":ID,"subtype":"success","response":{"models":rows}}}).to_string()
    }
    #[test]
    fn family_labels() {
        for (name, id, expected) in [
            ("Opus", "claude-opus-4-1-20250805", "Opus 4.1"),
            ("Sonnet 1M", "claude-sonnet-4-6", "Sonnet 4.6 1M"),
            ("Haiku", "claude-haiku-5-5[1m]", "Haiku 5.5 1M"),
            ("Fable", "claude-fable-5-1", "Fable 5.1"),
            ("Opus", "custom", "Opus (custom)"),
            ("Team deployment", "custom", "Team deployment"),
        ] {
            assert_eq!(
                display(&json!({"displayName":name,"resolvedModel":id})).as_deref(),
                Some(expected)
            )
        }
    }
    #[test]
    fn effort_catalog() {
        let parse_rows = |r| parse(&line(r)).unwrap().unwrap()["availableModels"].clone();
        assert_eq!(
            parse_rows(json!([{"value":"legacy"},{"value":" "},"bad"])),
            json!([{"value":"legacy"}])
        );
        assert_eq!(
            parse_rows(
                json!([{"value":"y","supportsEffort":true},{"value":"z","supportsEffort":true,"supportedEffortLevels":["low",1]},{"value":"x","supportedEffortLevels":["low"]}])
            ),
            json!([{"value":"y"},{"value":"z","effortLevels":["low"]},{"value":"x","effortLevels":[]}])
        );
    }
    #[test]
    fn ignores_unrelated_probe_output() {
        for s in ["welcome", "{}", &line(json!([])).replace(ID, "other")] {
            assert!(parse(s).is_none())
        }
        assert_eq!(
            parse(&line(json!([]))).unwrap().unwrap_err(),
            "Claude reported no models"
        );
        assert_eq!(parse(&json!({"type":"control_response","response":{"request_id":ID,"error":"Not logged in"}}).to_string()).unwrap().unwrap_err(),"Not logged in");
    }
    fn script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap()
    }
    #[tokio::test]
    async fn isolated_probe_and_success_cache() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_owned();
        let exe = home.join("probe");
        let run = || cached(exe.clone(), home.clone());
        assert!(run().await.unwrap_err().ends_with("ENOENT"));
        script(&exe, "echo 'Invalid API key' >&2; exit 1");
        assert_eq!(run().await.unwrap_err(), "Invalid API key");
        script(&exe, &format!("echo '{}'", line(json!([]))));
        assert_eq!(run().await.unwrap_err(), "Claude reported no models");
        script(&exe,&format!("echo run >> count\nprintf '%s\\n' \"$@\" > args\nread -r req\nprintf '%s' \"$req\" > request\necho '{}'\nexec sleep 60",line(json!([{"value":"opus"}]))));
        let (a, b) = tokio::join!(run(), run());
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(
            run().await.unwrap()["availableModels"],
            json!([{"value":"opus"}])
        );
        assert_eq!(fs::read_to_string(home.join("count")).unwrap(), "run\n");
        assert_eq!(
            fs::read_to_string(home.join("args"))
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ARGS
        );
        let req: Value = serde_json::from_slice(&fs::read(home.join("request")).unwrap()).unwrap();
        assert_eq!(
            req,
            json!({"type":"control_request","request_id":ID,"request":{"subtype":"initialize"}})
        );
        if let Some(Entry::Ready(until, _)) = CACHE.lock().unwrap().get_mut(&exe) {
            assert!(until.duration_since(Instant::now()) > Duration::from_secs(299));
            *until = Instant::now();
        }
        run().await.unwrap();
        let other = home.join("other");
        fs::hard_link(exe, &other).unwrap();
        cached(other, home.clone()).await.unwrap();
        assert_eq!(
            fs::read_to_string(home.join("count")).unwrap(),
            "run\nrun\nrun\n"
        );
    }
    #[tokio::test]
    async fn timed_out_probe_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("probe");
        script(&exe, "trap '' TERM; echo $$ > pid; exec sleep 60");
        assert_eq!(
            probe(&exe, dir.path(), Duration::from_millis(200))
                .await
                .unwrap_err(),
            "Claude model discovery timed out"
        );
        let pid = fs::read_to_string(dir.path().join("pid")).unwrap();
        assert!(!Path::new("/proc").join(pid.trim()).exists());
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use std::{ffi::OsStr, fs, os::unix::fs::PermissionsExt};
    #[test]
    fn executable_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let local = home.join("claude");
        fs::write(&local, "").unwrap();
        fs::set_permissions(&local, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(resolve_executable(home, Some(" \t"), OsStr::new("::")).is_err());
        assert_eq!(
            resolve_executable(home, Some(" custom  "), OsStr::new("")).unwrap(),
            Path::new("custom")
        );
        fs::create_dir(home.join("bin")).unwrap();
        let path = home.join("bin/claude");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(resolve_executable(home, None, OsStr::new("bin")).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            resolve_executable(home, None, OsStr::new(":bin:")).unwrap(),
            path
        );
    }
    #[tokio::test(start_paused = true)]
    async fn post_kill_wait_is_bounded() {
        let start = Instant::now();
        assert_eq!(
            wait_after_kill(17, std::future::pending())
                .await
                .unwrap_err(),
            "Claude model probe could not be stopped (pid 17)"
        );
        assert_eq!(Instant::now() - start, Duration::from_secs(1));
        assert!(
            wait_after_kill(17, std::future::ready(Err(io::Error::other("wait failed"))))
                .await
                .is_err()
        );
    }
}
