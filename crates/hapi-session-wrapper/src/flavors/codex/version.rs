use regex_lite::Regex;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{process::Command, time::timeout};

fn compatible(output: &str) -> bool {
    let pattern = Regex::new(r"codex-cli ([0-9]+)\.([0-9]+)\.([0-9]+)").unwrap();
    pattern.captures(output).is_some_and(|c| {
        !c[1].trim_start_matches('0').is_empty() || c[2].parse::<f64>().unwrap_or_default() >= 154.
    })
}
#[cfg(windows)]
fn executable(path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| ["codex.exe", "codex.cmd", "codex.bat"].map(|name| dir.join(name)))
        .filter_map(|p| std::path::absolute(p).ok())
        .find(|p| p.is_file())
}
#[cfg(unix)]
fn executable(path: &OsStr) -> Option<PathBuf> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("codex"))
        .filter_map(|path| std::path::absolute(path).ok())
        .find(|path| {
            path.is_file()
                && CString::new(path.as_os_str().as_bytes())
                    .is_ok_and(|p| unsafe { libc::access(p.as_ptr(), libc::X_OK) } == 0)
        })
}
pub async fn resolve() -> Result<PathBuf, String> {
    let cmd = executable(&std::env::var_os("PATH").unwrap_or_default())
        .ok_or_else(|| "Cannot find executable codex on PATH".to_string())?;
    check(&cmd).await?;
    Ok(cmd)
}
async fn check(command: &Path) -> Result<(), String> {
    let out = timeout(
        Duration::from_secs(10),
        Command::new(command)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    if out.status.success() && compatible(&String::from_utf8_lossy(&out.stdout)) {
        Ok(())
    } else {
        Err(
            "Shared sessions require Codex >= 0.154.0. Upgrade the Codex executable on PATH."
                .into(),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn minimum_version() {
        for s in ["codex-cli 0.154.0", "codex-cli 1.0.0-beta"] {
            assert!(compatible(s))
        }
        for s in ["0.155.0", "codex-cli 0.153.100", "codex-cli 0.154"] {
            assert!(!compatible(s))
        }
    }
}
