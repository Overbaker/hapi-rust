//! agy model and effort selection (TS `agy/utils/agyModelSelection.ts`,
//! `shared/src/agyModels.ts`, helpers of `agyAcpRemoteLauncher.ts`).
use serde_json::Value;

pub const AGY_EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// `(model, effort)`; `None` model is agy's default.
pub type AgyModelSelection = (Option<String>, Option<String>);

/// Splits a native effort suffix off known model families (`gemini-x-high`).
pub fn split_agy_model_selection(model: Option<&str>) -> AgyModelSelection {
    let value = model.map(str::trim).unwrap_or("");
    if matches!(value, "" | "auto" | "default") {
        return (None, None);
    }
    let re =
        regex_lite::Regex::new(r"^(gemini-.+|claude-.+|gpt-oss-.+)-(low|medium|high|xhigh|max)$")
            .unwrap();
    match re.captures(value) {
        Some(c) => (Some(c[1].to_owned()), Some(c[2].to_owned())),
        None => (Some(value.to_owned()), None),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgyModelChoice {
    pub model_id: String,
    pub name: String,
    pub supported_efforts: Vec<&'static str>,
}

/// Collapses effort variants of the catalog (`[{modelId, name}]`) into choices.
pub fn get_agy_model_choices(catalog: &[Value]) -> Vec<AgyModelChoice> {
    let entries: Vec<(&str, Option<&str>)> = catalog
        .iter()
        .filter_map(|e| Some((e["modelId"].as_str()?, e["name"].as_str())))
        .collect();
    let efforts_of = |base: &str| -> Vec<&'static str> {
        AGY_EFFORT_LEVELS
            .into_iter()
            .filter(|level| {
                entries.iter().any(|&(id, _)| {
                    split_agy_model_selection(Some(id))
                        == (Some(base.to_owned()), Some((*level).to_owned()))
                })
            })
            .collect()
    };
    let mut choices: Vec<AgyModelChoice> = Vec::new();
    for &(id, name) in &entries {
        let (Some(model), effort) = split_agy_model_selection(Some(id)) else {
            continue;
        };
        let efforts = efforts_of(&model);
        // A single variant (e.g. GPT-OSS Medium) does not establish adjustable effort.
        let adjustable = efforts.len() > 1;
        let model_id = if adjustable {
            model.clone()
        } else {
            id.to_owned()
        };
        if choices.iter().any(|c| c.model_id == model_id) {
            continue;
        }
        let name = match (adjustable, effort.as_deref()) {
            (true, Some(effort)) => name.map_or(model.clone(), |n| {
                regex_lite::Regex::new(&format!(r"(?i)\s*\({effort}\)$"))
                    .unwrap()
                    .replace(n, "")
                    .into_owned()
            }),
            _ => name.unwrap_or(id).to_owned(),
        };
        choices.push(AgyModelChoice {
            model_id,
            name,
            supported_efforts: if adjustable { efforts } else { vec![] },
        });
    }
    choices
}

/// Normalizes launch/resume and RPC config against the catalog before it is
/// queued. `effort`: `None` = not given, `Some(None)` = explicit default.
pub fn normalize_agy_model_selection(
    model: Option<&str>,
    effort: Option<Option<&str>>,
    catalog: &[Value],
) -> Result<AgyModelSelection, String> {
    if let Some(Some(e)) = effort {
        if !AGY_EFFORT_LEVELS.contains(&e) {
            return Err(format!("Invalid Antigravity effort: {e}"));
        }
    }
    let split = split_agy_model_selection(model);
    let choices = get_agy_model_choices(catalog);
    let original = model.map(str::trim).filter(|m| !m.is_empty());
    let choice = choices
        .iter()
        .find(|c| Some(c.model_id.as_str()) == original)
        .or_else(|| {
            choices
                .iter()
                .find(|c| Some(&c.model_id) == split.0.as_ref())
        });
    if let Some(c) = choice.filter(|c| c.supported_efforts.is_empty()) {
        return Ok((Some(c.model_id.clone()), None));
    }
    let selected = match effort {
        None => split.1.clone(),
        Some(e) => e.map(str::to_owned),
    };
    if let Some(c) = choice {
        if let Some(e) = selected
            .as_deref()
            .filter(|e| !c.supported_efforts.iter().any(|s| s == e))
        {
            return Err(format!(
                "Antigravity model {} does not support effort {e}",
                c.model_id
            ));
        }
        return Ok((Some(c.model_id.clone()), selected));
    }
    if split.1.is_some() {
        if effort.is_some_and(|e| e.map(str::to_owned) != split.1) {
            return Err(format!(
                "Cannot change effort for {}: refresh the Antigravity model catalog first",
                original.unwrap_or("")
            ));
        }
        return Ok((original.map(str::to_owned), None));
    }
    Ok((split.0, selected))
}

/// `always-proceed` stays in agy's default mode; HAPI answers its requests.
pub fn agy_acp_mode_for_permission(mode: Option<&str>) -> &'static str {
    match mode {
        Some("acceptEdits") => "accept-edits",
        Some("plan") => "plan",
        _ => "default",
    }
}

/// agy ids carry effort as a suffix (`gemini-3.8-flash-low`); no model means
/// agy's default, whose effort can still be chosen.
pub fn resolve_agy_acp_model_id(
    model: Option<&str>,
    effort: Option<&str>,
    default_model_id: Option<&str>,
) -> Option<String> {
    let effort = effort.filter(|e| !e.is_empty());
    let (selected, selected_effort) = split_agy_model_selection(model);
    if selected.is_none() {
        let default = default_model_id?;
        let Some(effort) = effort else {
            return Some(default.to_owned());
        };
        let base = split_agy_model_selection(Some(default))
            .0
            .unwrap_or_else(|| default.to_owned());
        return Some(format!("{base}-{effort}"));
    }
    let value = model.unwrap_or("").trim();
    Some(match (selected_effort, effort) {
        (None, Some(effort)) => format!("{value}-{effort}"),
        _ => value.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> Vec<Value> {
        json!([
            {"modelId":"gemini-4-argon-medium","name":"Gemini 4 Argon (Medium)"},
            {"modelId":"gemini-4-argon-high","name":"Gemini 4 Argon (High)"},
            {"modelId":"gemini-4-argon-xhigh","name":"Gemini 4 Argon (Xhigh)"},
            {"modelId":"gemini-4-argon-max","name":"Gemini 4 Argon (Max)"},
            {"modelId":"gpt-oss-120b-medium","name":"GPT-OSS 120B (Medium)"},
            {"modelId":"claude-opus-4-6-thinking","name":"Claude Opus 4.6 (Thinking)"}
        ])
        .as_array()
        .unwrap()
        .clone()
    }
    fn sel(m: &str, e: Option<&str>) -> AgyModelSelection {
        (Some(m.into()), e.map(str::to_owned))
    }

    #[test]
    fn normalizes_selection() {
        let c = catalog();
        let n = normalize_agy_model_selection;
        assert_eq!(
            n(Some("gemini-4-argon-medium"), Some(Some("high")), &c),
            Ok(sel("gemini-4-argon", Some("high")))
        );
        assert_eq!(
            n(Some("gemini-4-argon-medium"), None, &c),
            Ok(sel("gemini-4-argon", Some("medium")))
        );
        assert_eq!(
            n(Some("gemini-4-argon-medium"), Some(None), &c),
            Ok(sel("gemini-4-argon", None))
        );
        for e in ["xhigh", "max"] {
            assert_eq!(
                n(Some("gemini-4-argon"), Some(Some(e)), &c),
                Ok(sel("gemini-4-argon", Some(e)))
            );
        }
        assert!(n(Some("gemini-4-argon"), Some(Some("low")), &c)
            .unwrap_err()
            .contains("does not support effort low"));
        assert!(n(None, Some(Some("extreme")), &c)
            .unwrap_err()
            .contains("Invalid Antigravity effort"));
        for m in ["gpt-oss-120b-medium", "claude-opus-4-6-thinking"] {
            assert_eq!(n(Some(m), Some(Some("high")), &c), Ok(sel(m, None)));
        }
        assert_eq!(
            n(Some("custom-high"), Some(Some("medium")), &[]),
            Ok(sel("custom-high", Some("medium")))
        );
        assert_eq!(
            n(Some("gemini-future-medium"), None, &[]),
            Ok(sel("gemini-future-medium", None))
        );
        for e in [Some("high"), None] {
            assert!(n(Some("gemini-future-medium"), Some(e), &[])
                .unwrap_err()
                .contains("refresh the Antigravity model catalog first"));
        }
        assert_eq!(
            n(None, Some(Some("high")), &c),
            Ok((None, Some("high".into())))
        );
    }

    #[test]
    fn choices_collapse_variants() {
        let choices = get_agy_model_choices(&catalog());
        assert_eq!(
            choices[0],
            AgyModelChoice {
                model_id: "gemini-4-argon".into(),
                name: "Gemini 4 Argon".into(),
                supported_efforts: vec!["medium", "high", "xhigh", "max"]
            }
        );
        assert_eq!(choices[1].model_id, "gpt-oss-120b-medium");
        assert!(choices[1].supported_efforts.is_empty());
        assert_eq!(choices.len(), 3);
    }

    #[test]
    fn launcher_helpers() {
        assert_eq!(
            agy_acp_mode_for_permission(Some("request-review")),
            "default"
        );
        assert_eq!(
            agy_acp_mode_for_permission(Some("always-proceed")),
            "default"
        );
        assert_eq!(
            agy_acp_mode_for_permission(Some("acceptEdits")),
            "accept-edits"
        );
        assert_eq!(agy_acp_mode_for_permission(Some("plan")), "plan");
        assert_eq!(agy_acp_mode_for_permission(None), "default");
        let r = resolve_agy_acp_model_id;
        assert_eq!(
            r(Some("gemini-3.8-flash"), Some("low"), None).as_deref(),
            Some("gemini-3.8-flash-low")
        );
        assert_eq!(
            r(Some("gpt-oss-120b-medium"), None, None).as_deref(),
            Some("gpt-oss-120b-medium")
        );
        assert_eq!(
            r(Some("gemini-3.8-flash-high"), Some("low"), None).as_deref(),
            Some("gemini-3.8-flash-high")
        );
        assert_eq!(
            r(None, None, Some("gemini-4-argon-medium")).as_deref(),
            Some("gemini-4-argon-medium")
        );
        assert_eq!(
            r(None, Some("high"), Some("gemini-4-argon-medium")).as_deref(),
            Some("gemini-4-argon-high")
        );
        assert_eq!(r(None, None, None), None);
        assert_eq!(split_agy_model_selection(Some(" auto ")), (None, None));
    }
}
