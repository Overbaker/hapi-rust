use base64::{engine::general_purpose::STANDARD, Engine};
use hapi_wrapper_core::rpc::handlers::uploads;
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};
#[cfg(unix)]
fn authorized(file: &File, path: &Path, session: &str) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(m.is_file() && uploads::is_authorized_upload_file(path, session, m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn authorized(_: &File, _: &Path, _: &str) -> io::Result<bool> {
    Ok(false)
}
fn read(path: &Path, session: &str, remaining: usize) -> io::Result<Vec<u8>> {
    if !uploads::is_path_within_upload_dir(path, session) {
        return Err(io::Error::other("invalid upload path"));
    }
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !authorized(&file, path, session)? {
        return Err(io::Error::other("unauthorized upload"));
    }
    let mut bytes = Vec::new();
    file.take(remaining as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > remaining {
        return Err(io::Error::other("image exceeds upload limit"));
    }
    Ok(bytes)
}
pub fn prepare(v: &Value, session: &str, commands: &Value) -> (Value, Vec<String>) {
    let content = &v["content"];
    let raw = content["text"].as_str().unwrap_or("");
    let mut text = raw.to_owned();
    let skill = regex_lite::Regex::new(r"^(\s*)\$([a-z0-9]+(?:-[a-z0-9]+)*)(\s|$)").unwrap();
    if let Some(m) = skill.captures(raw) {
        let key = m.get(2).unwrap();
        let name = commands
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["source"] == "skill")
            .filter_map(|c| c["name"].as_str())
            .find(|s| s.trim_start_matches("skill:") == key.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("skill:{}", key.as_str()));
        text.replace_range(m.get(1).unwrap().end()..key.end(), &format!("/{name}"));
    }
    let mut images = Vec::new();
    let mut errors = Vec::new();
    let mut files = Vec::new();
    let mut remaining = 50 << 20;
    for a in content["attachments"].as_array().into_iter().flatten() {
        let Some(path) = a["path"].as_str() else {
            continue;
        };
        let mime = a["mimeType"].as_str().unwrap_or("");
        if !mime.to_ascii_lowercase().starts_with("image/") {
            files.push(format!("Attached file: {}", json!(path)));
            continue;
        }
        match read(Path::new(path), session, remaining) {
            Ok(bytes) => {
                remaining -= bytes.len();
                images.push(json!({"type":"image","mimeType":mime,"data":STANDARD.encode(bytes)}))
            }
            Err(e) => errors.push(format!(
                "Could not attach image {}: {e}",
                a["filename"].as_str().unwrap_or(path)
            )),
        }
    }
    if !files.is_empty() {
        let paths = files.join("\n");
        text = if text.is_empty() {
            paths
        } else if text == raw {
            format!("{paths}\n\n{text}")
        } else {
            format!("{text}\n\n{paths}")
        }
    }
    let mut cmd = json!({"type":"prompt","message":text});
    if !images.is_empty() {
        cmd["images"] = json!(images)
    }
    (cmd, errors)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skill_stays_first_and_attachment_path_is_quoted() {
        let v = json!({"content":{"text":"$review now","attachments":[{"path":"a\nb","mimeType":"text/plain"}]}});
        let (command, errors) = prepare(
            &v,
            "session",
            &json!([{"name":"skill:review","source":"skill"}]),
        );
        assert!(errors.is_empty());
        assert_eq!(
            command["message"],
            "/skill:review now\n\nAttached file: \"a\\nb\""
        );
    }
    #[test]
    fn unregistered_images_do_not_reach_pi() {
        let (cmd, errors) = prepare(
            &json!({"content":{"text":"","attachments":[{"path":"/unregistered","filename":"x","mimeType":"image/png"}]}}),
            "session",
            &Value::Null,
        );
        assert!(cmd.get("images").is_none());
        assert_eq!(errors.len(), 1);
    }
}
