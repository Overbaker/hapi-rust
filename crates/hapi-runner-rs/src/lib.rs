use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_KEEP_ALIVE_INTERVAL_MS: u64 = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedSession {
    pub id: String,
    pub flavor: String,
    pub workdir: String,
    pub thinking: bool,
    pub last_activity_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoalescedHeartbeatBatch {
    pub tick: u64,
    pub active_session_ids: Vec<String>,
    pub thinking_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessMemoryProbe {
    pub session_count: usize,
    pub boot_ms: f64,
    pub vm_rss_kib: u64,
    pub vm_size_kib: u64,
    pub heartbeat_interval_ms: u64,
}

/// Reads real Linux `VmRSS` and `VmSize` (KiB) from `/proc/self/status`.
pub fn read_proc_memory_kib() -> (u64, u64) {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return (0, 0);
    };
    let mut rss = 0u64;
    let mut vsize = 0u64;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            if let Some(num) = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok()) {
                rss = num;
            }
        } else if let Some(rest) = line.strip_prefix("VmSize:") {
            if let Some(num) = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok()) {
                vsize = num;
            }
        }
    }
    (rss, vsize)
}

/// Single-process Tokio runner daemon managing N concurrent sessions without
/// per-session VM/runtime overhead.
pub struct RunnerDaemon {
    pub sessions: DashMap<String, ManagedSession>,
    pub heartbeat_ticks: AtomicU64,
    pub dirty: AtomicBool,
    pub heartbeat_interval: Duration,
}

impl RunnerDaemon {
    pub fn new(heartbeat_interval_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            sessions: DashMap::new(),
            heartbeat_ticks: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            heartbeat_interval: Duration::from_millis(heartbeat_interval_ms),
        })
    }

    pub fn register_session(&self, session: ManagedSession) {
        self.sessions.insert(session.id.clone(), session);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn set_thinking(&self, session_id: &str, thinking: bool) {
        if let Some(mut entry) = self.sessions.get_mut(session_id) {
            if entry.thinking != thinking {
                entry.thinking = thinking;
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    /// Aggregates all managed sessions into one coalesced heartbeat frame.
    pub fn collect_coalesced_heartbeat(&self) -> CoalescedHeartbeatBatch {
        let tick = self.heartbeat_ticks.fetch_add(1, Ordering::Relaxed) + 1;
        let mut active_session_ids = Vec::with_capacity(self.sessions.len());
        let mut thinking_count = 0usize;
        for entry in self.sessions.iter() {
            active_session_ids.push(entry.id.clone());
            if entry.thinking {
                thinking_count += 1;
            }
        }
        active_session_ids.sort();
        self.dirty.store(false, Ordering::Release);
        CoalescedHeartbeatBatch {
            tick,
            active_session_ids,
            thinking_count,
        }
    }

    /// Computes minimal appended delta between cumulative agent snapshots to avoid
    /// re-transmitting unchanged prefixes over the wire.
    pub fn compute_text_delta<'a>(previous: &str, current: &'a str) -> Option<&'a str> {
        if previous.is_empty() {
            return Some(current);
        }
        current.strip_prefix(previous)
    }
}

/// Spawns `session_count` lightweight session state/task supervisors inside a single
/// multi-threaded Tokio process and reports measured boot latency and `VmRSS`.
pub async fn run_session_probe(session_count: usize, hold_ms: u64) -> ProcessMemoryProbe {
    let t0 = Instant::now();
    let daemon = RunnerDaemon::new(DEFAULT_KEEP_ALIVE_INTERVAL_MS);
    let mut handles = Vec::with_capacity(session_count);
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    for i in 0..session_count {
        let sid = format!("session-{i}");
        daemon.register_session(ManagedSession {
            id: sid.clone(),
            flavor: if i % 2 == 0 { "codex".into() } else { "claude".into() },
            workdir: format!("/workspace/project-{i}"),
            thinking: i == 0,
            last_activity_ms: 1_700_000_000_000 + i as u64,
        });
        let daemon_clone = daemon.clone();
        let mut rx = shutdown_tx.subscribe();
        handles.push(tokio::spawn(async move {
            let mut buffer = String::with_capacity(512);
            buffer.push_str("ready");
            let _ = rx.recv().await;
            let _ = daemon_clone.sessions.contains_key(&sid);
        }));
    }

    let _batch = daemon.collect_coalesced_heartbeat();
    let boot_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if hold_ms > 0 {
        tokio::time::sleep(Duration::from_millis(hold_ms)).await;
    }
    let (vm_rss_kib, vm_size_kib) = read_proc_memory_kib();
    let _ = shutdown_tx.send(());
    for h in handles {
        let _ = h.await;
    }

    ProcessMemoryProbe {
        session_count,
        boot_ms,
        vm_rss_kib,
        vm_size_kib,
        heartbeat_interval_ms: DEFAULT_KEEP_ALIVE_INTERVAL_MS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manages_multiple_sessions_in_one_daemon_and_coalesces_heartbeats() {
        let daemon = RunnerDaemon::new(DEFAULT_KEEP_ALIVE_INTERVAL_MS);
        for i in 0..10 {
            daemon.register_session(ManagedSession {
                id: format!("s-{i}"),
                flavor: "codex".into(),
                workdir: "/tmp".into(),
                thinking: i % 3 == 0,
                last_activity_ms: i as u64,
            });
        }
        let batch = daemon.collect_coalesced_heartbeat();
        assert_eq!(batch.active_session_ids.len(), 10);
        assert_eq!(batch.thinking_count, 4);

        assert_eq!(RunnerDaemon::compute_text_delta("hello ", "hello world"), Some("world"));
        assert_eq!(RunnerDaemon::compute_text_delta("abc", "xyz"), None);
    }

    #[tokio::test]
    async fn session_probe_reports_positive_memory_and_fast_boot() {
        let probe = run_session_probe(10, 0).await;
        assert_eq!(probe.session_count, 10);
        assert!(probe.boot_ms < 200.0);
        assert!(probe.vm_rss_kib > 0);
    }
}
