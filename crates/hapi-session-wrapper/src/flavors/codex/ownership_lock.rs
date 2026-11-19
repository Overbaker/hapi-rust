use std::{
    fs::{self, File},
    future::Future,
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{sleep, Instant};
const STALE: Duration = Duration::from_secs(30);
const UPDATE: Duration = Duration::from_secs(10);
struct Lease {
    path: PathBuf,
    dir: File,
    stamp: u128,
    seconds: bool,
}
fn millis(time: SystemTime) -> io::Result<u128> {
    Ok(time
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis())
}
fn stamp(dir: &File) -> io::Result<u128> {
    millis(dir.metadata()?.modified()?)
}
fn set_mtime(dir: &File, ms: u128) -> io::Result<()> {
    let time = UNIX_EPOCH + Duration::from_millis(ms as u64);
    dir.set_times(fs::FileTimes::new().set_accessed(time).set_modified(time))
}
impl Lease {
    fn create(path: &Path) -> io::Result<Self> {
        fs::create_dir(path)?;
        let init = || {
            let dir = File::open(path)?;
            set_mtime(&dir, millis(SystemTime::now())?.div_ceil(1000) * 1000 + 5)?;
            let stamp = stamp(&dir)?;
            Ok(Self {
                dir,
                path: path.into(),
                stamp,
                seconds: stamp % 1000 == 0,
            })
        };
        init().inspect_err(|_| {
            let _ = fs::remove_dir(path);
        })
    }
    fn attempt(path: &Path) -> io::Result<Self> {
        match Self::create(path) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            other => return other,
        }
        match fs::metadata(path) {
            Ok(m) if millis(m.modified()?)? + STALE.as_millis() < millis(SystemTime::now())? => {
                match fs::remove_dir(path) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Lock file is already being held",
                ))
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        Self::create(path)
    }
    fn owns(&self) -> io::Result<bool> {
        let path = fs::metadata(&self.path)?;
        let dir = self.dir.metadata()?;
        Ok(path.ino() == dir.ino()
            && path.dev() == dir.dev()
            && millis(path.modified()?)? == self.stamp)
    }
    fn release(&self) -> io::Result<()> {
        if self.owns()? {
            fs::remove_dir(&self.path)
        } else {
            Err(io::Error::other("Lock is no longer owned"))
        }
    }
    fn refresh(&mut self) -> io::Result<()> {
        if !self.owns()? {
            return Err(io::Error::other(
                "Unable to update lock within the stale threshold",
            ));
        }
        let now = millis(SystemTime::now())?;
        let next = if self.seconds {
            now.div_ceil(1000) * 1000
        } else {
            now
        };
        set_mtime(&self.dir, next)?;
        self.stamp = next;
        Ok(())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

pub async fn with_lock<T>(
    directory: &Path,
    work: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    super::registry::mkdir(directory)?;
    let path = directory.join("ownership.hapi.lock");
    let mut retries = 50;
    let mut lease = loop {
        match Lease::attempt(&path) {
            Ok(l) => break l,
            Err(e) if retries == 0 => return Err(e),
            Err(_) => {
                retries -= 1;
                sleep(Duration::from_millis(100)).await;
            }
        }
    };
    let mut delay = UPDATE;
    let mut last = Instant::now();
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            _=sleep(delay)=>match lease.refresh(){
                Ok(())=>{last=Instant::now();delay=UPDATE;}
                Err(e) if e.kind()==io::ErrorKind::NotFound || e.kind()==io::ErrorKind::Other || last.elapsed()>=STALE=>return Err(e),
                Err(_)=>delay=Duration::from_secs(1)
            },
            result = &mut work => {
                lease.release()?;
                return result;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        process::Command,
    };
    #[tokio::test]
    async fn contention_between_typescript_and_rust() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let spawn = || {
            Command::new("bun")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/flavors/codex/lock_holder.ts"
                ))
                .arg(dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap()
        };
        let mut ts = spawn();
        let mut lines = BufReader::new(ts.stdout.take().unwrap()).lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("waiting"));
        ts.stdin
            .as_mut()
            .unwrap()
            .write_all(b"start\n")
            .await
            .unwrap();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("held"));
        let acquire = with_lock(dir, async { Ok(()) });
        tokio::pin!(acquire);
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut acquire)
                .await
                .is_err()
        );
        ts.stdin
            .take()
            .unwrap()
            .write_all(b"release\n")
            .await
            .unwrap();
        assert!(ts.wait().await.unwrap().success());
        acquire.await.unwrap();
        let mut ts = spawn();
        let mut lines = BufReader::new(ts.stdout.take().unwrap()).lines();
        with_lock(dir, async {
            assert_eq!(lines.next_line().await?.as_deref(), Some("waiting"));
            ts.stdin.as_mut().unwrap().write_all(b"start\n").await?;
            assert!(
                tokio::time::timeout(Duration::from_millis(250), lines.next_line())
                    .await
                    .is_err()
            );
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("held"));
        ts.stdin
            .take()
            .unwrap()
            .write_all(b"release\n")
            .await
            .unwrap();
        assert!(ts.wait().await.unwrap().success());
    }
    #[tokio::test(start_paused = true)]
    async fn stale_refresh_and_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ownership.hapi.lock");
        fs::create_dir(&path).unwrap();
        set_mtime(&File::open(&path).unwrap(), 1234).unwrap();
        with_lock(tmp.path(), async {
            let first = stamp(&File::open(&path)?)?;
            sleep(UPDATE + Duration::from_secs(1)).await;
            assert_ne!(first, stamp(&File::open(&path)?)?);
            Ok(())
        })
        .await
        .unwrap();
        let lost = with_lock(tmp.path(), async {
            fs::remove_dir(&path)?;
            fs::create_dir(&path)?;
            sleep(STALE).await;
            Ok(())
        })
        .await;
        assert!(lost.is_err());
        assert!(path.is_dir());
    }
}
