//! `agy --version` probe: the Rust wrapper needs `agy acp` (1.5.0 and newer)
//! and exits 86 below it (TS `legacyStreamJson/agyVersion.ts`).
use std::{
    io,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

pub const AGY_ACP_MIN_VERSION: [u64; 3] = [1, 5, 0];
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
// A background child can hold stdout open after agy exits.
const STDOUT_GRACE_AFTER_EXIT: Duration = Duration::from_millis(500);

/// What a one-shot child wrote before it exited (plus a bounded grace for
/// pipes a background grandchild keeps open).
pub struct Captured {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

fn pump<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: Option<R>,
    buf: Arc<Mutex<Vec<u8>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(mut reader) = reader else {
            return;
        };
        let mut chunk = [0u8; 4096];
        while let Ok(n @ 1..) = reader.read(&mut chunk).await {
            buf.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    })
}

/// Runs `cmd` with piped output; `Ok(None)` when it did not exit within `limit`
/// (it is killed then).
pub async fn capture(
    mut cmd: Command,
    limit: Duration,
    grace: Duration,
) -> io::Result<Option<Captured>> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let (out, err) = (Arc::default(), Arc::default());
    let mut readers = [
        pump(child.stdout.take(), Arc::clone(&out)),
        pump(child.stderr.take(), Arc::clone(&err)),
    ];
    let status = match tokio::time::timeout(limit, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.start_kill();
            readers.iter().for_each(|r| r.abort());
            return Ok(None);
        }
    };
    let _ = tokio::time::timeout(grace, futures_util::future::join_all(readers.iter_mut())).await;
    readers.iter().for_each(|r| r.abort());
    let text = |b: &Arc<Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock().unwrap()).into_owned();
    Ok(Some(Captured {
        code: status.code(),
        stdout: text(&out),
        stderr: text(&err),
    }))
}

/// `<command> --version` stdout; only stdout carries the version.
pub async fn probe(command: &str) -> Result<String, String> {
    probe_within(command, VERSION_PROBE_TIMEOUT).await
}
async fn probe_within(command: &str, limit: Duration) -> Result<String, String> {
    let mut cmd = Command::new(command);
    cmd.arg("--version");
    match capture(cmd, limit, STDOUT_GRACE_AFTER_EXIT).await {
        Err(e) => Err(e.to_string()),
        Ok(None) => Err(format!(
            "agy --version did not answer within {} ms",
            limit.as_millis()
        )),
        Ok(Some(c)) if c.code == Some(0) => Ok(c.stdout),
        Ok(Some(c)) => Err(format!(
            "agy --version exited with code {}",
            c.code.map_or("null".into(), |c| c.to_string())
        )),
    }
}

fn is_word_or_dot(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

/// First `v?MAJOR.MINOR[.PATCH][-pre]` not preceded by a word character or a
/// dot: `(core, prerelease)`.
pub fn parse_version(output: &str) -> Option<([u64; 3], bool)> {
    let re = regex_lite::Regex::new(r"^v?(\d+)\.(\d+)(?:\.(\d+))?(-[0-9A-Za-z.-]+)?").unwrap();
    let mut previous: Option<char> = None;
    for (i, c) in output.char_indices() {
        if !previous.is_some_and(is_word_or_dot) {
            if let Some(m) = re.captures(&output[i..]) {
                let n = |k: usize| {
                    m.get(k)
                        .map_or(0, |v| v.as_str().parse().unwrap_or(u64::MAX))
                };
                return Some(([n(1), n(2), n(3)], m.get(4).is_some()));
            }
        }
        previous = Some(c);
    }
    None
}

fn is_before(version: ([u64; 3], bool), floor: [u64; 3]) -> bool {
    match version.0.cmp(&floor) {
        std::cmp::Ordering::Equal => version.1,
        o => o.is_lt(),
    }
}

/// `Ok(reason)` when this agy has `agy acp`, `Err(reason)` otherwise (TS
/// `needsStreamJson` reasons).
pub fn supports_acp(output: &str) -> Result<String, String> {
    let Some(version) = parse_version(output) else {
        let head: String = output.trim().chars().take(80).collect();
        return Err(format!("could not read the agy version from \"{head}\""));
    };
    let digits = regex_lite::Regex::new(r"\d+\.\d+").unwrap();
    let label = output
        .split_whitespace()
        .find(|w| digits.is_match(w))
        .map_or_else(|| version.0.map(|n| n.to_string()).join("."), str::to_owned);
    if is_before(version, AGY_ACP_MIN_VERSION) {
        let floor = AGY_ACP_MIN_VERSION.map(|n| n.to_string()).join(".");
        return Err(format!("agy {label} predates agy acp ({floor})"));
    }
    Ok(format!("agy {label} supports agy acp"))
}

/// Probe and decide in one step.
pub async fn check(command: &str) -> Result<String, String> {
    match probe(command).await {
        Ok(output) => supports_acp(&output),
        Err(e) => Err(format!("could not read the agy version ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decides_acp_support() {
        for (output, acp) in [
            ("1.4.12\n", false),
            ("1.5.0\n", true),
            ("1.5.2\n", true),
            ("agy version 2.0.0\n", true),
            ("1.5.0-rc.1\n", false),
            ("1.5.1-beta\n", true),
            ("2.0\n", true),
            ("1.4\n", false),
            ("v1.4.12\n", false),
            ("agy v1.5.2\n", true),
        ] {
            assert_eq!(supports_acp(output).is_ok(), acp, "{output:?}");
        }
        assert_eq!(
            supports_acp("agy v1.5.2\n").unwrap(),
            "agy v1.5.2 supports agy acp"
        );
        assert_eq!(
            supports_acp("1.4.12").unwrap_err(),
            "agy 1.4.12 predates agy acp (1.5.0)"
        );
        assert_eq!(
            supports_acp("dev build").unwrap_err(),
            "could not read the agy version from \"dev build\""
        );
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("1.5.2"), Some(([1, 5, 2], false)));
        assert_eq!(parse_version("2.0"), Some(([2, 0, 0], false)));
        assert_eq!(parse_version("1.5.0-rc.1"), Some(([1, 5, 0], true)));
        assert_eq!(parse_version("v1.4.12"), Some(([1, 4, 12], false)));
        assert_eq!(parse_version("none"), None);
        assert_eq!(
            parse_version("build.9.9 and 1.6.0"),
            Some(([1, 6, 0], false))
        );
        assert_eq!(parse_version("x1.2 1.5"), Some(([1, 5, 0], false)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probes_the_binary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |body: &str| {
            let path = dir.path().join("agy");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        };
        let missing = dir.path().join("missing-agy");
        assert!(check(&missing.to_string_lossy()).await.is_err());
        assert_eq!(
            probe_within(&script("sleep 5"), Duration::from_millis(200)).await,
            Err("agy --version did not answer within 200 ms".into())
        );
        let started = std::time::Instant::now();
        let out = probe_within(&script("(sleep 3 &); echo 1.5.2"), Duration::from_secs(5)).await;
        assert_eq!(out.unwrap().trim(), "1.5.2");
        assert!(started.elapsed() < Duration::from_millis(2500));
        assert_eq!(
            probe(&script("echo 1.5.2; echo 9.9.9 >&2; exit 3")).await,
            Err("agy --version exited with code 3".into())
        );
    }
}
