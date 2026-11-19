use super::*;
use tokio::{
    process::Command,
    time::{timeout, Duration},
};
async fn capture(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    bytes: &mut Vec<u8>,
    stream: &str,
) -> Result<(), String> {
    use tokio::io::AsyncReadExt;
    let mut buf = [0; 8192];
    loop {
        let n = pipe.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        let allowed = (1024 * 1024 - bytes.len()).min(n);
        bytes.extend_from_slice(&buf[..allowed]);
        if n != allowed {
            return Err(format!("{stream} maxBuffer length exceeded"));
        }
    }
}
async fn exec(mut command: Command, label: &str, ms: u64) -> Value {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let mut child = match command
        .kill_on_drop(true)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let msg = format!(
                "spawn {program} {:?}",
                nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(0))
            );
            return json!({"success":false,"error":msg,"stdout":"","stderr":msg,"exitCode":1});
        }
    };
    let mut stdout = vec![];
    let mut stderr = vec![];
    let read_stdout = capture(child.stdout.take().unwrap(), &mut stdout, "stdout");
    let read_stderr = capture(child.stderr.take().unwrap(), &mut stderr, "stderr");
    let pid = child.id().unwrap() as i32;
    let wait = async {
        let (status, _, _) = tokio::try_join!(
            async { child.wait().await.map_err(|e| e.to_string()) },
            read_stdout,
            read_stderr
        )?;
        Ok(status)
    };
    let result = if ms == 0 {
        wait.await
    } else {
        timeout(Duration::from_millis(ms), wait)
            .await
            .unwrap_or_else(|_| Err("Command timed out".to_owned()))
    };
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    let (msg, code) = match result {
        Ok(status) if status.success() => {
            return json!({"success":true,"stdout":stdout,"stderr":stderr,"exitCode":0})
        }
        Ok(status) => (
            format!("Command failed: {label}\n{stderr}"),
            status.code().unwrap_or(1),
        ),
        Err(msg) => {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.wait().await;
            let code = if msg == "Command timed out" { -1 } else { 1 };
            (msg, code)
        }
    };
    json!({"success":false,"error":msg,"stdout":stdout,"stderr":if stderr.is_empty() && code!=-1{msg.as_str()}else{stderr.as_ref()},"exitCode":code})
}
pub(super) fn register(manager: &RpcHandlerManager, root: &Path) {
    let root = root.to_owned();
    manager.register("bash", move |v| {
        let root = root.clone();
        async move {
            let label = field(&v, "command");
            let mut c = Command::new("/bin/sh");
            c.args(["-c", label]);
            if let Some(cwd) = v["cwd"].as_str() {
                if let Err(e) = lexical(&root, cwd) {
                    return Ok(Some(error(e)));
                }
                if !cwd.is_empty() {
                    c.current_dir(cwd);
                }
            }
            Ok(Some(
                exec(
                    c,
                    label,
                    v["timeout"].as_u64().filter(|n| *n > 0).unwrap_or(30000),
                )
                .await,
            ))
        }
    });
}
fn search_match(line: &str, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    let line = line.to_lowercase();
    if !query.contains(['?', '*']) {
        return line.contains(&query);
    }
    let input: Vec<_> = line.chars().collect();
    let pattern: Vec<_> = query.chars().collect();
    let (mut i, mut j, mut retry, mut star) = (0, 0, 0, None);
    while i < input.len() {
        if pattern.get(j) == Some(&'?') || pattern.get(j) == Some(&input[i]) {
            i += 1;
            j += 1
        } else if pattern.get(j) == Some(&'*') {
            star = Some(j);
            j += 1;
            retry = i
        } else if let Some(s) = star {
            retry += 1;
            i = retry;
            j = s + 1
        } else {
            return false;
        }
    }
    pattern[j..].iter().all(|c| *c == '*')
}
async fn tool(method: &str, root: &Path, v: &Value) -> Result<Value, String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let args: Vec<String> =
        serde_json::from_value(v["args"].clone()).map_err(|_| "Invalid arguments")?;
    let requested = lexical(root, field(v, "cwd"));
    let rg = method == "ripgrep";
    let search = rg && v["fileSearch"].is_object();
    let cwd = if rg {
        let root = root.to_owned();
        let paths = if search {
            args.iter()
                .position(|s| s == "--")
                .map(|i| args[i + 1..].to_vec())
                .unwrap_or_default()
        } else {
            vec![]
        };
        tokio::task::spawn_blocking(move || {
            let cwd = requested
                .and_then(|p| canonical(&root, &p))
                .map_err(|_| "Invalid working directory".to_owned())?;
            for raw in paths {
                lexical(&cwd, &raw)
                    .and_then(|p| canonical(&cwd, &p))
                    .map_err(|_| "Invalid file search path".to_owned())?;
            }
            Ok::<_, String>(cwd)
        })
        .await
        .map_err(|e| e.to_string())??
    } else {
        requested?
    };
    let runtime = std::env::var_os("HAPI_WRAPPER_RUNTIME_PATH")
        .ok_or("HAPI_WRAPPER_RUNTIME_PATH is required")?;
    let bin = Path::new(&runtime)
        .join("tools/unpacked")
        .join(if rg { "rg" } else { "difft" });
    let mut c = Command::new(&bin);
    c.args(args).kill_on_drop(true);
    if rg {
        c.current_dir(cwd);
    } else if let Some(requested) = v["cwd"].as_str().filter(|p| !p.is_empty()) {
        c.current_dir(requested);
    }
    if !rg {
        c.env("FORCE_COLOR", "1");
    }
    let spawn_err = |e: io::Error| {
        format!(
            "spawn {} {:?}",
            bin.display(),
            nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(0))
        )
    };
    if !search {
        let out = c.output().await.map_err(spawn_err)?;
        return Ok(
            json!({"success":true,"exitCode":out.status.code().unwrap_or(0),"stdout":String::from_utf8_lossy(&out.stdout),"stderr":String::from_utf8_lossy(&out.stderr)}),
        );
    }
    let mut child = c
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(spawn_err)?;
    let mut lines = BufReader::new(child.stdout.take().unwrap()).split(b'\n');
    let mut err = child.stderr.take().unwrap();
    let mut stderr = vec![];
    let mut stdout = String::new();
    let opts = &v["fileSearch"];
    let limit = opts["limit"].as_u64().unwrap_or(u64::MAX).max(1);
    let read = async {
        let mut count = 0;
        while let Some(line) = lines.next_segment().await? {
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\r');
            if search_match(line, field(opts, "query")) {
                stdout.push_str(line);
                stdout.push('\n');
                count += 1;
                if count >= limit {
                    let _ = child.start_kill();
                    break;
                }
            }
        }
        child.wait().await
    };
    let (status, _) =
        tokio::try_join!(read, err.read_to_end(&mut stderr)).map_err(|e| e.to_string())?;
    Ok(
        json!({"success":true,"exitCode":status.code().unwrap_or(0),"stdout":stdout,"stderr":String::from_utf8_lossy(&stderr)}),
    )
}
async fn git(method: &str, root: &Path, v: &Value) -> Result<Value, String> {
    let cwd = v["cwd"].as_str().map(Path::new).unwrap_or(root);
    lexical(root, field(v, "cwd"))?;
    let mut args = if method == "git-status" {
        vec![
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
        ]
    } else {
        let mut args = vec!["diff"];
        if v["staged"] == true {
            args.push("--cached")
        }
        args
    };
    match method {
        "git-diff-numstat" => args.push("--numstat"),
        "git-diff-file" => {
            let path = field(v, "filePath");
            lexical(root, path)?;
            args.extend(["--no-ext-diff", "--", path])
        }
        _ => {}
    }
    let mut c = Command::new("git");
    c.args(&args).current_dir(cwd);
    Ok(exec(
        c,
        &format!("git {}", args.join(" ")),
        v["timeout"].as_u64().unwrap_or(10000),
    )
    .await)
}
pub(super) fn register_tools(manager: &RpcHandlerManager, root: &Path, methods: &[&'static str]) {
    for &method in methods {
        let root = root.to_owned();
        manager.register(method, move |v| {
            let root = root.clone();
            async move {
                let result = if method.starts_with("git-") {
                    git(method, &root, &v).await
                } else {
                    tool(method, &root, &v).await
                };
                Ok(Some(result.unwrap_or_else(error)))
            }
        });
    }
}
