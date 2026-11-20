use super::value::text;
use serde_json::{json, Value};
pub(super) fn normalize_model(v: &Value) -> Option<Value> {
    let s = |k: &str| v[k].as_str().map(str::trim).filter(|s| !s.is_empty());
    let id = s("id").or_else(|| s("model"))?;
    let mut out = json!({"id":id,"displayName":s("displayName").unwrap_or(id),"isDefault":v["isDefault"]==true,"defaultReasoningEffort":s("defaultReasoningEffort"),"defaultServiceTier":s("defaultServiceTier")});
    let efforts = v["supportedReasoningEfforts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            text(e, "reasoningEffort")
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .collect::<Vec<_>>();
    if !efforts.is_empty() {
        out["supportedReasoningEfforts"] = json!(efforts);
    }
    let mut tiers = vec![];
    for entry in v["serviceTiers"].as_array().into_iter().flatten() {
        for k in ["id", "name"] {
            if let Some(s) = text(entry, k).map(str::trim).filter(|s| !s.is_empty()) {
                let s = s.to_lowercase();
                if !tiers.contains(&s) {
                    tiers.push(s);
                }
            }
        }
    }
    if !tiers.is_empty() {
        out["serviceTiers"] = json!(tiers);
    }
    Some(out)
}
