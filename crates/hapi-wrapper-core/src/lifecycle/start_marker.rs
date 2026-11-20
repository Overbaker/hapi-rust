#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    use nix::{sys::signal::kill, unistd::Pid};
    i32::try_from(pid).is_ok_and(|id| id > 0 && kill(Pid::from_raw(id), None).is_ok())
}

#[cfg(unix)]
pub async fn start_marker(pid: u32) -> Option<String> {
    if !alive(pid) {
        return None;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let marker = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !marker.is_empty()).then_some(marker)
}

/// Format a Windows `FILETIME` (100-nanosecond ticks since 1601-01-01 UTC) to
/// match the TypeScript `getProcessStartMarker` format on Windows:
/// CIM `Win32_Process.CreationDate` truncates `FILETIME` to microseconds and
/// `.ToUniversalTime().ToString('o')` prints a trailing `0Z` for the 7th
/// fractional digit (see `cli/src/utils/process.ts` and `windowsProcessStop.ts`).
#[cfg(any(windows, test))]
fn format_filetime_marker(ticks_100ns: u64) -> Option<String> {
    const WINDOWS_TO_UNIX_EPOCH_SECS: u64 = 11_644_473_600;
    if ticks_100ns == 0 {
        return None;
    }
    let total_us = ticks_100ns / 10;
    let secs_since_1601 = total_us / 1_000_000;
    let micros = total_us % 1_000_000;
    let secs_since_unix = secs_since_1601.checked_sub(WINDOWS_TO_UNIX_EPOCH_SECS)?;
    let days = (secs_since_unix / 86_400) as i64;
    let sod = secs_since_unix % 86_400;
    let hour = sod / 3600;
    let minute = (sod % 3600) / 60;
    let second = sod % 60;

    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);

    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}0Z"
    ))
}

#[cfg(windows)]
mod win {
    use super::format_filetime_marker;
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, GetLastError, ERROR_ACCESS_DENIED, FILETIME, HANDLE, STILL_ACTIVE,
            WAIT_TIMEOUT,
        },
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, WaitForSingleObject,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        },
    };

    struct ProcessHandle {
        handle: HANDLE,
        can_sync: bool,
    }

    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe {
                    CloseHandle(self.handle);
                }
            }
        }
    }

    enum OpenOutcome {
        Handle(ProcessHandle),
        AccessDenied,
        NotFound,
    }

    fn open_for_query(pid: u32) -> OpenOutcome {
        if pid == 0 {
            return OpenOutcome::NotFound;
        }
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if !handle.is_null() {
            return OpenOutcome::Handle(ProcessHandle {
                handle,
                can_sync: true,
            });
        }
        let err = unsafe { GetLastError() };
        if err == ERROR_ACCESS_DENIED {
            let query_only = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if !query_only.is_null() {
                return OpenOutcome::Handle(ProcessHandle {
                    handle: query_only,
                    can_sync: false,
                });
            }
            if unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
                return OpenOutcome::AccessDenied;
            }
        }
        OpenOutcome::NotFound
    }

    fn handle_is_alive(proc: &ProcessHandle) -> bool {
        if proc.can_sync {
            return unsafe { WaitForSingleObject(proc.handle, 0) } == WAIT_TIMEOUT;
        }
        let mut exit_code: u32 = 0;
        unsafe {
            GetExitCodeProcess(proc.handle, &mut exit_code) != 0 && exit_code == STILL_ACTIVE as u32
        }
    }

    pub fn alive(pid: u32) -> bool {
        match open_for_query(pid) {
            OpenOutcome::Handle(proc) => handle_is_alive(&proc),
            OpenOutcome::AccessDenied => true,
            OpenOutcome::NotFound => false,
        }
    }

    pub async fn start_marker(pid: u32) -> Option<String> {
        let OpenOutcome::Handle(proc) = open_for_query(pid) else {
            return None;
        };
        if !handle_is_alive(&proc) {
            return None;
        }
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut kernel = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut user = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        if unsafe {
            GetProcessTimes(
                proc.handle,
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return None;
        }
        let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        format_filetime_marker(ticks)
    }
}

#[cfg(windows)]
pub use win::{alive, start_marker};

#[cfg(not(any(unix, windows)))]
pub fn alive(_pid: u32) -> bool {
    false
}

#[cfg(not(any(unix, windows)))]
pub async fn start_marker(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod format_tests {
    use super::format_filetime_marker;

    #[test]
    fn formats_filetime_with_microsecond_truncation() {
        const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
        assert_eq!(format_filetime_marker(0), None);
        assert_eq!(format_filetime_marker(UNIX_EPOCH_TICKS - 10), None);
        assert_eq!(
            format_filetime_marker(UNIX_EPOCH_TICKS).as_deref(),
            Some("1970-01-01T00:00:00.0000000Z")
        );
        // 900ns truncates to 0 microseconds (matching CIM DMTF truncation, not rounding).
        assert_eq!(
            format_filetime_marker(UNIX_EPOCH_TICKS + 9).as_deref(),
            Some("1970-01-01T00:00:00.0000000Z")
        );
        assert_eq!(
            format_filetime_marker(UNIX_EPOCH_TICKS + 17).as_deref(),
            Some("1970-01-01T00:00:00.0000010Z")
        );
        // 2024-02-29T23:59:59.9876543Z (leap day) -> 19782 days after 1970-01-01.
        let leap_secs = 19_782 * 86_400 + 23 * 3_600 + 59 * 60 + 59;
        let leap_ticks = UNIX_EPOCH_TICKS + leap_secs * 10_000_000 + 9_876_543;
        assert_eq!(
            format_filetime_marker(leap_ticks).as_deref(),
            Some("2024-02-29T23:59:59.9876540Z")
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn matches_typescript_for_live_children() {
        let mut children = Vec::new();
        for i in 0..50 {
            if i == 25 {
                tokio::time::sleep(Duration::from_millis(1050)).await;
            }
            children.push(
                tokio::process::Command::new("sleep")
                    .arg("60")
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap(),
            );
        }
        let mut args = Vec::new();
        for child in &children {
            args.push(child.id().unwrap().to_string());
        }
        let output=tokio::process::Command::new("bun").current_dir(concat!(env!("CARGO_MANIFEST_DIR"),"/../../cli")).args(["-e","import {getProcessStartMarker as m} from './src/utils/process.ts'; console.log(JSON.stringify(process.argv.slice(1).map(p=>m(Number(p)))))"]).args(args).output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(expected.len(), children.len());
        for (mut child, marker) in children.into_iter().zip(expected) {
            assert_eq!(start_marker(child.id().unwrap()).await, Some(marker));
            child.kill().await.unwrap();
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn alive_and_start_marker_match_typescript() {
        const SLEEP_ROLE: &str = "HAPI_START_MARKER_WIN_SLEEP";
        if std::env::var_os(SLEEP_ROLE).is_some() {
            tokio::time::sleep(Duration::from_secs(60)).await;
            return;
        }

        assert!(!alive(0));
        assert_eq!(start_marker(0).await, None);

        let self_pid = std::process::id();
        assert!(alive(self_pid));
        let first = start_marker(self_pid).await.unwrap();
        let second = start_marker(self_pid).await.unwrap();
        assert_eq!(first, second);
        assert!(first.ends_with("0Z"), "unexpected marker format: {first}");

        let mut children = Vec::new();
        for _ in 0..5 {
            children.push(
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "lifecycle::start_marker::windows_tests::alive_and_start_marker_match_typescript",
                    ])
                    .env(SLEEP_ROLE, "1")
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap(),
            );
        }

        let mut args = vec![self_pid.to_string()];
        for child in &children {
            args.push(child.id().unwrap().to_string());
        }
        let output = tokio::process::Command::new("bun")
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
            .args([
                "-e",
                "import {getProcessStartMarker as m} from './src/utils/process.ts'; console.log(JSON.stringify(process.argv.slice(1).map(p=>m(Number(p)))))",
            ])
            .args(&args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(expected.len(), args.len());
        assert_eq!(first, expected[0]);

        for (mut child, marker) in children.into_iter().zip(expected.into_iter().skip(1)) {
            let pid = child.id().unwrap();
            assert!(alive(pid));
            assert_eq!(start_marker(pid).await, Some(marker));
            child.kill().await.unwrap();
            let _ = child.wait().await;
            assert!(!alive(pid));
            assert_eq!(start_marker(pid).await, None);
        }
    }
}
