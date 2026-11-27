use crate::{rest::Rest, session::metadata::now};
use regex_lite::Regex;
use serde_json::{json, Value};
use std::{future::Future, sync::LazyLock, time::Duration};
use tokio::sync::{watch, Mutex};
use url::Url;

pub(super) const NAME: &str = "link_pr";
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(15);
pub(super) const LIST_CHANGED: &str =
    r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;

#[derive(Default)]
pub(super) struct Visibility {
    refresh: Mutex<()>,
    enabled: watch::Sender<bool>,
}

impl Visibility {
    pub(super) async fn check(&self, fetch: impl Future<Output = Option<bool>>) -> bool {
        let _guard = self.refresh.lock().await;
        if let Some(enabled) = fetch.await {
            self.enabled.send_if_modified(|previous| {
                let changed = *previous != enabled;
                *previous = enabled;
                changed
            });
        }
        *self.enabled.borrow()
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<bool> {
        self.enabled.subscribe()
    }
}

pub(super) fn listed(catalog: &Value) -> bool {
    catalog["tools"]
        .as_array()
        .is_some_and(|items| items.iter().any(|tool| tool["name"] == NAME))
}

pub(super) fn hide(catalog: &mut Value) {
    if let Some(items) = catalog["tools"].as_array_mut() {
        items.retain(|tool| tool["name"] != NAME);
    }
}

const DEFAULT_HOST: &str = "github.com";
const ENTERPRISE_SUFFIX: &str = ".ghe.com";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
static SLUG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").unwrap());
static SHORTHAND: LazyLock<Regex> = LazyLock::new(|| {
    let ws = r"\t-\r \xA0\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
    Regex::new(&format!(
        r"(?i)^([^/{ws}]+/[^/{ws}]+)[{ws}]*#[{ws}]*(?:PR[{ws}]*)?([0-9]+)$"
    ))
    .unwrap()
});
static PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/([^/]+/[^/]+)/pull/([0-9]+)/?$").unwrap());

fn trim(input: &str) -> &str {
    input.trim_matches(|c: char| c == '\u{feff}' || (c != '\u{85}' && c.is_whitespace()))
}

struct PullRequest {
    repo: String,
    number: u64,
    host: String,
}

impl PullRequest {
    fn parse(raw: &str, configured_hosts: &[String]) -> Result<Self, &'static str> {
        let raw = trim(raw);
        if raw.is_empty() {
            return Err("empty input");
        }
        if let Some(parts) = SHORTHAND.captures(raw) {
            return Self::from_parts(&parts[1], &parts[2], DEFAULT_HOST)
                .ok_or("invalid owner/repo#N");
        }

        let url = Url::parse(raw).map_err(|_| "expected GitHub PR URL or owner/repo#N")?;
        let host = url.host_str().unwrap_or_default();
        let allowed = host == DEFAULT_HOST
            || host
                .strip_suffix(ENTERPRISE_SUFFIX)
                .is_some_and(|label| !label.is_empty())
            || configured_hosts
                .iter()
                .any(|value| host.eq_ignore_ascii_case(trim(value)));
        if url.scheme() != "https" || host.is_empty() || !allowed {
            return Err("expected https://<github-or-ghes-host>/.../pull/N URL");
        }
        let parts = PATH
            .captures(url.path())
            .ok_or("expected https://<host>/owner/repo/pull/N URL")?;
        Self::from_parts(&parts[1], &parts[2], host).ok_or("invalid GitHub PR URL")
    }

    fn from_parts(repo: &str, number: &str, host: &str) -> Option<Self> {
        let number = number.parse().ok()?;
        (repo.len() <= 140 && SLUG.is_match(repo) && (1..=MAX_SAFE_INTEGER).contains(&number)).then(
            || Self {
                repo: repo.into(),
                number,
                host: host.into(),
            },
        )
    }

    fn matches(&self, candidate: &Value, role: &str) -> bool {
        candidate["kind"] == "github_pr"
            && candidate["role"] == role
            && candidate["repo"]
                .as_str()
                .is_some_and(|repo| repo.eq_ignore_ascii_case(&self.repo))
            && candidate["number"].as_f64() == Some(self.number as f64)
            && candidate["url"]
                .as_str()
                .and_then(|url| Url::parse(url).ok())
                .as_ref()
                .and_then(Url::host_str)
                .unwrap_or(DEFAULT_HOST)
                .eq_ignore_ascii_case(&self.host)
    }
}

pub(super) async fn call(
    rest: &Rest,
    id: &str,
    args: &Value,
    hosts: &[String],
) -> Result<String, String> {
    let shorthand;
    let raw = match args["url"].as_str().map(trim).filter(|url| !url.is_empty()) {
        Some(url) => url,
        None => match (
            args["repo"].as_str().filter(|repo| !repo.is_empty()),
            args["number"].as_f64(),
        ) {
            (Some(repo), Some(number)) => {
                shorthand = format!("{repo}#{number}");
                &shorthand
            }
            _ => "",
        },
    };
    let pr = PullRequest::parse(raw, hosts)?;
    let role = args["role"].as_str().unwrap_or("primary");
    let body = rest
        .upsert_external_ref(
            id,
            json!({
                "kind": "github_pr",
                "repo": pr.repo,
                "number": pr.number,
                "url": format!("https://{}/{}/pull/{}", pr.host, pr.repo, pr.number),
                "role": role,
                "source": "agent",
                "linkedAt": now(),
            }),
        )
        .await
        .map_err(|error| error.to_string())?;
    if !body["externalRefs"]
        .as_array()
        .is_some_and(|refs| refs.iter().any(|candidate| pr.matches(candidate, role)))
    {
        return Err("Hub did not persist the PR link (awareness may be disabled)".into());
    }
    Ok(format!("Linked {}#{} to this session", pr.repo, pr.number))
}
