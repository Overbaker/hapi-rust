use hapi_wrapper_core::{jsjson::parse as parse_json, session::claude_external_text};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    time::Duration,
};

pub(super) async fn probe_resume_at() -> bool {
    let mut command = super::query::configured_command(None);
    let result =
        tokio::time::timeout(Duration::from_secs(10), command.arg("--version").output()).await;
    matches!(result, Ok(Ok(output))
        if output.status.success() && supports_resume_at(&String::from_utf8_lossy(&output.stdout)))
}

fn supports_resume_at(output: &str) -> bool {
    let pattern = regex_lite::Regex::new(r"(\d+)\.(\d+)\.(\d+)").unwrap();
    let Some(parts) = pattern.captures(output) else {
        return false;
    };
    let version = || {
        Some((
            parts[1].parse::<u32>().ok()?,
            parts[2].parse::<u32>().ok()?,
            parts[3].parse::<u32>().ok()?,
        ))
    };
    version().is_some_and(|v| v >= (2, 1, 223))
}

pub(super) struct Turn {
    pub prompt: String,
    pub end: String,
}

pub(super) fn native_turns(path: &Path) -> Vec<Turn> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut entries = HashMap::new();
    let mut tail = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(entry) = parse_json::<Value>(line.as_bytes()) else {
            continue;
        };
        let Some(uuid) = entry["uuid"]
            .as_str()
            .filter(|_| entry["isSidechain"] != true)
        else {
            continue;
        };
        if matches!(
            entry["type"].as_str(),
            Some("user" | "assistant" | "system")
        ) {
            tail = Some(uuid.to_owned());
        }
        entries.insert(uuid.to_owned(), entry);
    }

    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut cursor = tail.as_deref();
    while let Some(uuid) = cursor.filter(|uuid| visited.insert(*uuid)) {
        let Some(entry) = entries.get(uuid) else {
            break;
        };
        chain.push((uuid, entry));
        cursor = entry["parentUuid"]
            .as_str()
            .or_else(|| entry["logicalParentUuid"].as_str());
    }

    let mut turns: Vec<Turn> = Vec::new();
    for (uuid, entry) in chain.into_iter().rev() {
        if claude_external_text(entry).is_some() {
            turns.push(Turn {
                prompt: uuid.into(),
                end: uuid.into(),
            });
        } else if entry["type"] == "assistant" {
            if let Some(turn) = turns.last_mut() {
                turn.end = uuid.into();
            }
        }
    }
    turns
}

pub(super) fn prompt_index(
    turns: &[Turn],
    metadata: &Value,
    local_id: &str,
) -> Result<usize, String> {
    let prompt = metadata["conversationHistoryEntryIds"][local_id]
        .as_str()
        .ok_or_else(|| format!("No native history point for message {local_id}"))?;
    turns
        .iter()
        .position(|turn| turn.prompt == prompt)
        .ok_or_else(|| format!("No native history point for message prompt {prompt}"))
}

#[test]
fn resume_at_minimum_version() {
    for version in ["2.1.223 (Claude Code)", "2.2.0", "3.0.0"] {
        assert!(supports_resume_at(version));
    }
    for version in ["2.1.222 (Claude Code)", "2.0.900", "unknown"] {
        assert!(!supports_resume_at(version));
    }
}

#[test]
fn deep_tool_input_preserves_native_turn_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let input = format!("{}0{}", r#"{"next":"#.repeat(256), "}".repeat(256));
    let user = r#"{"type":"user","uuid":"prompt","message":{"content":"hello"}}"#;
    let assistant = format!(
        r#"{{"type":"assistant","uuid":"tool","parentUuid":"prompt","message":{{"content":[{{"type":"tool_use","input":{input}}}]}}}}"#
    );
    std::fs::write(&path, format!("{user}\n{assistant}\n")).unwrap();

    let turns = native_turns(&path);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].prompt, "prompt");
    assert_eq!(turns[0].end, "tool");
}
