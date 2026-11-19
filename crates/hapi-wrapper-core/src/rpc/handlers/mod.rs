use super::manager::RpcHandlerManager;
use serde_json::{json, Value};
use std::{
    io,
    path::{Component, Path, PathBuf},
};
mod commands;
mod discovery;
mod files;
mod models;
pub mod uploads;
pub use files::GeneratedImages;

fn error(s: impl ToString) -> Value {
    json!({"success":false,"error":s.to_string()})
}
fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn lexical(root: &Path, raw: &str) -> Result<PathBuf, String> {
    let mut path = PathBuf::new();
    for c in root.join(raw).components() {
        match c {
            Component::ParentDir => {
                path.pop();
            }
            Component::CurDir => {}
            _ => path.push(c.as_os_str()),
        }
    }
    if path.starts_with(root) {
        Ok(path)
    } else {
        Err(format!(
            "Access denied: Path '{raw}' is outside the working directory"
        ))
    }
}
fn fs_error(e: io::Error, op: &str, p: &Path) -> String {
    let Some(code) = e.raw_os_error() else {
        return e.to_string();
    };
    let errno = nix::errno::Errno::from_raw(code);
    let msg = e.to_string();
    format!(
        "{errno:?}: {}, {op} '{}'",
        msg.split(" (os error")
            .next()
            .unwrap_or(&msg)
            .to_lowercase(),
        p.display()
    )
}
fn canonical(root: &Path, p: &Path) -> Result<PathBuf, String> {
    let resolved = p.canonicalize().map_err(|e| fs_error(e, "realpath", p))?;
    if resolved.starts_with(
        root.canonicalize()
            .map_err(|e| fs_error(e, "realpath", root))?,
    ) {
        Ok(resolved)
    } else {
        Err("Access denied: resolved path is outside the working directory".into())
    }
}
pub fn register_common_handlers(manager: &RpcHandlerManager, root: &Path) -> GeneratedImages {
    let images = GeneratedImages::default();
    commands::register(manager, root);
    models::register(manager);
    files::register(manager, root, &images);
    commands::register_tools(manager, root, &["ripgrep", "difftastic"]);
    discovery::register(manager, root);
    commands::register_tools(
        manager,
        root,
        &["git-status", "git-diff-numstat", "git-diff-file"],
    );
    uploads::register(manager);
    images
}

#[cfg(test)]
mod tests;
