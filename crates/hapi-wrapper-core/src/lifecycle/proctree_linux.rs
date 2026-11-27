use nix::libc;
use std::{
    collections::HashMap,
    fs, io,
    os::fd::AsRawFd,
    sync::{LazyLock, Mutex},
};
use tokio::process::{Child, Command};

static SUPERVISORS: LazyLock<Mutex<HashMap<i32, u64>>> = LazyLock::new(Mutex::default);

fn identity(pid: i32) -> Option<(u64, bool)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat.rsplit_once(") ")?.1.split_ascii_whitespace();
    let zombie = fields.next()? == "Z";
    let start = fields.nth(18)?.parse().ok()?;
    Some((start, zombie))
}

pub(super) fn supervisor_running(pid: i32) -> Option<bool> {
    let supervisors = SUPERVISORS.lock().unwrap();
    let start = supervisors.get(&pid)?;
    let (actual, zombie) = identity(pid)?;
    (*start == actual).then_some(!zombie)
}

pub(super) fn spawn(command: &mut Command, setup: fn() -> io::Result<()>) -> io::Result<Child> {
    // The launcher never execs. Only the payload runs the remaining command setup
    // and keeps Rust's exec-error pipe and the command's stdio.
    unsafe {
        command.process_group(0).pre_exec(move || {
            nix::sys::prctl::set_child_subreaper(true)?;
            let children = nix::fcntl::open(
                c"/proc/thread-self/children",
                nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_CLOEXEC,
                nix::sys::stat::Mode::empty(),
            )?;
            let mut fds = std::mem::MaybeUninit::uninit();
            if libc::getrlimit(libc::RLIMIT_NOFILE, fds.as_mut_ptr()) < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd_limit = fds.assume_init().rlim_cur.min(i32::MAX as _) as i32;
            let parent = libc::getpid();
            match libc::fork() {
                -1 => Err(io::Error::last_os_error()),
                0 => {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        libc::_exit(127);
                    }
                    setup()
                }
                pid => supervise(pid, children.as_raw_fd(), fd_limit),
            }
        });
    }
    let child = command.spawn()?;
    if let Some(pid) = child.id().map(|pid| pid as i32) {
        let mut supervisors = SUPERVISORS.lock().unwrap();
        supervisors.retain(|&pid, start| identity(pid).is_some_and(|(actual, _)| actual == *start));
        if let Some((start, false)) = identity(pid) {
            supervisors.insert(pid, start);
        }
    }
    Ok(child)
}

// This runs after fork in the pre_exec hook. Use only syscalls and stack data.
unsafe fn supervise(leader: libc::pid_t, children: libc::c_int, fd_limit: i32) -> ! {
    let fd = children as u32;
    let lower_closed = fd == 0 || libc::syscall(libc::SYS_close_range, 0u32, fd - 1, 0u32) == 0;
    let upper_closed = libc::syscall(libc::SYS_close_range, fd + 1, u32::MAX, 0u32) == 0;
    if !lower_closed || !upper_closed {
        for fd in 0..fd_limit {
            if fd != children {
                libc::close(fd);
            }
        }
    }
    let mut status = 0;
    loop {
        let pid = libc::waitpid(-1, &mut status, 0);
        if pid == leader {
            break;
        }
        if pid < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            libc::_exit(127);
        }
    }

    // The kernel reparents the payload's surviving descendants here, even
    // when they have left its session. Each killed child can expose more.
    drain(children);

    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        libc::signal(signal, libc::SIG_DFL);
        let mut mask = std::mem::MaybeUninit::uninit();
        libc::sigemptyset(mask.as_mut_ptr());
        libc::sigaddset(mask.as_mut_ptr(), signal);
        libc::sigprocmask(libc::SIG_UNBLOCK, mask.as_ptr(), std::ptr::null_mut());
        libc::kill(libc::getpid(), signal);
        libc::_exit(128 + signal);
    }
    libc::_exit(libc::WEXITSTATUS(status));
}

unsafe fn drain(children: libc::c_int) {
    let Some(start) = monotonic_ms() else { return };
    let pause = libc::timespec {
        tv_sec: 0,
        tv_nsec: 1_000_000,
    };
    while monotonic_ms().is_some_and(|now| now - start < 1000) {
        let Some(pid) = first_child(children) else {
            return;
        };
        match libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) {
            0 => {}
            result if result == pid => continue,
            _ if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) => continue,
            _ => return,
        }
        if libc::kill(pid, libc::SIGKILL) < 0
            && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            return;
        }
        loop {
            let result = libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
            if result == pid {
                break;
            }
            if (result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR))
                || monotonic_ms().is_none_or(|now| now - start >= 1000)
            {
                return;
            }
            libc::nanosleep(&pause, std::ptr::null_mut());
        }
    }
}

unsafe fn monotonic_ms() -> Option<u64> {
    let mut time = std::mem::MaybeUninit::uninit();
    if libc::clock_gettime(libc::CLOCK_MONOTONIC, time.as_mut_ptr()) < 0 {
        return None;
    }
    let time = time.assume_init();
    Some(time.tv_sec as u64 * 1000 + time.tv_nsec as u64 / 1_000_000)
}

unsafe fn first_child(fd: libc::c_int) -> Option<libc::pid_t> {
    let mut buf = [0u8; 16];
    if libc::lseek(fd, 0, libc::SEEK_SET) < 0 {
        return None;
    }
    let count = loop {
        let count = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
        if count >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break count;
        }
    };
    if count <= 0 {
        return None;
    }
    let pid = buf[..count as usize]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .fold(0, |pid, byte| pid * 10 + i32::from(byte - b'0'));
    (pid > 1).then_some(pid)
}
