use crate::rest::encode_component;
use crate::rest::Rest;
use hyper::Method;
#[cfg(test)]
use serde_json::json;
use serde_json::Value;

const RECIPE:&str=concat!("hapi job run <session-id> <job-key> --label \"<label>\" --progress unknown|fraction|until … -- <cmd>…",
" (required --progress: unknown=stripes; fraction=remaining+total|done+total; until=--until <iso> with auto remaining on heartbeats).",
" Fraction without a continuous updater freezes — keep updating done/remaining.",
" (use \"$HAPI_SESSION_ID\" only when it matches the operator /sessions/<id> chat row; remote Cursor: pass the URL uuid explicitly)");
const HINT:&str="On a remote runner, set HAPI_API_URL to the runner hub, and set CLI_API_TOKEN or run `hapi auth login`. Prefer `hapi job` over raw JWT+curl.";

fn sanitize(s: &str) -> String {
    static VT: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(r"(?:\x1b\]|\x{9d})[^\x07\x1b\x{9c}]*(?:\x07|\x1b\\|\x{9c})|(?:\x1b\[|\x{9b})[0-?]*[ -/]*[@-~]|\x1b[ -/]*[@-~]").unwrap()
    });
    VT.replace_all(s, "")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
fn line(job: &Value) -> String {
    let field = |k: &str| sanitize(job[k].as_str().unwrap_or(""));
    let mut parts = vec![field("key"), field("label"), field("status")];
    let unit = field("unit");
    let unit = if unit.is_empty() {
        unit
    } else {
        format!(" {unit}")
    };
    if job["remaining"].is_number() {
        parts.push(format!("{}{unit} left", job["remaining"]))
    } else if job["done"].is_number() && job["total"].is_number() {
        parts.push(format!("{}/{}{unit}", job["done"], job["total"]))
    }
    for (k, prefix) in [("detail", ""), ("runId", "runId ")] {
        if job[k].as_str().is_some_and(|v| !v.is_empty()) {
            parts.push(format!("{prefix}{}", field(k)))
        }
    }
    parts.join(" · ")
}
async fn auth(rest: &Rest) -> Result<String, String> {
    let res = rest
        .job_request(Method::POST, "/api/auth", None, None)
        .await;
    let detail = match res {
        Ok((status, v)) => {
            if status.is_success() {
                if let Some(token) = v["token"].as_str().filter(|s| !s.is_empty()) {
                    return Ok(token.into());
                }
            }
            v["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("HTTP {}", status.as_u16()))
        }
        Err(e) => e.to_string(),
    };
    Err(format!(
        "failed to exchange access token for JWT ({detail}). Hub URL: {}. {HINT}",
        rest.base
    ))
}
fn resolve(prefix: &str, sessions: &Value) -> Result<String, String> {
    let matches: Vec<_> = sessions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["id"].as_str())
        .filter(|id| id.starts_with(prefix))
        .collect();
    if matches.contains(&prefix) {
        return Ok(prefix.into());
    }
    match matches.as_slice() {
        [id] => Ok((*id).into()),
        [] if prefix.len() == 36 && uuid::Uuid::parse_str(prefix).is_ok() => Ok(prefix.into()),
        [] => Err(format!("no session matching prefix '{prefix}'")),
        _ => {
            let sample = matches
                .iter()
                .take(5)
                .map(|s| s.chars().take(8).collect::<String>())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "prefix '{prefix}' matches {} sessions ({sample}{}); use a longer prefix",
                matches.len(),
                if matches.len() > 5 { ", ..." } else { "" }
            ))
        }
    }
}
async fn request(rest: &Rest, id: &str, args: &Value) -> Result<String, String> {
    let action = args["action"].as_str().unwrap();
    let key = args["jobKey"].as_str().unwrap_or("");
    let mut jwt = auth(rest).await?;
    let (status, data) = rest
        .job_request(
            Method::GET,
            "/api/sessions?limit=500&order=updatedAt",
            Some(&jwt),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("list sessions failed: HTTP {}", status.as_u16()));
    }
    let id = resolve(id, &data["sessions"])?;
    let mut path = format!("/api/sessions/{}/jobs", encode_component(&id));
    let mut body = None;
    let method = match action {
        "list" => Method::GET,
        "clear" => {
            path.push_str(&format!("/{}", encode_component(key)));
            if let Some(run) = args["expectedRunId"].as_str() {
                path.push_str(&format!("?expectedRunId={}", encode_component(run)))
            }
            Method::DELETE
        }
        _ => {
            path.push_str(&format!("/{}", encode_component(key)));
            let mut patch = args.clone();
            let fields = patch.as_object_mut().unwrap();
            fields.remove("action");
            fields.remove("jobKey");
            body = Some(patch);
            Method::PATCH
        }
    };
    let mut res = rest
        .job_request(method.clone(), &path, Some(&jwt), body.clone())
        .await
        .map_err(|e| e.to_string())?;
    if res.0 == 401 {
        jwt = auth(rest).await?;
        res = rest
            .job_request(method, &path, Some(&jwt), body)
            .await
            .map_err(|e| e.to_string())?;
    }
    let (status, v) = res;
    if status == 404 && action != "list" {
        return if action == "clear" {
            Ok("job is already absent; nothing to clear".into())
        } else {
            Err(format!("job not found on this session (update requires an existing meter). Create one with Shell (MCP action=set is refused): {RECIPE} Wrap the real long-running command — not a sleep stub while work runs elsewhere."))
        };
    }
    if status == 409 && action != "list" {
        return Err(v["error"]
            .as_str()
            .unwrap_or("job run mismatch (expectedRunId); another run reused this key")
            .into());
    }
    if !status.is_success() || (action == "update" && !v["job"].is_object()) {
        let detail = if action == "update" {
            v["error"].as_str()
        } else {
            None
        }
        .map(str::to_owned)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        return Err(format!(
            "{action} {} failed: {detail}",
            if action == "list" { "jobs" } else { "job" }
        ));
    }
    match action {
        "list" => {
            let mut lines = vec![format!("session {id}")];
            for job in v["jobs"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "{} {}",
                    if v["primary"]["key"] == job["key"] {
                        "*"
                    } else {
                        " "
                    },
                    line(job)
                ))
            }
            if lines.len() == 1 {
                lines.push("(no jobs)".into())
            }
            Ok(lines.join("\n"))
        }
        "clear" => Ok(format!("cleared {key} on {id}")),
        _ => Ok(format!("updated {} on {id}", line(&v["job"]))),
    }
}
fn check(id: &str, args: &Value) -> Result<(), String> {
    if id.is_empty() {
        return Err(
            "own session id required (set HAPI_SESSION_ID / call from a HAPI-wrapped session)"
                .into(),
        );
    }
    match args["action"].as_str(){
        Some("set")=>Err(format!("action=set is refused over MCP. Creating a progress meter for work that outlives this turn requires a babysitter that heartbeats while the agent is idle — use the Shell tool: {RECIPE} MCP session_job is only for action=update (progress/heartbeat/terminal status), action=clear, or action=list on a job that job run (or CLI set + self-heartbeat wrapper) already created. Bare set + nohup freezes the bar when the agent goes idle (wardrobe dogfood). Bare job run --done 0 --total N with no continuous update freezes 0/N under live heartbeats.")),
        Some("list") if args.as_object().unwrap().len()>1=>Err("action=list does not accept job fields".into()),
        Some("list")=>Ok(()),
        _ if args["jobKey"].as_str().unwrap_or("").trim().is_empty()=>Err("jobKey is required for update/clear".into()),
        _ if args.get("startedAt").is_some()=>Err("startedAt is not valid over MCP (set is refused; use CLI job set --started-at)".into()),
        Some("clear") if ["label","status","done","total","remaining","unit","detail"].iter().any(|k|args.get(k).is_some())=>Err("action=clear only accepts jobKey and expectedRunId".into()),
        _=>Ok(())
    }
}
pub(super) async fn call(rest: &Rest, id: &str, args: &Value) -> Result<String, String> {
    let id = if id.is_empty() {
        std::env::var("HAPI_SESSION_ID").unwrap_or_default()
    } else {
        id.into()
    };
    check(id.trim(), args)?;
    request(rest, id.trim(), args)
        .await
        .map_err(|e| format!("session_job failed: {e}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn own_session_actions() {
        for args in [
            json!({"action":"set"}),
            json!({"action":"clear","jobKey":"k","detail":null}),
            json!({"action":"list","jobKey":"k"}),
            json!({"action":"update","jobKey":"k","startedAt":0}),
        ] {
            assert!(check("own", &args).is_err())
        }
        assert!(check(
            "own",
            &json!({"action":"update","jobKey":"k","remaining":null})
        )
        .is_ok());
        assert!(check("own", &json!({"action":"update","jobKey":"k"})).is_ok());
        assert!(resolve("ab", &json!([{"id":"abc"},{"id":"abd"}])).is_err());
        assert_eq!(
            line(
                &json!({"key":"k","label":"\x1b[31mred\x1b[0m","status":"running","done":1,"total":2})
            ),
            "k · red · running · 1/2"
        );
    }
}

#[cfg(test)]
mod hub_tests {
    use super::*;
    use crate::{
        config::Config,
        loopback::{reply, Listener},
    };
    use http_body_util::BodyExt;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    #[tokio::test]
    async fn job_routes_retry_auth_and_keep_own_session() {
        let count = Arc::new(AtomicUsize::default());
        let auth = count.clone();
        let server=Listener::bind(move |req|{let auth=auth.clone();async move {
            let (parts,body)=req.into_parts();
            let bytes=body.collect().await.unwrap().to_bytes();
            assert_eq!(parts.headers["x-test"],"example");
            if parts.uri.path()=="/api/auth"{
                assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(),json!({"accessToken":"secret"}));
                assert!(parts.headers.get("authorization").is_none());
                let n=auth.fetch_add(1,Ordering::Relaxed)+1;
                return reply(200,"application/json",json!({"token":format!("jwt-{n}")}).to_string())
            }
            let jwt=&parts.headers["authorization"];
            match (parts.method.as_str(),parts.uri.path()){
                ("GET","/api/sessions")=>{assert_eq!(parts.uri.query(),Some("limit=500&order=updatedAt"));reply(200,"application/json",r#"{"sessions":[{"id":"own-id"},{"id":"other"}]}"#)}
                ("GET","/api/sessions/own-id/jobs")=>reply(200,"application/json",r#"{"jobs":[{"key":"k","label":"test","status":"running","remaining":2,"unit":"files"}],"primary":{"key":"k"}}"#),
                ("PATCH","/api/sessions/own-id/jobs/key%2Fone")=>{
                    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(),json!({"remaining":null,"expectedRunId":"run"}));
                    if jwt=="Bearer jwt-1"{reply(401,"application/json","{}")}else{reply(200,"application/json",r#"{"job":{"key":"k","status":"running","label":"test"}}"#)}
                }
                ("DELETE","/api/sessions/own-id/jobs/k")=>{assert_eq!(parts.uri.query(),Some("expectedRunId=a%2Fb"));reply(404,"application/json","{}")}
                ("PATCH","/api/sessions/own-id/jobs/missing")=>reply(404,"application/json","{}"),
                _=>reply(500,"application/json","{}")
            }
        }}).await.unwrap();
        let rest = Rest::new(&Config {
            home: "/tmp".into(),
            api_url: format!("http://127.0.0.1:{}", server.port),
            cli_api_token: "secret".into(),
            extra_headers: vec![("x-test".into(), "example".into())],
            machine_id: None,
        })
        .await
        .unwrap();
        assert_eq!(call(&rest,"own",&json!({"action":"update","jobKey":"key/one","remaining":null,"expectedRunId":"run"})).await.unwrap(),"updated k · test · running on own-id");
        assert_eq!(count.load(Ordering::Relaxed), 2);
        assert_eq!(
            call(&rest, "own", &json!({"action":"list"})).await.unwrap(),
            "session own-id\n* k · test · running · 2 files left"
        );
        assert_eq!(
            call(
                &rest,
                "own",
                &json!({"action":"clear","jobKey":"k","expectedRunId":"a/b"})
            )
            .await
            .unwrap(),
            "job is already absent; nothing to clear"
        );
        assert!(
            call(&rest, "own", &json!({"action":"update","jobKey":"missing"}))
                .await
                .unwrap_err()
                .contains(RECIPE)
        );
    }
}
