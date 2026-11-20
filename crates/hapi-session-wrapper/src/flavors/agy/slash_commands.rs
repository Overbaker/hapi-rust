//! The agy `listSlashCommands` reply: the built-in fallback merged with the
//! commands agy negotiated over ACP (TS `handleListSlashCommands`). agy has no
//! user or project command directories, so `listSlashCommands('agy')` is the
//! built-in list alone.
use hapi_wrapper_core::acp::AvailableCommand;
use serde_json::{json, Value};
use std::path::Path;

const BUILTIN: [(&str, &str); 10] = [
    ("goal", "Run until the specified goal is completely finished."),
    ("compact", "Compact the conversation now (summarize history to free up the context window)."),
    ("schedule", "Run an instruction on a recurring schedule or as a one-time timer."),
    ("browser", "Invoke a browser agent for web tasks."),
    ("plan", "Plan carefully before executing a task."),
    ("grill-me", "Interview me to align on a plan."),
    ("teamwork", "Invoke a team of agents to autonomously tackle large projects."),
    ("learn", "Reflect on recent successes or corrections to capture reusable skills or rules."),
    ("boost", "Invoke the Boost multi-agent orchestrator for complex tasks."),
    ("swarm", "(Preview & Quota Heavy) Run a swarm: several agents working in parallel on one task, each in its own conversation, collaborating through a shared message board."),
];

/// `getBuiltinSlashCommands('agy')`.
pub fn builtin() -> Vec<Value> {
    BUILTIN
        .iter()
        .map(
            |(name, description)| json!({"name":name,"description":description,"source":"builtin"}),
        )
        .collect()
}

/// `mergeSlashCommands`: a later command replaces an earlier one with the same
/// case-insensitive name and takes its own position.
pub fn merge(commands: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for command in commands {
        let key = command["name"].as_str().unwrap_or("").to_lowercase();
        out.retain(|c| c["name"].as_str().unwrap_or("").to_lowercase() != key);
        out.push(command);
    }
    out
}

pub async fn list(_cwd: &Path, negotiated: &[AvailableCommand]) -> Value {
    let mut commands = builtin();
    commands.extend(negotiated.iter().map(|c| {
        let mut v = json!({"name":c.name});
        if let Some(d) = &c.description {
            v["description"] = json!(d);
        }
        v["source"] = json!("builtin");
        v
    }));
    json!({"success":true,"commands":merge(commands)})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn merges_negotiated_commands() {
        let negotiated = [
            AvailableCommand {
                name: "Goal".into(),
                description: Some("native goal".into()),
            },
            AvailableCommand {
                name: "review".into(),
                description: None,
            },
        ];
        let reply = list(Path::new("."), &negotiated).await;
        let commands = reply["commands"].as_array().unwrap();
        assert_eq!(reply["success"], true);
        assert_eq!(commands.len(), 11);
        assert_eq!(commands[0]["name"], "compact");
        assert_eq!(
            commands[9],
            json!({"name":"Goal","description":"native goal","source":"builtin"})
        );
        assert_eq!(commands[10], json!({"name":"review","source":"builtin"}));
        assert_eq!(
            list(Path::new("."), &[]).await["commands"],
            json!(builtin())
        );
    }
}
