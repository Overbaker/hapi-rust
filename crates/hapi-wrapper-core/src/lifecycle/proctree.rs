//! Kernel parentage / Job Objects enforce wrapper ownership.
//!
//! On Linux, the wrapper becomes a subreaper so orphaned descendants, including
//! double-forked daemons, return to it; each tool is spawned in a private
//! process group for a single group SIGTERM, followed by SIGKILL at the soft
//! deadline.
//!
//! On Windows, the wrapper creates a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (assigned to itself for crash/exit
//! cleanup) plus a nested child-group Job Object and per-child nested Job
//! Objects. Soft termination closes any attached child stdin and, when a
//! console is attached, sends `CTRL_BREAK_EVENT` to the child's process group
//! (`CREATE_NEW_PROCESS_GROUP` at spawn). Hard termination calls
//! `TerminateJobObject` on the child's or wrapper's job at the deadline.

use std::io;
use tokio::{process::Command, time::Instant};

pub fn configure_child_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    #[cfg(not(any(unix, windows)))]
    let _ = command;
}

#[cfg(unix)]
mod platform {
    use super::*;
    use nix::{
        errno::Errno,
        sys::{
            signal::{kill, Signal},
            wait::{waitpid, WaitPidFlag},
        },
        unistd::{getpgrp, getpid, setsid, Pid},
    };
    use std::{collections::HashSet, time::Duration};
    use tokio::sync::watch;

    pub fn establish_wrapper_group() -> io::Result<()> {
        #[cfg(target_os = "linux")]
        nix::sys::prctl::set_child_subreaper(true)?;
        if getpgrp() != getpid() {
            setsid()?;
        }
        Ok(())
    }

    pub fn track_child(_pid: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn attach_child_stdin<T: Send + 'static>(_pid: u32, _stdin: T) {}

    #[cfg(target_os = "linux")]
    fn children(pid: Pid) -> io::Result<Vec<Pid>> {
        use std::fs;
        let mut children = Vec::new();
        let tasks = fs::read_dir(format!("/proc/{pid}/task"))?;
        for task in tasks {
            let file = task?.path().join("children");
            match fs::read_to_string(file) {
                Ok(list) => {
                    for raw in list.split_whitespace() {
                        children.push(Pid::from_raw(raw.parse().map_err(io::Error::other)?));
                    }
                }
                Err(e) if vanished(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(children)
    }

    #[cfg(target_os = "linux")]
    fn vanished(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(nix::libc::ENOENT | nix::libc::ESRCH))
    }

    #[derive(Clone, Copy)]
    pub(crate) struct Tree(Option<Pid>);

    impl Tree {
        pub(crate) fn terminal(pid: u32) -> io::Result<Self> {
            let raw = i32::try_from(pid)
                .ok()
                .filter(|p| *p > 1 && *p != getpid().as_raw())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid child PID"))?;
            Ok(Self(Some(Pid::from_raw(raw))))
        }

        fn members(self, known: &HashSet<Pid>) -> io::Result<Vec<(Pid, Pid)>> {
            let mut members = Vec::new();
            #[cfg(target_os = "linux")]
            {
                use nix::unistd::{getpgid, getsid};
                let mut pending = vec![(getpid(), self.0.is_none())];
                let mut visited = HashSet::new();
                while let Some((parent, selected)) = pending.pop() {
                    if !visited.insert(parent) {
                        continue;
                    }
                    let kids = match children(parent) {
                        Ok(kids) => kids,
                        Err(e) if parent != getpid() && vanished(&e) => continue,
                        Err(e) => return Err(e),
                    };
                    for pid in kids {
                        let group = match getpgid(Some(pid)) {
                            Ok(group) => group,
                            Err(Errno::ESRCH) => continue,
                            Err(e) => return Err(e.into()),
                        };
                        let owned = selected
                            || known.contains(&pid)
                            || Some(pid) == self.0
                            || Some(group) == self.0
                            || self.0.is_some_and(|root| getsid(Some(pid)) == Ok(root));
                        pending.push((pid, owned));
                        if owned {
                            members.push((pid, group));
                        }
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let output = std::process::Command::new("ps")
                    .args(["-A", "-o", "pid=,ppid=,pgid="])
                    .output()?;
                if !output.status.success() {
                    return Err(io::Error::other("ps failed"));
                }
                let rows: Vec<_> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| {
                        let mut ids = line
                            .split_whitespace()
                            .map(|v| v.parse::<i32>().ok().map(Pid::from_raw));
                        Some((ids.next()??, ids.next()??, ids.next()??))
                    })
                    .collect();
                let groups: HashSet<_> = rows
                    .iter()
                    .filter(|(pid, parent, group)| {
                        *parent == getpid()
                            && pid == group
                            && self.0.is_none_or(|root| root == *pid)
                    })
                    .map(|row| row.2)
                    .collect();
                for (pid, _, group) in rows {
                    if groups.contains(&group) || known.contains(&group) {
                        members.push((pid, group));
                    }
                }
            }
            Ok(members)
        }

        pub(crate) async fn terminate(
            self,
            soft: Instant,
            hard: Instant,
            shutdown: Option<&watch::Receiver<bool>>,
        ) -> io::Result<bool> {
            let mut sweep = Sweep::new(self, soft, hard);
            loop {
                if shutdown.is_some_and(|rx| *rx.borrow()) {
                    sweep.soft = Instant::now();
                    sweep.hard = sweep.hard.min(sweep.soft + Duration::from_millis(250));
                }
                match sweep.step()? {
                    Some(done) => return Ok(done),
                    None => tokio::time::sleep_until(sweep.next()).await,
                }
            }
        }
    }

    struct Sweep {
        tree: Tree,
        known: HashSet<Pid>,
        warned: HashSet<i32>,
        soft: Instant,
        hard: Instant,
    }

    impl Sweep {
        fn new(tree: Tree, soft: Instant, hard: Instant) -> Self {
            Self {
                tree,
                known: HashSet::new(),
                warned: HashSet::new(),
                soft,
                hard,
            }
        }

        fn next(&self) -> Instant {
            let now = Instant::now();
            (now + Duration::from_millis(10)).min(if now < self.soft {
                self.soft
            } else {
                self.hard
            })
        }

        fn step(&mut self) -> io::Result<Option<bool>> {
            let members = self.tree.members(&self.known)?;
            if members.is_empty() {
                return Ok(Some(true));
            }
            self.known.extend(members.iter().map(|m| m.0));
            let hard = Instant::now() >= self.soft;
            let mut targets = HashSet::new();
            for (pid, group) in members {
                #[cfg(all(target_os = "linux", not(target_env = "uclibc")))]
                if Some(pid) == self.tree.0 {
                    use nix::sys::wait::{waitid, Id, WaitStatus};
                    let flags = WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT | WaitPidFlag::WEXITED;
                    match waitid(Id::Pid(pid), flags) {
                        Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => continue,
                        Ok(_) | Err(Errno::ECHILD | Errno::EINTR) => (),
                        Err(e) => return Err(e.into()),
                    }
                }
                #[cfg(all(target_os = "linux", not(target_env = "uclibc")))]
                let preserve_status = Some(pid) == self.tree.0;
                #[cfg(not(all(target_os = "linux", not(target_env = "uclibc"))))]
                let preserve_status = false;
                if !preserve_status {
                    match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                        Ok(_) | Err(Errno::ECHILD | Errno::EINTR) => (),
                        Err(e) => return Err(e.into()),
                    }
                }
                let target = if group != getpgrp() && self.known.contains(&group) {
                    -group.as_raw()
                } else {
                    pid.as_raw()
                };
                targets.insert(target);
                if hard {
                    targets.insert(pid.as_raw());
                }
            }
            if targets.is_empty() {
                return Ok(Some(true));
            }
            for target in targets {
                if hard || self.warned.insert(target) {
                    let signal = if hard {
                        Signal::SIGKILL
                    } else {
                        Signal::SIGTERM
                    };
                    match kill(Pid::from_raw(target), signal) {
                        Ok(()) | Err(Errno::ESRCH) => (),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Ok((Instant::now() >= self.hard).then_some(false))
        }
    }

    pub fn terminate_child_tree(pid: u32) -> io::Result<bool> {
        let now = Instant::now();
        let mut sweep = Sweep::new(
            Tree::terminal(pid)?,
            now + Duration::from_secs(2),
            now + Duration::from_secs(3),
        );
        loop {
            if let Some(done) = sweep.step()? {
                return Ok(done);
            }
            std::thread::sleep(sweep.next().saturating_duration_since(Instant::now()));
        }
    }

    pub(in crate::lifecycle) async fn kill_agent_tree_by(
        deadline: Instant,
        changes: Option<&mut watch::Receiver<Option<Instant>>>,
    ) -> io::Result<bool> {
        let deadline = changes
            .and_then(|rx| *rx.borrow())
            .map_or(deadline, |cap| cap.min(deadline));
        let now = Instant::now();
        let soft =
            now + Duration::from_secs(2).min(deadline.saturating_duration_since(now) * 2 / 3);
        Tree(None).terminate(soft, deadline, None).await
    }

    pub async fn kill_agent_tree(deadline: Instant) -> io::Result<bool> {
        kill_agent_tree_by(deadline, None).await
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        ptr,
        sync::{LazyLock, Mutex},
        time::Duration,
    };
    use tokio::sync::watch;
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, HANDLE,
            INVALID_HANDLE_VALUE, STILL_ACTIVE, WAIT_TIMEOUT,
        },
        System::{
            Console::{GenerateConsoleCtrlEvent, GetConsoleProcessList, CTRL_BREAK_EVENT},
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
                JobObjectExtendedLimitInformation, QueryInformationJobObject,
                SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_PROCESS_ID_LIST,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::{
                GetCurrentProcess, GetExitCodeProcess, OpenProcess, TerminateProcess,
                WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
                PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            },
        },
    };

    struct OwnedHandle(HANDLE);
    unsafe impl Send for OwnedHandle {}
    unsafe impl Sync for OwnedHandle {}

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    #[derive(Default)]
    struct WrapperJobs {
        /// Wrapper-lifetime Job Object containing the wrapper itself and all its
        /// descendants (enforced by the kernel). Never terminated or closed while
        /// the wrapper runs; when the wrapper exits or crashes, Windows closes
        /// this handle and terminates every surviving descendant.
        self_job: Option<OwnedHandle>,
        /// Nested child-container Job Object holding tracked children (excludes
        /// the wrapper process itself). Terminated and closed on wrapper shutdown.
        wrapper_job: Option<OwnedHandle>,
        /// Per-child nested Job Objects keyed by spawned child PID.
        children: HashMap<u32, OwnedHandle>,
        /// Optional child stdin handles dropped at the start of the soft phase.
        stdins: HashMap<u32, Box<dyn Send>>,
    }

    static STATE: LazyLock<Mutex<WrapperJobs>> = LazyLock::new(Mutex::default);

    fn create_kill_on_close_job() -> io::Result<OwnedHandle> {
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let job = OwnedHandle(handle);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn job_pids(job: &OwnedHandle) -> io::Result<Vec<u32>> {
        let mut capacity = 64usize;
        loop {
            let header = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            let extra = capacity.saturating_sub(1) * std::mem::size_of::<usize>();
            let words = (header + extra).div_ceil(std::mem::size_of::<usize>());
            let mut buf = vec![0usize; words];
            let mut returned = 0u32;
            let ok = unsafe {
                QueryInformationJobObject(
                    job.0,
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr().cast(),
                    (words * std::mem::size_of::<usize>()) as u32,
                    &mut returned,
                )
            };
            if ok != 0 {
                let header_ptr = buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
                let count = unsafe { (*header_ptr).NumberOfProcessIdsInList as usize };
                let list_ptr = unsafe { ptr::addr_of!((*header_ptr).ProcessIdList[0]) };
                let slice = unsafe { std::slice::from_raw_parts(list_ptr, count.min(capacity)) };
                return Ok(slice
                    .iter()
                    .filter_map(|&pid| u32::try_from(pid).ok())
                    .collect());
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_MORE_DATA as i32) {
                let header_ptr = buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
                let needed = unsafe { (*header_ptr).NumberOfAssignedProcesses as usize };
                capacity = needed.max(capacity * 2) + 16;
                continue;
            }
            return Err(err);
        }
    }

    fn process_exited(handle: HANDLE) -> bool {
        if unsafe { WaitForSingleObject(handle, 0) } != WAIT_TIMEOUT {
            return true;
        }
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(handle, &mut code) != 0 && code != STILL_ACTIVE as u32 }
    }

    fn open_for_job(pid: u32) -> io::Result<Option<OwnedHandle>> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA
                    | PROCESS_TERMINATE
                    | PROCESS_QUERY_LIMITED_INFORMATION
                    | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if !handle.is_null() {
            return Ok(Some(OwnedHandle(handle)));
        }
        if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
            return Ok(None);
        }
        Err(io::Error::last_os_error())
    }

    fn has_console() -> bool {
        let mut pid = 0u32;
        unsafe { GetConsoleProcessList(&mut pid, 1) > 0 }
    }

    fn send_ctrl_break(pid: u32) {
        if pid != 0 && pid != std::process::id() && has_console() {
            unsafe {
                let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
            }
        }
    }

    fn descendants_within(root: u32, owned: &HashSet<u32>) -> Vec<u32> {
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap.is_null() || snap == INVALID_HANDLE_VALUE {
            return owned.contains(&root).then_some(root).into_iter().collect();
        }
        let _guard = OwnedHandle(snap);
        let mut by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if unsafe { Process32FirstW(snap, &mut entry) } != 0 {
            loop {
                if owned.contains(&entry.th32ProcessID) {
                    by_parent
                        .entry(entry.th32ParentProcessID)
                        .or_default()
                        .push(entry.th32ProcessID);
                }
                if unsafe { Process32NextW(snap, &mut entry) } == 0 {
                    break;
                }
            }
        }
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        if owned.contains(&root) {
            queue.push_back(root);
        }
        while let Some(pid) = queue.pop_front() {
            if !seen.insert(pid) {
                continue;
            }
            result.push(pid);
            if let Some(children) = by_parent.get(&pid) {
                for &child in children {
                    queue.push_back(child);
                }
            }
        }
        result
    }

    fn terminate_pid_hard(pid: u32) {
        if pid == 0 || pid == std::process::id() {
            return;
        }
        if let Ok(Some(proc)) = open_for_job(pid) {
            if let Ok(job) = create_kill_on_close_job() {
                if unsafe { AssignProcessToJobObject(job.0, proc.0) } != 0 {
                    unsafe {
                        let _ = TerminateJobObject(job.0, 1);
                    }
                    return;
                }
            }
            unsafe {
                let _ = TerminateProcess(proc.0, 1);
            }
        } else {
            let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
            if !handle.is_null() {
                let _guard = OwnedHandle(handle);
                unsafe {
                    let _ = TerminateProcess(handle, 1);
                }
            }
        }
    }

    fn self_job_non_self_pids() -> io::Result<Vec<u32>> {
        let self_pid = std::process::id();
        let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(self_job) = state.self_job.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(job_pids(self_job)?
            .into_iter()
            .filter(|&pid| pid != self_pid)
            .collect())
    }

    pub fn establish_wrapper_group() -> io::Result<()> {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if state.self_job.is_none() {
            let self_job = create_kill_on_close_job()?;
            if unsafe { AssignProcessToJobObject(self_job.0, GetCurrentProcess()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            state.self_job = Some(self_job);
        }
        if state.wrapper_job.is_none() {
            state.wrapper_job = Some(create_kill_on_close_job()?);
        }
        Ok(())
    }

    pub fn track_child(pid: u32) -> io::Result<()> {
        if pid == 0 || pid == std::process::id() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid child PID",
            ));
        }
        let Some(proc) = open_for_job(pid)? else {
            return Ok(());
        };
        if process_exited(proc.0) {
            return Ok(());
        }
        let child_job = create_kill_on_close_job()?;
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if state.self_job.is_some() && state.wrapper_job.is_none() {
            state.wrapper_job = Some(create_kill_on_close_job()?);
        }
        if let Some(wrapper_job) = state.wrapper_job.as_ref() {
            if unsafe { AssignProcessToJobObject(wrapper_job.0, proc.0) } == 0
                && process_exited(proc.0)
            {
                return Ok(());
            }
        }
        if unsafe { AssignProcessToJobObject(child_job.0, proc.0) } == 0 {
            if process_exited(proc.0) {
                return Ok(());
            }
            return Err(io::Error::last_os_error());
        }
        state.children.insert(pid, child_job);
        Ok(())
    }

    pub fn attach_child_stdin<T: Send + 'static>(pid: u32, stdin: T) {
        if pid == 0 || pid == std::process::id() {
            return;
        }
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.stdins.insert(pid, Box::new(stdin));
    }

    fn terminate_child_tree_with_budget(
        pid: u32,
        soft_budget: Duration,
        hard_budget: Duration,
    ) -> io::Result<bool> {
        if pid == 0 || pid == std::process::id() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid child PID",
            ));
        }
        let (tracked_job, attached_stdin) = {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            (state.children.remove(&pid), state.stdins.remove(&pid))
        };
        // Soft phase step 1: close child's stdin.
        drop(attached_stdin);

        let now = Instant::now();
        let soft = now + soft_budget;
        let hard = now + hard_budget;

        if let Some(job) = tracked_job {
            if job_pids(&job)?.is_empty() {
                return Ok(true);
            }
            if !soft_budget.is_zero() {
                send_ctrl_break(pid);
                while Instant::now() < soft {
                    if job_pids(&job)?.is_empty() {
                        return Ok(true);
                    }
                    let wait = (Instant::now() + Duration::from_millis(10))
                        .min(soft)
                        .saturating_duration_since(Instant::now());
                    if !wait.is_zero() {
                        std::thread::sleep(wait);
                    }
                }
            }
            if !job_pids(&job)?.is_empty() {
                unsafe {
                    let _ = TerminateJobObject(job.0, 1);
                }
            }
            loop {
                if job_pids(&job)?.is_empty() {
                    return Ok(true);
                }
                let now = Instant::now();
                if now >= hard {
                    return Ok(false);
                }
                let wait = (now + Duration::from_millis(10))
                    .min(hard)
                    .saturating_duration_since(now);
                std::thread::sleep(wait);
            }
        }

        // Untracked fallback: only touch processes proven to belong to `self_job`.
        let owned: HashSet<u32> = self_job_non_self_pids()?.into_iter().collect();
        if !owned.contains(&pid) {
            return Ok(true);
        }
        let mut known: HashSet<u32> = descendants_within(pid, &owned).into_iter().collect();
        let temp_job = create_kill_on_close_job().ok();
        if let Some(job) = temp_job.as_ref() {
            for &member in &known {
                if let Ok(Some(proc)) = open_for_job(member) {
                    unsafe {
                        let _ = AssignProcessToJobObject(job.0, proc.0);
                    }
                }
            }
        }
        if !soft_budget.is_zero() {
            send_ctrl_break(pid);
            while Instant::now() < soft {
                let current_owned: HashSet<u32> = self_job_non_self_pids()?.into_iter().collect();
                known.extend(descendants_within(pid, &current_owned));
                let job_empty = temp_job
                    .as_ref()
                    .is_none_or(|j| job_pids(j).ok().is_none_or(|v| v.is_empty()));
                if job_empty && !known.iter().any(|p| current_owned.contains(p)) {
                    return Ok(true);
                }
                let wait = (Instant::now() + Duration::from_millis(10))
                    .min(soft)
                    .saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            }
        }
        if let Some(job) = temp_job.as_ref() {
            unsafe {
                let _ = TerminateJobObject(job.0, 1);
            }
        }
        loop {
            let current_owned: HashSet<u32> = self_job_non_self_pids()?.into_iter().collect();
            known.extend(descendants_within(pid, &current_owned));
            let remaining: Vec<u32> = known
                .iter()
                .copied()
                .filter(|p| current_owned.contains(p))
                .collect();
            let job_empty = temp_job
                .as_ref()
                .is_none_or(|j| job_pids(j).ok().is_none_or(|v| v.is_empty()));
            if job_empty && remaining.is_empty() {
                return Ok(true);
            }
            for p in remaining {
                terminate_pid_hard(p);
            }
            let now = Instant::now();
            if now >= hard {
                return Ok(false);
            }
            let wait = (now + Duration::from_millis(10))
                .min(hard)
                .saturating_duration_since(now);
            std::thread::sleep(wait);
        }
    }

    pub fn terminate_child_tree(pid: u32) -> io::Result<bool> {
        terminate_child_tree_with_budget(pid, Duration::from_secs(2), Duration::from_secs(3))
    }

    pub(crate) fn kill_child_tree_now(pid: u32) -> io::Result<bool> {
        terminate_child_tree_with_budget(pid, Duration::ZERO, Duration::from_secs(1))
    }

    fn all_children_gone() -> io::Result<bool> {
        let self_pid = std::process::id();
        let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(self_job) = state.self_job.as_ref() {
            if job_pids(self_job)?.into_iter().any(|p| p != self_pid) {
                return Ok(false);
            }
        }
        if let Some(wrapper_job) = state.wrapper_job.as_ref() {
            if !job_pids(wrapper_job)?.is_empty() {
                return Ok(false);
            }
        }
        for job in state.children.values() {
            if !job_pids(job)?.is_empty() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(in crate::lifecycle) async fn kill_agent_tree_by(
        deadline: Instant,
        changes: Option<&mut watch::Receiver<Option<Instant>>>,
    ) -> io::Result<bool> {
        let mut deadline = changes
            .as_ref()
            .and_then(|rx| *rx.borrow())
            .map_or(deadline, |cap| cap.min(deadline));
        let now = Instant::now();
        let mut soft =
            now + Duration::from_secs(2).min(deadline.saturating_duration_since(now) * 2 / 3);

        // Soft phase: drop all attached child stdins and send CTRL_BREAK to child groups.
        let (stdins, break_targets) = {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            let stdins: Vec<Box<dyn Send>> = state.stdins.drain().map(|(_, s)| s).collect();
            let mut targets: Vec<u32> = state.children.keys().copied().collect();
            if let Some(self_job) = state.self_job.as_ref() {
                let self_pid = std::process::id();
                if let Ok(pids) = job_pids(self_job) {
                    for pid in pids {
                        if pid != self_pid && !targets.contains(&pid) {
                            targets.push(pid);
                        }
                    }
                }
            }
            (stdins, targets)
        };
        drop(stdins);

        if all_children_gone()? {
            return Ok(true);
        }

        for pid in break_targets {
            send_ctrl_break(pid);
        }

        let mut changes = changes;
        while Instant::now() < soft {
            if let Some(rx) = changes.as_ref() {
                if let Some(cap) = *rx.borrow() {
                    deadline = deadline.min(cap);
                    soft = soft.min(deadline);
                }
            }
            if all_children_gone()? {
                let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
                state.children.clear();
                return Ok(true);
            }
            let next = (Instant::now() + Duration::from_millis(10)).min(soft);
            tokio::time::sleep_until(next).await;
        }

        // Hard phase: terminate & close the wrapper child job and all per-child jobs.
        let (wrapper_job, child_jobs) = {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            let wrapper_job = state.wrapper_job.take();
            let child_jobs: Vec<OwnedHandle> = state.children.drain().map(|(_, j)| j).collect();
            if state.self_job.is_some() {
                state.wrapper_job = create_kill_on_close_job().ok();
            }
            (wrapper_job, child_jobs)
        };
        if let Some(job) = wrapper_job {
            unsafe {
                let _ = TerminateJobObject(job.0, 1);
            }
            drop(job);
        }
        for job in child_jobs {
            unsafe {
                let _ = TerminateJobObject(job.0, 1);
            }
            drop(job);
        }

        loop {
            let remaining = self_job_non_self_pids()?;
            if remaining.is_empty() {
                return Ok(true);
            }
            for pid in remaining {
                terminate_pid_hard(pid);
            }
            if let Some(rx) = changes.as_mut() {
                if let Some(cap) = *rx.borrow_and_update() {
                    deadline = deadline.min(cap);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(self_job_non_self_pids()?.is_empty());
            }
            let next = (now + Duration::from_millis(10)).min(deadline);
            tokio::time::sleep_until(next).await;
        }
    }

    pub async fn kill_agent_tree(deadline: Instant) -> io::Result<bool> {
        kill_agent_tree_by(deadline, None).await
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    pub fn establish_wrapper_group() -> io::Result<()> {
        Ok(())
    }
    pub fn track_child(_pid: u32) -> io::Result<()> {
        Ok(())
    }
    pub fn attach_child_stdin<T: Send + 'static>(_pid: u32, _stdin: T) {}
    pub fn terminate_child_tree(_: u32) -> io::Result<bool> {
        Ok(true)
    }
    pub async fn kill_agent_tree(_: Instant) -> io::Result<bool> {
        Ok(true)
    }
}

#[cfg(any(unix, windows))]
#[allow(unused_imports)]
pub(super) use platform::kill_agent_tree_by;
#[cfg(windows)]
pub(crate) use platform::kill_child_tree_now;
#[cfg(unix)]
#[allow(unused_imports)]
pub(crate) use platform::Tree;
pub use platform::{
    attach_child_stdin, establish_wrapper_group, kill_agent_tree, terminate_child_tree, track_child,
};

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    use std::{
        io::{BufRead, BufReader},
        process::{Command as StdCommand, Stdio},
        time::Duration,
    };
    use tokio::io::{AsyncBufReadExt, BufReader as AsyncReader};

    #[test]
    fn linux_process_trees() {
        const INNER: &str = "HAPI_PROCTREE_INNER";
        if std::env::var_os(INNER).is_none() {
            let mut sentinel = StdCommand::new("sleep").arg("60").spawn().unwrap();
            let status = StdCommand::new(std::env::current_exe().unwrap())
                .args(["--exact", "lifecycle::proctree::tests::linux_process_trees"])
                .env(INNER, sentinel.id().to_string())
                .status()
                .unwrap();
            let alive = sentinel.try_wait().unwrap().is_none();
            sentinel.kill().unwrap();
            sentinel.wait().unwrap();
            assert!(status.success() && alive);
            return;
        }

        establish_wrapper_group().unwrap();
        let sentinel = std::env::var(INNER).unwrap().parse().unwrap();
        assert!(terminate_child_tree(sentinel).unwrap());
        let mut sibling = StdCommand::new("sleep").arg("60").spawn().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sh -c 'sleep 60 & echo $$ $!; wait' & wait"])
                .stdout(Stdio::piped());
            configure_child_group(&mut cmd);
            let mut root = cmd.spawn().unwrap();
            let pid = root.id().unwrap();
            let ids = AsyncReader::new(root.stdout.take().unwrap())
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                nix::unistd::getpgid(Some(Pid::from_raw(pid as i32)))
                    .unwrap()
                    .as_raw(),
                pid as i32
            );
            assert!(terminate_child_tree(pid).unwrap());
            root.wait().await.unwrap();
            for id in ids.split_whitespace() {
                assert_eq!(
                    kill(Pid::from_raw(id.parse().unwrap()), None),
                    Err(Errno::ESRCH)
                );
            }
        });
        assert!(sibling.try_wait().unwrap().is_none());
        sibling.kill().unwrap();
        sibling.wait().unwrap();

        let mut leader = StdCommand::new("sh")
            .args([
                "-c",
                "setsid sh -c 'sh -c \"trap \\\"\\\" TERM; echo \\$\\$; exec sleep 60\" &' & wait",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let daemon = Pid::from_raw(ready.trim().parse().unwrap());
        assert!(leader.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(format!("/proc/{daemon}/status"))
                .unwrap()
                .lines()
                .find(|l| l.starts_with("PPid:"))
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap(),
            std::process::id().to_string()
        );
        let start = Instant::now();
        let deadline = start + Duration::from_millis(900);
        assert!(runtime.block_on(kill_agent_tree(deadline)).unwrap());
        assert!(Instant::now() <= deadline);
        assert!(start.elapsed() >= Duration::from_millis(600));
        assert_eq!(kill(daemon, None), Err(Errno::ESRCH));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::lifecycle::start_marker::alive;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        process::{Command as StdCommand, Stdio},
        time::Duration,
    };
    use tokio::io::{AsyncBufReadExt, BufReader as AsyncReader};

    const ROLE: &str = "HAPI_WIN_PROCTREE_ROLE";
    const SENTINEL: &str = "HAPI_WIN_PROCTREE_SENTINEL";
    const TEST_NAME: &str = "lifecycle::proctree::windows_tests::windows_job_object_process_trees";

    unsafe extern "system" fn ignore_ctrl(_ctrl_type: u32) -> i32 {
        1
    }

    fn helper_command(role: &str) -> StdCommand {
        let mut cmd = StdCommand::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", TEST_NAME]).env(ROLE, role);
        cmd
    }

    fn async_helper_command(role: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", TEST_NAME]).env(ROLE, role);
        cmd
    }

    #[allow(clippy::zombie_processes)]
    fn run_helper(role: &str) {
        match role {
            "sleep" => std::thread::sleep(Duration::from_secs(60)),
            "level3" => {
                println!("{}", std::process::id());
                let _ = std::io::stdout().flush();
                std::thread::sleep(Duration::from_secs(60));
            }
            "level2" => {
                let mut child = helper_command("level3")
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut line = String::new();
                BufReader::new(child.stdout.take().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                println!("{} {}", std::process::id(), line.trim());
                let _ = std::io::stdout().flush();
                std::thread::sleep(Duration::from_secs(60));
            }
            "level1" => {
                let mut child = helper_command("level2")
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut line = String::new();
                BufReader::new(child.stdout.take().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                println!("{}", line.trim());
                let _ = std::io::stdout().flush();
                std::thread::sleep(Duration::from_secs(60));
            }
            "level1_orphan" => {
                let mut child = helper_command("level2")
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut line = String::new();
                BufReader::new(child.stdout.take().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                println!("{}", line.trim());
                let _ = std::io::stdout().flush();
                // Exit immediately so level2 and level3 become orphaned inside the nested job.
            }
            "exit_on_stdin_eof" => {
                println!("ready");
                let _ = std::io::stdout().flush();
                let mut buf = Vec::new();
                let _ = std::io::stdin().read_to_end(&mut buf);
            }
            "ignore_soft" => {
                unsafe {
                    windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                        Some(ignore_ctrl),
                        1,
                    );
                }
                let mut grandchild = helper_command("level3")
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut line = String::new();
                BufReader::new(grandchild.stdout.take().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                println!("{} {}", std::process::id(), line.trim());
                let _ = std::io::stdout().flush();
                std::thread::sleep(Duration::from_secs(60));
            }
            other => panic!("unknown helper role: {other}"),
        }
    }

    #[test]
    fn windows_job_object_process_trees() {
        if let Ok(role) = std::env::var(ROLE) {
            run_helper(&role);
            return;
        }
        if std::env::var_os(SENTINEL).is_none() {
            // Spawn an unrelated sentinel outside the inner wrapper's Job Object.
            let mut sentinel = helper_command("sleep").spawn().unwrap();
            let status = StdCommand::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME])
                .env(SENTINEL, sentinel.id().to_string())
                .status()
                .unwrap();
            let sentinel_survived = sentinel.try_wait().unwrap().is_none() && alive(sentinel.id());
            let _ = sentinel.kill();
            let _ = sentinel.wait();
            assert!(status.success(), "inner wrapper process failed: {status}");
            assert!(
                sentinel_survived,
                "unrelated sentinel process should survive"
            );
            return;
        }

        establish_wrapper_group().unwrap();
        let sentinel: u32 = std::env::var(SENTINEL).unwrap().parse().unwrap();
        // Unrelated PID is not owned by this wrapper; terminate_child_tree must not touch it.
        assert!(terminate_child_tree(sentinel).unwrap());
        assert!(alive(sentinel));

        // Spawn a sibling that must survive terminating another child's tree.
        let mut sibling = helper_command("sleep").spawn().unwrap();
        let sibling_pid = sibling.id();
        track_child(sibling_pid).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // 1. 3-level process tree (level1 -> level2 -> level3) is completely gone after terminate.
        runtime.block_on(async {
            let mut cmd = async_helper_command("level1");
            cmd.stdout(Stdio::piped());
            configure_child_group(&mut cmd);
            let mut root = cmd.spawn().unwrap();
            let pid1 = root.id().unwrap();
            track_child(pid1).unwrap();
            let line = AsyncReader::new(root.stdout.take().unwrap())
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            let descendants: Vec<u32> = line
                .split_whitespace()
                .map(|s| s.parse().unwrap())
                .collect();
            assert_eq!(descendants.len(), 2);
            assert!(alive(pid1));
            assert!(alive(descendants[0]));
            assert!(alive(descendants[1]));

            assert!(terminate_child_tree(pid1).unwrap());
            let _ = root.wait().await;
            assert!(!alive(pid1));
            for pid in descendants {
                assert!(
                    !alive(pid),
                    "descendant {pid} survived terminate_child_tree"
                );
            }
        });

        // 2. Orphaned 3-level tree where level1 exits early: nested Job Object still owns level2 & level3.
        runtime.block_on(async {
            let mut cmd = async_helper_command("level1_orphan");
            cmd.stdout(Stdio::piped());
            configure_child_group(&mut cmd);
            let mut root = cmd.spawn().unwrap();
            let pid1 = root.id().unwrap();
            track_child(pid1).unwrap();
            let line = AsyncReader::new(root.stdout.take().unwrap())
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            let descendants: Vec<u32> = line
                .split_whitespace()
                .map(|s| s.parse().unwrap())
                .collect();
            assert_eq!(descendants.len(), 2);
            assert!(root.wait().await.unwrap().success());
            assert!(alive(descendants[0]));
            assert!(alive(descendants[1]));

            assert!(terminate_child_tree(pid1).unwrap());
            for pid in descendants {
                assert!(!alive(pid), "orphaned descendant {pid} survived");
            }
        });

        // 3. Soft phase closes attached stdin so cooperative children exit before the 2s hard kill.
        runtime.block_on(async {
            let mut cmd = async_helper_command("exit_on_stdin_eof");
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
            configure_child_group(&mut cmd);
            let mut child = cmd.spawn().unwrap();
            let pid = child.id().unwrap();
            track_child(pid).unwrap();
            attach_child_stdin(pid, child.stdin.take().unwrap());
            let ready = AsyncReader::new(child.stdout.take().unwrap())
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(ready, "ready");
            let start = Instant::now();
            assert!(terminate_child_tree(pid).unwrap());
            let _ = child.wait().await;
            assert!(
                start.elapsed() < Duration::from_millis(1500),
                "soft stdin close took too long: {:?}",
                start.elapsed()
            );
            assert!(!alive(pid));
        });

        // Sibling and unrelated sentinel must still be alive after terminating other child trees.
        assert!(sibling.try_wait().unwrap().is_none() && alive(sibling_pid));
        assert!(alive(sentinel));

        // 4. Child (+ grandchild) that ignores soft phase is gone at the deadline via kill_agent_tree.
        let (stubborn_pids, mut stubborn_child) = runtime.block_on(async {
            let mut cmd = async_helper_command("ignore_soft");
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
            configure_child_group(&mut cmd);
            let mut child = cmd.spawn().unwrap();
            let pid = child.id().unwrap();
            track_child(pid).unwrap();
            attach_child_stdin(pid, child.stdin.take().unwrap());
            let line = AsyncReader::new(child.stdout.take().unwrap())
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            let pids: Vec<u32> = line
                .split_whitespace()
                .map(|s| s.parse().unwrap())
                .collect();
            assert_eq!(pids.len(), 2);
            (pids, child)
        });

        let start = Instant::now();
        let deadline = start + Duration::from_millis(900);
        assert!(runtime.block_on(kill_agent_tree(deadline)).unwrap());
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(550),
            "should wait soft phase before hard kill: {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_millis(1500),
            "exceeded deadline budget: {elapsed:?}"
        );
        let _ = runtime.block_on(stubborn_child.wait());
        let _ = sibling.wait();
        for pid in stubborn_pids {
            assert!(!alive(pid), "stubborn process {pid} survived deadline");
        }
        assert!(!alive(sibling_pid), "sibling survived wrapper shutdown");
        assert!(alive(sentinel), "unrelated sentinel died during shutdown");
    }
}
