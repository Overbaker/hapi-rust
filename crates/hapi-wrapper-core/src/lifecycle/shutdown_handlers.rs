use super::{runner_lifecycle::RunnerLifecycle, start_marker::start_marker};
use crate::rpc::manager::RpcHandlerManager;
use futures_util::future::BoxFuture;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use std::{
    io,
    sync::{Arc, Weak},
};
use tokio::sync::watch;

pub type BeforeKill =
    Arc<dyn Fn(Option<Value>) -> BoxFuture<'static, Result<bool, String>> + Send + Sync>;
pub(super) fn uptime_ms() -> Result<f64, String> {
    let mut ts = std::mem::MaybeUninit::uninit();
    if unsafe { nix::libc::clock_gettime(nix::libc::CLOCK_BOOTTIME, ts.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(unsafe { ts.assume_init() }.tv_sec as f64 * 1000.)
}
fn scoped_name(machine: &str) -> String {
    const SAFE: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~')
        .remove(b'!')
        .remove(b'*')
        .remove(39)
        .remove(b'(')
        .remove(b')');
    format!(
        "killSession/process/{}/{}",
        utf8_percent_encode(machine, SAFE),
        std::process::id()
    )
}
struct Target {
    machine: Option<String>,
    lifecycle: Weak<RunnerLifecycle>,
    before: Option<BeforeKill>,
}
impl Target {
    async fn kill(&self, request: Value, scoped: bool) -> Result<Option<Value>, String> {
        let pid = std::process::id();
        let prepare = request["prepareOnly"] == true;
        let machine_matches = request["expectedMachineId"].is_string()
            && request["expectedMachineId"].as_str() == self.machine.as_deref();
        if scoped && (!prepare || !machine_matches || request["expectedPid"] != pid) {
            return Err("Process-scoped shutdown requires the exact preparation target".into());
        }
        if request.get("expectedMachineId").is_some() && !machine_matches {
            return Err("Session machine does not match shutdown target".into());
        }
        let marker = start_marker(pid).await;
        if request.get("expectedPid").is_some()
            && (request["expectedPid"] != pid
                || marker.is_none()
                || request["expectedProcessStartMarker"].as_str() != marker.as_deref())
        {
            return Err("Session process generation does not match shutdown target".into());
        }
        let lease = prepare.then(|| request["preparationLease"].clone());
        let mut expiry = None;
        if let Some(l) = &lease {
            let now = uptime_ms()?;
            expiry = l["expiresAt"].as_f64();
            if l["id"].as_str().is_none_or(str::is_empty)
                || !expiry.is_some_and(|t| t > now && t <= now + 30_000.)
            {
                return Err("Session shutdown preparation lease is invalid or expired".into());
            }
        }
        let lifecycle = self.lifecycle.upgrade().ok_or("Session closed")?;
        lifecycle.set_archive_reason("User terminated");
        let confirmed = if let Some(before) = &self.before {
            before(lease.clone()).await?
        } else {
            true
        };
        if !confirmed || (expiry.is_some() && expiry.unwrap() <= uptime_ms()?) {
            return Err("Session shutdown preparation was not confirmed".into());
        }
        if !prepare {
            lifecycle.request_exit();
        }
        let mut result = json!({"success":true,"pid":pid,"message":if prepare{"Session shutdown prepared"}else{"Killing hapi CLI process"}});
        if let Some(marker) = marker {
            result["processStartMarker"] = marker.into()
        }
        if let Some(l) = lease {
            result["preparationLease"] = l
        }
        if let Some(m) = self.machine.as_ref().filter(|m| !m.is_empty()) {
            result["machineId"] = json!(m)
        }
        Ok(Some(result))
    }
}
pub fn register_shutdown_handlers(
    rpc: &RpcHandlerManager,
    lifecycle: &Arc<RunnerLifecycle>,
    machine: Option<String>,
    before: Option<BeforeKill>,
    mut archived: watch::Receiver<bool>,
) {
    let target = Arc::new(Target {
        machine,
        lifecycle: Arc::downgrade(lifecycle),
        before,
    });
    if let Some(m) = target
        .machine
        .as_ref()
        .filter(|m| !m.is_empty() && target.before.is_some())
    {
        let name = scoped_name(m);
        let t = target.clone();
        rpc.register(&name, move |r| {
            let t = t.clone();
            async move { t.kill(r, true).await }
        });
    }
    rpc.register("killSession", move |r| {
        let t = target.clone();
        async move { t.kill(r, false).await }
    });
    let weak = Arc::downgrade(lifecycle);
    rpc.register("handoff-local", move |_| {
        let weak = weak.clone();
        async move {
            let l = weak.upgrade().ok_or("Session closed")?;
            l.set_archive_reason("Handed off to local terminal");
            l.set_session_end_reason("handoff");
            l.set_exit_code(0);
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                l.request_exit();
            });
            Ok(Some(json!({"ok":true})))
        }
    });
    let weak = Arc::downgrade(lifecycle);
    let stop = move || {
        if let Some(l) = weak.upgrade() {
            l.set_archive_reason("User terminated");
            l.request_exit();
        }
    };
    if *archived.borrow() {
        stop()
    } else {
        tokio::spawn(async move {
            if archived.wait_for(|v| *v).await.is_ok() {
                stop()
            }
        });
    }
}
