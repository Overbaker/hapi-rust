use super::value::text;
use serde_json::Value;
pub fn proposal_id(thread: &str, turn: &str, item: &str) -> String {
    format!("codex-proposed-plan:{thread}:{turn}:{item}")
}
pub fn proposal_for_item(thread: &str, turn: &str, item: &Value) -> Option<String> {
    if item["type"] != "plan" || text(item, "text")?.trim().is_empty() {
        return None;
    }
    Some(proposal_id(thread, turn, text(item, "id")?))
}
pub fn proposal_for_turn(thread: &str, turn: &Value) -> Option<String> {
    let id = text(turn, "id")?;
    turn["items"]
        .as_array()?
        .iter()
        .rev()
        .find_map(|item| proposal_for_item(thread, id, item))
}
pub fn implementation_id(plan: &str) -> String {
    format!("{plan}:implement")
}
