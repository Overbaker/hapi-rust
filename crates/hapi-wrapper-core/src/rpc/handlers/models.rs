use super::*;
use crate::jsjson::decode as decode_json;
use futures_util::{
    future::{BoxFuture, Shared},
    FutureExt,
};
use nix::{
    errno::Errno,
    sys::signal::{killpg, Signal},
    unistd::{access, AccessFlags, Pid},
};
use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    process::Stdio,
    sync::{LazyLock, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::{timeout, Duration, Instant},
};

const ID: &str = "hapi-claude-models-probe";
const CACHE_TTL: Duration = Duration::from_secs(300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
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

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn string<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v[k].as_str()
        .filter(|s| !s.trim_matches(is_js_whitespace).is_empty())
}

fn parse_family(name: &str) -> Option<(&str, bool)> {
    let prefix_len = ["opus", "sonnet", "haiku", "fable"]
        .iter()
        .find_map(|prefix| {
            name.get(..prefix.len())
                .filter(|head| head.eq_ignore_ascii_case(prefix))
                .map(|_| prefix.len())
        })?;
    let (family, rest) = name.split_at(prefix_len);
    if rest.is_empty() {
        return Some((family, false));
    }
    let suffix = rest.trim_start_matches(is_js_whitespace);
    (suffix.len() < rest.len() && suffix.eq_ignore_ascii_case("1m")).then_some((family, true))
}

fn parse_version(rest: &str) -> Option<(String, bool)> {
    let (body, one_m) = match rest.strip_suffix("[1m]") {
        Some(body) => (body, true),
        None => (rest, false),
    };
    let mut parts: Vec<&str> = body.split('-').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    if parts.len() >= 2 && parts.last().is_some_and(|p| p.len() == 8) {
        parts.pop();
    }
    Some((parts.join("."), one_m))
}

fn display(row: &Value) -> Option<String> {
    let name = string(row, "displayName")?;
    if let (Some((family, family_1m)), Some(resolved)) =
        (parse_family(name), string(row, "resolvedModel"))
    {
        if let Some((version, version_1m)) = resolved
            .strip_prefix(&format!("claude-{}-", family.to_ascii_lowercase()))
            .and_then(parse_version)
        {
            return Some(format!(
                "{family} {version}{}",
                if family_1m || version_1m { " 1M" } else { "" }
            ));
        }
        return Some(format!("{name} ({resolved})"));
    }
    Some(name.into())
}

fn parse(line: &str) -> Option<Result<Value, String>> {
    let trimmed = line.trim_matches(is_js_whitespace);
    if !trimmed.starts_with('{') {
        return None;
    }
    let v: Value = decode_json(trimmed.as_bytes()).ok()?;
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
            let mut m = json!({"value": string(r, "value")?});
            if let Some(d) = display(r) {
                m["displayName"] = json!(d);
            }
            if effort && r["supportsEffort"] != true {
                m["effortLevels"] = json!([]);
            } else if let Some(levels) = r["supportedEffortLevels"].as_array() {
                m["effortLevels"] =
                    json!(levels.iter().filter_map(Value::as_str).collect::<Vec<_>>());
            }
            Some(m)
        })
        .collect();
    Some(if models.is_empty() {
        Err("Claude reported no models".into())
    } else {
        Ok(json!({"success": true, "availableModels": models}))
    })
}

fn executable(home: &Path) -> Result<PathBuf, String> {
    resolve_executable(
        home,
        env::var_os("HAPI_CLAUDE_PATH").as_deref(),
        &env::var_os("PATH").unwrap_or_default(),
    )
}

fn resolve_executable(
    home: &Path,
    configured: Option<&OsStr>,
    path: &OsStr,
) -> Result<PathBuf, String> {
    if let Some(raw) = configured {
        match raw.to_str() {
            Some(s) => {
                let trimmed = s.trim_matches(is_js_whitespace);
                if !trimmed.is_empty() {
                    return Ok(trimmed.into());
                }
            }
            None => return Ok(raw.into()),
        }
    }
    env::split_paths(path)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| home.join(p).join("claude"))
        .find(|p| p.is_file() && access(p, AccessFlags::X_OK).is_ok())
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
        .map_err(|e| match e.raw_os_error() {
            Some(code) => format!("spawn {} {:?}", exe.display(), Errno::from_raw(code)),
            None => format!("spawn {}: {e}", exe.display()),
        })?;
    let pid = child.id().unwrap() as i32;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    let mut log = tokio::spawn(async move {
        let mut bytes = vec![];
        let _ = stderr.read_to_end(&mut bytes).await;
        String::from_utf8_lossy(&bytes)
            .trim_matches(is_js_whitespace)
            .to_owned()
    });
    let mut reaped = false;
    let outcome = timeout(limit, async {
        let request =
            json!({"type":"control_request","request_id":ID,"request":{"subtype":"initialize"}});
        let _ = stdin.write_all(format!("{request}\n").as_bytes()).await;
        let mut buf = vec![];
        loop {
            buf.clear();
            let n = stdout
                .read_until(b'\n', &mut buf)
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 || buf.last() != Some(&b'\n') {
                break;
            }
            let line = String::from_utf8_lossy(&buf);
            if let Some(r) = parse(&line) {
                return r;
            }
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        reaped = true;
        let code = status
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
    .await;
    let timed_out = outcome.is_err();
    let result = outcome.unwrap_or_else(|_| Err("Claude model discovery timed out".into()));
    log.abort();
    drop(stdin);
    drop(stdout);
    if !reaped {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
        if matches!(
            timeout(Duration::from_millis(300), child.wait()).await,
            Ok(Ok(_))
        ) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        } else {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
            wait_after_kill(pid, child.wait()).await?;
        }
    } else if timed_out {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
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

struct PendingGuard(Option<PathBuf>);
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(key) = self.0.take() {
            let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(cache.get(&key), Some(Entry::Probing(_))) {
                cache.remove(&key);
            }
        }
    }
}

async fn cached(exe: PathBuf, home: PathBuf) -> Result<Value, String> {
    let pending = {
        let mut cache = CACHE.lock().unwrap();
        match cache.get(&exe) {
            Some(Entry::Ready(until, result)) if *until > Instant::now() => {
                return Ok(result.clone());
            }
            Some(Entry::Probing(p)) => p.clone(),
            _ => {
                let key = exe.clone();
                let task = tokio::spawn(async move {
                    let mut guard = PendingGuard(Some(key.clone()));
                    let result = probe(&key, &home, PROBE_TIMEOUT).await;
                    let mut cache = CACHE.lock().unwrap();
                    if let Ok(value) = &result {
                        guard.0 = None;
                        cache.insert(key, Entry::Ready(Instant::now() + CACHE_TTL, value.clone()));
                    } else {
                        guard.0 = None;
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

pub fn register(manager: &RpcHandlerManager) {
    manager.register("listClaudeModels", |_| async {
        let home = env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let search_home = home.clone();
        let result = match tokio::task::spawn_blocking(move || executable(&search_home)).await {
            Ok(Ok(p)) => cached(p, home).await,
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.to_string()),
        };
        Ok(Some(result.unwrap_or_else(error)))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::kill;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn line(rows: Value) -> String {
        json!({"type":"control_response","response":{"request_id":ID,"subtype":"success","response":{"models":rows}}}).to_string()
    }

    fn script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn family_labels() {
        for (name, id, expected) in [
            ("Opus", "claude-opus-4-1-20250805", "Opus 4.1"),
            ("Opus", "claude-opus-4-6[1m]", "Opus 4.6 1M"),
            ("Sonnet 1M", "claude-sonnet-4-6", "Sonnet 4.6 1M"),
            ("Sonnet 1M", "claude-sonnet-4-6[1m]", "Sonnet 4.6 1M"),
            ("Haiku", "claude-haiku-5-5[1m]", "Haiku 5.5 1M"),
            ("Fable", "claude-fable-5-1", "Fable 5.1"),
            ("opus\u{00a0}1m", "claude-opus-4-6-20250805", "opus 4.6 1M"),
            ("Opus", "claude-opus-20250805", "Opus 20250805"),
            ("Opus", "claude-opus-4-20250805-1", "Opus 4.20250805.1"),
            ("Opus", "custom", "Opus (custom)"),
            ("Opus 1M", "custom", "Opus 1M (custom)"),
            ("Opus 5.5", "claude-opus-5-5", "Opus 5.5"),
            ("Team deployment", "custom", "Team deployment"),
        ] {
            assert_eq!(
                display(&json!({"displayName": name, "resolvedModel": id})).as_deref(),
                Some(expected),
                "display({name:?}, {id:?})"
            );
        }
        assert_eq!(
            display(&json!({"displayName": "Opus"})).as_deref(),
            Some("Opus")
        );
        assert_eq!(display(&json!({"displayName": "   "})), None);
    }

    #[test]
    fn effort_catalog() {
        let parse_rows = |r| parse(&line(r)).unwrap().unwrap()["availableModels"].clone();
        assert_eq!(
            parse_rows(json!([{"value": "legacy"}, {"value": " "}, "bad"])),
            json!([{"value": "legacy"}])
        );
        assert_eq!(
            parse_rows(json!([
                {"value": "y", "supportsEffort": true},
                {"value": "z", "supportsEffort": true, "supportedEffortLevels": ["low", 1]},
                {"value": "x", "supportedEffortLevels": ["low"]},
                {"value": "n", "supportsEffort": null},
                {"value": "f", "supportsEffort": false, "supportedEffortLevels": ["high"]}
            ])),
            json!([
                {"value": "y"},
                {"value": "z", "effortLevels": ["low"]},
                {"value": "x", "effortLevels": []},
                {"value": "n", "effortLevels": []},
                {"value": "f", "effortLevels": []}
            ])
        );
    }

    #[test]
    fn ignores_unrelated_probe_output() {
        for s in [
            "welcome",
            "{}",
            "[]",
            &json!({"type": "system", "subtype": "hook_started"}).to_string(),
            &line(json!([])).replace(ID, "other"),
        ] {
            assert!(parse(s).is_none(), "expected None for {s:?}");
        }
        assert_eq!(
            parse(&line(json!([]))).unwrap().unwrap_err(),
            "Claude reported no models"
        );
        assert_eq!(
            parse(
                &json!({"type": "control_response", "response": {"request_id": ID, "error": "Not logged in"}})
                    .to_string()
            )
            .unwrap()
            .unwrap_err(),
            "Not logged in"
        );
        for err in [json!(null), json!(""), json!("   "), json!(42)] {
            assert_eq!(
                parse(
                    &json!({"type": "control_response", "response": {"request_id": ID, "subtype": "error", "error": err}})
                        .to_string()
                )
                .unwrap()
                .unwrap_err(),
                "Claude rejected the model probe request"
            );
        }
        let lone_surrogate = format!(
            r#"{{"type":"control_response","response":{{"request_id":"{ID}","subtype":"success","response":{{"models":[{{"value":"opus","displayName":"Opus \ud83d","description":"\ude00"}}]}}}}"#,
        ) + "}";
        assert_eq!(
            parse(&lone_surrogate).unwrap().unwrap()["availableModels"],
            json!([{"value": "opus", "displayName": "Opus \u{fffd}"}])
        );
    }

    #[test]
    fn parse_matches_typescript_claude_models_probe() {
        let cases: Vec<String> = vec![
            "Welcome to Claude Code".into(),
            "{}".into(),
            "[]".into(),
            json!({"type": "system", "subtype": "hook_started"}).to_string(),
            line(json!([{"value": "opus"}])).replace(ID, "someone-else"),
            line(json!([])),
            line(json!([
                {"value": "opus", "displayName": "Opus", "resolvedModel": "claude-opus-5-5"},
                {"value": "sonnet", "displayName": "Sonnet", "resolvedModel": "claude-sonnet-5-5"},
                {"value": "haiku", "displayName": "Haiku", "resolvedModel": "claude-haiku-5-5"},
                {"value": "fable", "displayName": "Fable", "resolvedModel": "claude-fable-5-1"},
                {"value": "opus-snap", "displayName": "Opus", "resolvedModel": "claude-opus-4-1-20250805"},
                {"value": "opus-1m", "displayName": "Opus", "resolvedModel": "claude-opus-4-6[1m]"},
                {"value": "opus-snap-1m", "displayName": "Opus", "resolvedModel": "claude-opus-4-6-20250805[1m]"},
                {"value": "sonnet-1m", "displayName": "Sonnet 1M", "resolvedModel": "claude-sonnet-4-6"},
                {"value": "sonnet-both", "displayName": "Sonnet 1M", "resolvedModel": "claude-sonnet-4-6[1m]"},
                {"value": "opus-nbsp", "displayName": "opus\u{00a0}1m", "resolvedModel": "claude-opus-4-6"},
                {"value": "opus-date-only", "displayName": "Opus", "resolvedModel": "claude-opus-20250805"},
                {"value": "opus-two-dates", "displayName": "Opus", "resolvedModel": "claude-opus-4-20250805-20250806"},
                {"value": "opus-nine", "displayName": "Opus", "resolvedModel": "claude-opus-4-123456789"},
                {"value": "opus-upper-1m", "displayName": "Opus", "resolvedModel": "claude-opus-4-6[1M]"},
                {"value": "opus-custom", "displayName": "Opus", "resolvedModel": "custom-opus"},
                {"value": "opus-1m-custom", "displayName": "Opus 1M", "resolvedModel": "custom-opus"},
                {"value": "opus-versioned", "displayName": "Opus 5.5", "resolvedModel": "claude-opus-5-5"},
                {"value": "team", "displayName": "Team deployment", "resolvedModel": "custom-opus"},
                {"value": "no-resolved", "displayName": "Opus"},
                {"value": "padded", "displayName": " Opus ", "resolvedModel": "claude-opus-5-5"},
            ])),
            line(json!([
                {"value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus 5.5", "description": "For complex work", "supportsEffort": true, "supportedEffortLevels": ["low", "high"], "supportsFastMode": true},
                {"value": "haiku", "resolvedModel": "claude-haiku-4-5", "displayName": "Haiku 4.5"},
                {"value": "legacy", "supportsEffort": null},
                {"value": "false-effort", "supportsEffort": false, "supportedEffortLevels": ["low"]},
                {"value": "open-effort", "supportsEffort": true},
                {"value": ""},
                {"value": "   "},
                {"displayName": "No value"},
                "opus",
                {"value": "sonnet", "displayName": 42, "supportsEffort": true, "supportedEffortLevels": ["low", 7]},
            ])),
            line(json!([
                {"value": "opus", "supportedEffortLevels": ["low", "high"]},
                {"value": "sonnet"}
            ])),
            line(json!([
                {"supportsEffort": false},
                {"value": "opus"}
            ])),
            format!("\u{feff}{}", line(json!([{"value": "bom"}]))),
            format!(
                r#"{{"type":"control_response","response":{{"request_id":"{ID}","subtype":"success","response":{{"models":[{{"value":"surr","description":"\ud83d"}}]}}}}"#,
            ) + "}",
            json!({"type": "control_response", "response": {"subtype": "error", "request_id": ID, "error": "Not logged in"}}).to_string(),
            json!({"type": "control_response", "response": {"subtype": "error", "request_id": ID, "error": "   "}}).to_string(),
            json!({"type": "control_response", "response": {"request_id": ID}}).to_string(),
        ];
        let script = format!(
            "import {{ parseClaudeModelsProbeLine }} from './src/modules/common/claudeModels.ts';\
             const cases = {};\
             console.log(JSON.stringify(cases.map((line) => {{\
               const r = parseClaudeModelsProbeLine(line);\
               if (r === null) return null;\
               if (r.kind === 'error') return {{ ok: false, error: r.error }};\
               if (r.models.length === 0) return {{ ok: false, error: 'Claude reported no models' }};\
               return {{ ok: true, value: {{ success: true, availableModels: r.models }} }};\
             }})));",
            serde_json::to_string(&cases).unwrap()
        );
        let output = std::process::Command::new("bun")
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
            .args(["-e", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        let actual: Vec<Value> = cases
            .iter()
            .map(|c| match parse(c) {
                None => Value::Null,
                Some(Ok(value)) => json!({"ok": true, "value": value}),
                Some(Err(error)) => json!({"ok": false, "error": error}),
            })
            .collect();
        assert_eq!(actual, expected);
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
        script(&exe, "exit 7");
        assert_eq!(
            run().await.unwrap_err(),
            "claude exited with code 7 before answering the model probe"
        );
        script(&exe, "kill -9 $$");
        assert_eq!(
            run().await.unwrap_err(),
            "claude exited with code unknown before answering the model probe"
        );
        script(&exe, "exec <&-\nexit 3");
        assert_eq!(
            run().await.unwrap_err(),
            "claude exited with code 3 before answering the model probe"
        );
        script(&exe, &format!("echo '{}'", line(json!([]))));
        assert_eq!(run().await.unwrap_err(), "Claude reported no models");
        script(
            &exe,
            &format!(
                "echo run >> count\nprintf '%s\\n' \"$@\" > args\npwd -P > cwd\nread -r req\nprintf '%s' \"$req\" > request\nprintf '\\377\\376 non-utf8 banner\\n'\necho '{}'\nexec sleep 60",
                line(json!([{"value": "opus"}]))
            ),
        );
        let before = Instant::now();
        let (a, b) = tokio::join!(run(), run());
        let after = Instant::now();
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(
            run().await.unwrap()["availableModels"],
            json!([{"value": "opus"}])
        );
        assert_eq!(fs::read_to_string(home.join("count")).unwrap(), "run\n");
        assert_eq!(
            fs::read_to_string(home.join("args"))
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ARGS
        );
        assert_eq!(
            fs::read_to_string(home.join("cwd")).unwrap().trim(),
            home.canonicalize().unwrap().to_str().unwrap()
        );
        let req: Value = serde_json::from_slice(&fs::read(home.join("request")).unwrap()).unwrap();
        assert_eq!(
            req,
            json!({"type":"control_request","request_id":ID,"request":{"subtype":"initialize"}})
        );
        if let Some(Entry::Ready(until, _)) = CACHE.lock().unwrap().get_mut(&exe) {
            assert!(*until >= before + CACHE_TTL && *until <= after + CACHE_TTL);
            *until = Instant::now();
        }
        run().await.unwrap();
        let other = home.join("other");
        fs::hard_link(&exe, &other).unwrap();
        cached(other, home.clone()).await.unwrap();
        assert_eq!(
            fs::read_to_string(home.join("count")).unwrap(),
            "run\nrun\nrun\n"
        );
    }

    #[tokio::test]
    async fn ignores_unterminated_final_stdout_line() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("probe");
        script(
            &exe,
            &format!("printf '%s' '{}'\nexit 2", line(json!([{"value": "opus"}]))),
        );
        assert_eq!(
            probe(&exe, dir.path(), Duration::from_secs(5))
                .await
                .unwrap_err(),
            "claude exited with code 2 before answering the model probe"
        );
    }

    #[tokio::test]
    async fn timed_out_probe_and_grandchild_are_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("probe");
        script(
            &exe,
            "trap '' TERM\nexec sleep 60 &\necho $! > gpid\necho $$ > pid\nexec sleep 60",
        );
        assert_eq!(
            probe(&exe, dir.path(), Duration::from_millis(500))
                .await
                .unwrap_err(),
            "Claude model discovery timed out"
        );
        let pid: i32 = fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gpid: i32 = fs::read_to_string(dir.path().join("gpid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(pid, gpid);
        assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH));
        let deadline = Instant::now() + Duration::from_secs(2);
        while kill(Pid::from_raw(gpid), None) != Err(Errno::ESRCH) {
            assert!(Instant::now() < deadline, "grandchild {gpid} still alive");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn answered_probe_stops_grandchild_ignoring_term() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("probe");
        nix::unistd::mkfifo(&dir.path().join("ready"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        script(
            &exe,
            &format!(
                "(trap '' TERM; echo ok > ready; exec sleep 60) &\necho $! > gpid\nread -r _ < ready\necho '{}'\nexec sleep 60",
                line(json!([{"value": "opus"}]))
            ),
        );
        let reply = probe(&exe, dir.path(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(reply["availableModels"], json!([{"value": "opus"}]));
        let gpid: i32 = fs::read_to_string(dir.path().join("gpid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while kill(Pid::from_raw(gpid), None) != Err(Errno::ESRCH) {
            assert!(Instant::now() < deadline, "grandchild {gpid} still alive");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn executable_lookup() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let local = home.join("claude");
        fs::write(&local, "").unwrap();
        fs::set_permissions(&local, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(resolve_executable(home, Some(OsStr::new(" \t")), OsStr::new("::")).is_err());
        assert_eq!(
            resolve_executable(home, Some(OsStr::new(" custom  ")), OsStr::new("")).unwrap(),
            Path::new("custom")
        );
        fs::create_dir(home.join("bin")).unwrap();
        fs::create_dir(home.join("dir_claude")).unwrap();
        fs::create_dir(home.join("dir_claude/claude")).unwrap();
        fs::create_dir(home.join("links")).unwrap();
        symlink(home.join("absent"), home.join("links/claude")).unwrap();
        let path = home.join("bin/claude");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(resolve_executable(home, None, OsStr::new("dir_claude:links:bin")).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            resolve_executable(home, None, OsStr::new(":dir_claude:links:bin:")).unwrap(),
            path
        );
        fs::create_dir(home.join("symbin")).unwrap();
        let link = home.join("symbin/claude");
        symlink(&path, &link).unwrap();
        assert_eq!(
            resolve_executable(home, None, OsStr::new("symbin")).unwrap(),
            link
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

    #[tokio::test]
    async fn rpc_handler_end_to_end_and_env_isolation() {
        let marker = "HAPI_MODELS_HANDLER_TEST_MODE";
        if let Ok(mode) = env::var(marker) {
            let rpc = RpcHandlerManager::new("s");
            register(&rpc);
            let call = |m: &str| {
                let req = json!({"method": format!("s:{m}"), "params": "{}"});
                let rpc = &rpc;
                async move {
                    serde_json::from_str::<Value>(&rpc.handle_request(&req).await.unwrap()).unwrap()
                }
            };
            match mode.as_str() {
                "ok" => {
                    let reply = call("listClaudeModels").await;
                    assert_eq!(
                        reply,
                        json!({"success": true, "availableModels": [{"value": "opus", "displayName": "Opus 5.5"}]})
                    );
                    assert_eq!(
                        call("listCodexModels").await,
                        json!({"error": "Method not found"})
                    );
                }
                "err" => {
                    assert_eq!(call("listClaudeModels").await, error("Not logged in"));
                }
                "missing" => {
                    assert_eq!(
                        call("listClaudeModels").await,
                        error("Claude Code CLI not found on PATH. Install Claude Code or set HAPI_CLAUDE_PATH.")
                    );
                }
                _ => panic!("unknown mode {mode}"),
            }
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let home = base.join("home");
        fs::create_dir(&home).unwrap();
        let ok_script = base.join("claude-ok");
        script(
            &ok_script,
            &format!(
                "[ -z \"${{BUN_BE_BUN+x}}\" ] || {{ echo 'BUN_BE_BUN leaked' >&2; exit 9; }}\necho '{}'\nexec sleep 60",
                line(json!([{"value": "opus", "displayName": "Opus", "resolvedModel": "claude-opus-5-5"}]))
            ),
        );
        let err_script = base.join("claude-err");
        script(&err_script, "echo 'Not logged in' >&2\nexit 1");
        let test_bin = env::current_exe().unwrap();
        let run_case = |mode: &str, claude_path: Option<&Path>| {
            let mut cmd = std::process::Command::new(&test_bin);
            cmd.args([
                "--exact",
                "rpc::handlers::models::tests::rpc_handler_end_to_end_and_env_isolation",
                "--nocapture",
            ])
            .env(marker, mode)
            .env("HOME", &home)
            .env("BUN_BE_BUN", "1");
            if let Some(p) = claude_path {
                cmd.env("HAPI_CLAUDE_PATH", p);
            } else {
                cmd.env("PATH", "").env_remove("HAPI_CLAUDE_PATH");
            }
            let out = cmd.output().unwrap();
            assert!(
                out.status.success(),
                "mode={mode}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_case("ok", Some(&ok_script));
        run_case("err", Some(&err_script));
        run_case("missing", None);
    }
}
