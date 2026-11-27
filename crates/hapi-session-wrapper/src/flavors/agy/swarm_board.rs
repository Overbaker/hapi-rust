//! `/swarm board`: a Markdown snapshot of agy's native swarm roster and board
//! (TS `agySwarmBoard.ts`).
use hapi_wrapper_core::jsjson::parse as parse_json;
use serde_json::Value;
use std::path::Path;

const MAX_DISPLAYED_BOARD_POSTS: usize = 50;
const NO_BOARD: &str =
    "No active swarm board for this session yet. Start one with `/swarm <task>`.";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgySwarmBoardCommand {
    pub channel: Option<String>,
}

fn is_safe_session_segment(id: &str) -> bool {
    !id.is_empty()
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains("..")
        && !id.contains('\0')
}

/// Matches `/swarm board`, `/swarm board --channel <name>` and `/swarm board #<name>`;
/// task prompts such as `/swarm refactor the parser` go to agy.
pub fn parse_agy_swarm_board_command(message: &str) -> Option<AgySwarmBoardCommand> {
    let re = regex_lite::Regex::new(
        r"(?i)^/swarm\s+board(?:\s+(?:--channel(?:=|\s+)|#)([a-zA-Z0-9_-]+))?\s*$",
    )
    .unwrap();
    let caps = re.captures(message.trim())?;
    Some(AgySwarmBoardCommand {
        channel: caps
            .get(1)
            .map(|m| m.as_str().trim().to_owned())
            .filter(|s| !s.is_empty()),
    })
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}
fn member_state(member: &Value) -> String {
    if member["killed"] == true {
        return "killed".into();
    }
    let phase = s(member, "phase")
        .or_else(|| s(member, "desired_state"))
        .unwrap_or("")
        .trim()
        .to_uppercase();
    match phase.as_str() {
        "WAITING" | "" => "waiting".into(),
        "RUNNING" | "WORKING" => "working".into(),
        "STOPPED" => "stopped".into(),
        "KILLED" => "killed".into(),
        other => other.to_lowercase(),
    }
}
fn cell(value: &str) -> String {
    value
        .replace('|', "\\|")
        .replace("\r\n", " ")
        .replace('\n', " ")
        .trim()
        .to_owned()
}
fn read_posts(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| parse_json::<Value>(l.as_bytes()).ok())
        .filter(Value::is_object)
        .collect()
}

/// Reads `<brain_dir>/<session_id>/.system_generated/swarm/{members.json,board.jsonl}`.
pub fn format_agy_swarm_board(
    brain_dir: &Path,
    session_id: Option<&str>,
    command: &AgySwarmBoardCommand,
) -> String {
    let Some(session_id) = session_id.filter(|id| is_safe_session_segment(id)) else {
        return NO_BOARD.into();
    };
    let dir = brain_dir
        .join(session_id)
        .join(".system_generated")
        .join("swarm");
    let roster = std::fs::read_to_string(dir.join("members.json"))
        .ok()
        .and_then(|r| parse_json::<Value>(r.as_bytes()).ok())
        .unwrap_or(Value::Null);
    let posts = read_posts(&dir.join("board.jsonl"));
    let mut members: Vec<(&String, &Value)> = roster["members"]
        .as_object()
        .map(|m| m.iter().collect())
        .unwrap_or_default();
    if members.is_empty() && posts.is_empty() {
        return NO_BOARD.into();
    }
    let mut lines = Vec::new();
    let title = s(&roster, "title").map(str::trim).filter(|t| !t.is_empty());
    let filter = command
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let suffix = [
        title.map(|t| format!("— {t}")),
        filter.map(|c| format!("(#{c})")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    lines.push(if suffix.is_empty() {
        "### Swarm Board".to_owned()
    } else {
        format!("### Swarm Board {suffix}")
    });
    if !members.is_empty() {
        lines.push(String::new());
        lines.push("| Member | Role | State | Model | Workspace |".into());
        lines.push("| :--- | :--- | :--- | :--- | :--- |".into());
        members.sort_by(|(a, _), (b, _)| match (a.as_str(), b.as_str()) {
            ("coordinator", _) => std::cmp::Ordering::Less,
            (_, "coordinator") => std::cmp::Ordering::Greater,
            _ => a
                .to_lowercase()
                .cmp(&b.to_lowercase())
                .then_with(|| a.cmp(b)),
        });
        for (name, m) in members {
            let default_role = if name == "coordinator" {
                "coordinator"
            } else {
                "worker"
            };
            let role = cell(s(m, "role").unwrap_or(default_role));
            let state = cell(&member_state(m));
            let model = cell(
                s(m, "applied_model")
                    .or_else(|| s(m, "model"))
                    .or_else(|| s(m, "desired_model"))
                    .unwrap_or("default"),
            );
            let workspace = cell(s(m, "workspace_mode").unwrap_or("inherit"));
            lines.push(format!(
                "| `@{}` | {role} | {state} | `{model}` | {workspace} |",
                cell(name)
            ));
        }
    }
    let filtered: Vec<&Value> = posts
        .iter()
        .filter(|p| filter.is_none_or(|c| s(p, "channel").unwrap_or("main") == c))
        .collect();
    lines.push(String::new());
    lines.push(match filter {
        Some(c) => format!("#### Board Messages (#{c})"),
        None => "#### Board Messages".into(),
    });
    if filtered.is_empty() {
        lines.push(match filter {
            Some(c) => format!("_No messages in #{c} yet._"),
            None => "_No messages posted yet._".into(),
        });
        return lines.join("\n");
    }
    let omitted = filtered.len().saturating_sub(MAX_DISPLAYED_BOARD_POSTS);
    if omitted > 0 {
        lines.push(format!(
            "_Showing latest {MAX_DISPLAYED_BOARD_POSTS} of {} messages ({omitted} earlier omitted)._",
            filtered.len()
        ));
    }
    for post in &filtered[omitted..] {
        let seq = match &post["seq"] {
            Value::Number(n) => format!("#{n}"),
            _ => "#?".into(),
        };
        let author = s(post, "agent")
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or("unknown");
        let channel = s(post, "channel").map(str::trim);
        let badge = match channel {
            Some(c) if !c.is_empty() && c != "main" && filter.is_none() => format!(" `#{c}`"),
            _ => String::new(),
        };
        let time = match s(post, "time").map(str::trim) {
            Some(t) if !t.is_empty() => format!(" ({t})"),
            _ => String::new(),
        };
        let text = s(post, "text").unwrap_or("").trim();
        lines.push(format!("- **{seq}** `@{author}`{badge}{time}: {text}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_board_commands() {
        assert_eq!(
            parse_agy_swarm_board_command(" /swarm board "),
            Some(AgySwarmBoardCommand::default())
        );
        let parser = Some(AgySwarmBoardCommand {
            channel: Some("parser".into()),
        });
        assert_eq!(
            parse_agy_swarm_board_command("/swarm board --channel parser"),
            parser
        );
        assert_eq!(
            parse_agy_swarm_board_command("/SWARM Board --channel=parser"),
            parser
        );
        assert_eq!(
            parse_agy_swarm_board_command("/swarm board #parser"),
            parser
        );
        assert_eq!(
            parse_agy_swarm_board_command("/swarm refactor the parser"),
            None
        );
    }

    #[test]
    fn formats_roster_and_posts() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            format_agy_swarm_board(dir.path(), None, &Default::default()),
            NO_BOARD
        );
        assert_eq!(
            format_agy_swarm_board(dir.path(), Some("../x"), &Default::default()),
            NO_BOARD
        );
        assert_eq!(
            format_agy_swarm_board(dir.path(), Some("s"), &Default::default()),
            NO_BOARD
        );
        let swarm = dir.path().join("s/.system_generated/swarm");
        std::fs::create_dir_all(&swarm).unwrap();
        std::fs::write(
            swarm.join("members.json"),
            r#"{"title":"Parser","members":{"worker":{"phase":"RUNNING","model":"m|x"},"coordinator":{"desired_state":"WAITING"}}}"#,
        )
        .unwrap();
        std::fs::write(
            swarm.join("board.jsonl"),
            "{\"seq\":1,\"agent\":\"coordinator\",\"text\":\" hi \"}\nnot json\n{\"seq\":2,\"agent\":\"worker\",\"channel\":\"parser\",\"time\":\"10:00\",\"text\":\"done\"}\n",
        )
        .unwrap();
        assert_eq!(
            format_agy_swarm_board(dir.path(), Some("s"), &Default::default()),
            "### Swarm Board — Parser\n\n| Member | Role | State | Model | Workspace |\n| :--- | :--- | :--- | :--- | :--- |\n| `@coordinator` | coordinator | waiting | `default` | inherit |\n| `@worker` | worker | working | `m\\|x` | inherit |\n\n#### Board Messages\n- **#1** `@coordinator`: hi\n- **#2** `@worker` `#parser` (10:00): done"
        );
        let filtered = format_agy_swarm_board(
            dir.path(),
            Some("s"),
            &AgySwarmBoardCommand {
                channel: Some("parser".into()),
            },
        );
        assert!(filtered.starts_with("### Swarm Board — Parser (#parser)"));
        assert!(
            filtered.ends_with("#### Board Messages (#parser)\n- **#2** `@worker` (10:00): done")
        );
    }
}
