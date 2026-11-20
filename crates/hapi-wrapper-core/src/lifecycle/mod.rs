#[cfg(target_os = "linux")]
pub mod proctree;
pub mod start_marker;

#[cfg(target_os = "linux")]
use std::future::Future;
#[cfg(target_os = "linux")]
use tokio::{
    sync::watch,
    time::{sleep_until, Instant},
};

#[cfg(target_os = "linux")]
type Deadline = watch::Receiver<Option<Instant>>;

#[cfg(target_os = "linux")]
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
