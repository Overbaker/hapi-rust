pub mod proctree;
#[cfg(any(target_os = "linux", windows))]
pub mod runner_lifecycle;
#[cfg(any(target_os = "linux", windows))]
pub mod shutdown_handlers;
pub mod start_marker;

pub const STARTUP_FALLBACK: i32 = 86;

pub fn startup_fallback(session_id: Option<&str>) -> i32 {
    if let Some(id) = session_id {
        eprintln!("HAPI_RUST_WRAPPER_SESSION_ID={id}");
    }
    STARTUP_FALLBACK
}

#[cfg(any(target_os = "linux", windows))]
use std::future::Future;
#[cfg(any(target_os = "linux", windows))]
use tokio::{
    sync::watch,
    time::{sleep_until, Instant},
};

#[cfg(any(target_os = "linux", windows))]
type Deadline = watch::Receiver<Option<Instant>>;

#[cfg(any(target_os = "linux", windows))]
async fn before<F: Future>(
    mut limit: Instant,
    mut changes: Option<&mut Deadline>,
    future: F,
) -> Option<F::Output> {
    tokio::pin!(future);
    loop {
        let Some(rx) = changes.as_mut() else {
            return tokio::time::timeout_at(limit, future).await.ok();
        };
        if let Some(new) = *rx.borrow_and_update() {
            limit = limit.min(new)
        }
        tokio::select! {
            biased;
            _=sleep_until(limit)=>return None,
            result=rx.changed()=>{if result.is_err(){changes=None;}},
            value=&mut future=>return Some(value),
        }
    }
}
