use hapi_wrapper_core::session::metadata::now;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(super) struct Quotas {
    directory: PathBuf,
    account: Option<String>,
    resets: HashMap<(String, String), u64>,
}
fn hash(s: &str) -> String {
    Sha256::digest(s)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
fn account(s: &str) -> Option<String> {
    let s = s.trim().to_lowercase();
    if s.is_empty() || s.starts_with("auth:") {
        None
    } else if ["acct:", "url:", "cred:", "org:"]
        .iter()
        .any(|p| s.starts_with(p))
    {
        Some(s)
    } else if s.starts_with("https://") || s.starts_with("http://") {
        Some(format!("url:{s}"))
    } else {
        Some(format!("acct:{s}"))
    }
}
fn config_identity(dir: &Path) -> Option<String> {
    let value: Value = serde_json::from_slice(&fs::read(dir.join(".claude.json")).ok()?).ok()?;
    ["emailAddress", "accountUuid"]
        .into_iter()
        .find_map(|k| account(value["oauthAccount"][k].as_str()?))
}
impl Quotas {
    pub fn new(home: &Path) -> Self {
        let account = [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]
        .into_iter()
        .find_map(env)
        .map(|token| format!("cred:{}", hash(&token)))
        .or_else(|| env("CLAUDE_CONFIG_DIR").and_then(|dir| config_identity(Path::new(&dir))))
        .or_else(|| std::env::home_dir().and_then(|dir| config_identity(&dir)))
        .or_else(|| env("ANTHROPIC_BASE_URL").map(|s| format!("url:{}", s.to_lowercase())));
        Self {
            directory: home.join("agent-quotas-live"),
            account,
            resets: HashMap::new(),
        }
    }
    pub fn bind(&mut self, identity: Option<&str>) {
        if let Some(key) = identity.and_then(account) {
            self.account = Some(key);
        }
    }
    pub fn observe(&mut self, v: &Value) {
        if v["type"] == "system" && v["subtype"] == "init" {
            self.bind(
                v["account"]["email"]
                    .as_str()
                    .or_else(|| v["email"].as_str()),
            );
        }
        if v["type"] != "rate_limit_event" {
            return;
        }
        let info = &v["rate_limit_info"];
        let Some(key) = self.account.clone().or_else(|| {
            ["accountEmail", "email"]
                .into_iter()
                .find_map(|k| account(info[k].as_str()?))
        }) else {
            return;
        };
        let rejected = info["status"]
            .as_str()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("rejected"));
        let used = if rejected {
            100.0
        } else if let Some(n) = info["utilization"].as_f64() {
            (((if n <= 1.0 { 100. * n } else { n }) * 10.).round() / 10.).clamp(0., 100.)
        } else {
            return;
        };
        let kind = info["rateLimitType"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("default");
        let segment: String = kind
            .encode_utf16()
            .take(48)
            .map(|c| {
                if c < 128 && ((c as u8).is_ascii_alphanumeric() || matches!(c, 45 | 95)) {
                    c as u8 as char
                } else {
                    '_'
                }
            })
            .collect();
        let path = self
            .directory
            .join(format!("claude-bucket-{}-{segment}.json", hash(&key)));
        let cache_key = (key.clone(), kind.to_owned());
        let reset = info["resetsAt"]
            .as_f64()
            .filter(|n| *n > 0.)
            .map(|n| (if n < 1e12 { n * 1000. } else { n }).round() as u64)
            .or_else(|| {
                let ms =
                    hapi_wrapper_core::session::parse_iso_timestamp_ms(info["resetsAt"].as_str()?)?;
                (ms > 0).then_some(ms as u64)
            })
            .or_else(|| self.resets.get(&cache_key).cloned())
            .or_else(|| {
                let old: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
                (old["accountKey"] == key && old["rateLimitType"] == kind)
                    .then(|| old["resetsAt"].as_u64())
                    .flatten()
            });
        if let Some(ms) = reset {
            self.resets.insert(cache_key, ms);
        }
        let lower = kind.to_ascii_lowercase();
        let (window, label) = if lower.contains("5h") || lower.contains("five_hour") {
            ("5h", "5h window".into())
        } else if ["7d", "seven_day", "week"]
            .iter()
            .any(|p| lower.contains(p))
        {
            ("weekly", "Weekly (7d)".into())
        } else {
            (kind, format!("Limit ({kind})"))
        };
        let bucket = json!({"id":format!("claude-{kind}"),"label":label,"window":window,"group":null,"usedPercent":used,"remainingPercent":((100.-used)*10.).round()/10.,"resetsAt":reset,"updatedAt":now(),"accountKey":key,"rateLimitType":kind});
        let tmp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
        if fs::create_dir_all(&self.directory).is_ok()
            && fs::write(&tmp, bucket.to_string()).is_ok()
            && fs::rename(&tmp, path).is_err()
        {
            let _ = fs::remove_file(tmp);
        }
    }
}

#[test]
fn hidden_allowed_event_records_verified_bucket_and_preserves_reset() {
    let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
    let mut quotas = Quotas {
        directory: dir.clone(),
        resets: HashMap::new(),
        account: None,
    };
    quotas.bind(Some(" Example@Local "));
    quotas.observe(&json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","utilization":0.37,"resetsAt":1800000000}}));
    quotas.observe(&json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}));
    let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let v: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(v["usedPercent"], 100.0);
    assert_eq!(v["accountKey"], "acct:example@local");
    assert_eq!(v["resetsAt"], 1800000000000u64);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_reset_reads_another_sessions_iso_reset() {
    let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
    let session = || Quotas {
        directory: dir.clone(),
        resets: HashMap::new(),
        account: Some("acct:test".into()),
    };
    let mut first = session();
    let mut event = json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}});
    first.observe(&event);
    event["rate_limit_info"]["resetsAt"] = "2024-01-01T01:00:00.123+01:00".into();
    session().observe(&event);
    event["rate_limit_info"]["resetsAt"] = Value::Null;
    first.observe(&event);
    let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let bucket: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(bucket["resetsAt"], 1704067200123u64);
    fs::remove_dir_all(dir).unwrap();
}
