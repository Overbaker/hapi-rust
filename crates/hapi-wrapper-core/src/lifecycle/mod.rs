pub mod proctree;
pub mod runner_lifecycle;
pub mod shutdown_handlers;
pub mod start_marker;

use std::future::Future;
use tokio::{
    sync::watch,
    time::{sleep_until, Instant},
};

type Deadline = watch::Receiver<Option<Instant>>;

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
