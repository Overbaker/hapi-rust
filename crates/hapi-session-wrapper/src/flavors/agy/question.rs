//! agy `ask_question` form elicitations as HAPI question cards (TS `agyQuestion.ts`).
use hapi_wrapper_core::{acp::FormElicitationRequest, jsjson::parse as parse_json};
use serde_json::{json, Map, Value};

/// The question card HAPI clients already render for an agent's question.
pub const QUESTION_TOOL_NAME: &str = "AskUserQuestion";

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgyQuestion {
    pub key: String,
    pub other_key: Option<String>,
    pub question: String,
    pub choices: Vec<Choice>,
    pub multi_select: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgyQuestionForm {
    pub tool_call_id: Option<String>,
    pub questions: Vec<AgyQuestion>,
}

fn read_choices(entries: &Value) -> Vec<Choice> {
    entries
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let value = e.get("const")?.as_str()?;
            Some(Choice {
                value: value.into(),
                label: e["title"].as_str().unwrap_or(value).into(),
            })
        })
        .collect()
}
fn question_number(key: &str) -> Option<u128> {
    let digits = key.strip_prefix('q')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().unwrap_or(u128::MAX))
}
fn first_present<'a>(a: &'a Value, b: &'a Value) -> &'a Value {
    if a.is_null() {
        b
    } else {
        a
    }
}

/// Question N is property `qN` (single choice: string with oneOf; multi-select:
/// array with items.anyOf), with an optional free-form `qN_other`.
pub fn parse_agy_question_form(request: &FormElicitationRequest) -> Option<AgyQuestionForm> {
    parse_schema(request.tool_call_id.as_deref(), &request.requested_schema)
}
fn parse_schema(tool_call_id: Option<&str>, schema: &Value) -> Option<AgyQuestionForm> {
    let properties = schema.get("properties")?.as_object()?;
    let mut keys: Vec<(u128, &String)> = properties
        .keys()
        .filter_map(|k| question_number(k).map(|n| (n, k)))
        .collect();
    keys.sort_by_key(|(n, _)| *n);
    let mut questions = Vec::new();
    for (_, key) in keys {
        let property = &properties[key];
        if !property.is_object() {
            continue;
        }
        let multi_select = property["type"] == "array";
        let items = &property["items"];
        let choices = if multi_select {
            read_choices(first_present(&items["anyOf"], &items["oneOf"]))
        } else {
            read_choices(first_present(&property["oneOf"], &property["anyOf"]))
        };
        let other = format!("{key}_other");
        questions.push(AgyQuestion {
            key: key.clone(),
            other_key: properties.contains_key(&other).then_some(other),
            question: property["title"].as_str().unwrap_or("").into(),
            choices,
            multi_select,
        });
    }
    (!questions.is_empty()).then(|| AgyQuestionForm {
        tool_call_id: tool_call_id.map(str::to_owned),
        questions,
    })
}

pub fn question_card_input(questions: &[AgyQuestion]) -> Value {
    json!({"questions":questions.iter().map(|q| json!({
        "question":q.question,
        "header":null,
        "options":q.choices.iter().map(|c| json!({"label":c.label,"description":null})).collect::<Vec<_>>(),
        "multiSelect":q.multi_select,
        "allowFreeText":q.other_key.is_some(),
    })).collect::<Vec<_>>()})
}

/// The `ask_question` tool call's own input, shown as the same question card.
/// agy sends the questions as an array; its transcript stores a JSON string.
pub fn question_card_input_from_tool_call(input: &Value) -> Option<Value> {
    let raw = input.as_object()?.get("questions")?;
    let parsed;
    let raw = match raw {
        Value::String(s) => {
            parsed = parse_json::<Value>(s.as_bytes()).ok()?;
            &parsed
        }
        other => other,
    };
    let questions: Vec<Value> = raw
        .as_array()?
        .iter()
        .filter(|e| e.is_object())
        .map(|e| {
            let options: Vec<Value> = e["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o.as_str().filter(|s| !s.is_empty()))
                .map(|label| json!({"label":label,"description":null}))
                .collect();
            json!({"question":e["question"].as_str().unwrap_or(""),"header":null,"options":options,"multiSelect":e["is_multi_select"]==true})
        })
        .collect();
    (!questions.is_empty()).then(|| json!({"questions":questions}))
}

/// Option labels each card question shows (`questions[].options[].label`).
pub fn shown_labels(card_input: &Value) -> Vec<Vec<String>> {
    card_input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| {
            q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o["label"].as_str().map(str::to_owned))
                .collect()
        })
        .collect()
}

fn answer_values(answer: &Value) -> Vec<String> {
    let values = match answer {
        Value::Array(a) => Some(a),
        Value::Object(o) => o.get("answers").and_then(Value::as_array),
        _ => None,
    };
    values
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The card's answers in agy's shape: the chosen option values, and free text
/// under `qN_other` only when the user typed some. `shown_labels` are matched
/// by position when they differ from the form's option titles.
pub fn agy_question_content(
    questions: &[AgyQuestion],
    answers: Option<&Value>,
    shown_labels: Option<&[Vec<String>]>,
) -> Map<String, Value> {
    let mut content = Map::new();
    let null = Value::Null;
    for (index, question) in questions.iter().enumerate() {
        let values = answer_values(answers.map_or(&null, |a| &a[index.to_string()]));
        let (mut chosen, mut typed) = (Vec::new(), Vec::new());
        let shown = shown_labels
            .and_then(|s| s.get(index))
            .filter(|s| s.len() == question.choices.len());
        for value in values {
            let choice = question
                .choices
                .iter()
                .find(|c| c.label.trim() == value)
                .or_else(|| {
                    let i = shown?.iter().position(|l| l.trim() == value)?;
                    question.choices.get(i)
                });
            match choice {
                Some(c) => chosen.push(c.value.clone()),
                None => typed.push(value),
            }
        }
        if !chosen.is_empty() {
            content.insert(
                question.key.clone(),
                if question.multi_select {
                    json!(chosen)
                } else {
                    json!(chosen[0])
                },
            );
        }
        if let (false, Some(other)) = (typed.is_empty(), &question.other_key) {
            content.insert(other.clone(), json!(typed.join(", ")));
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(schema: Value) -> Option<AgyQuestionForm> {
        parse_agy_question_form(&FormElicitationRequest {
            session_id: "agy-conv-1".into(),
            tool_call_id: Some("call_1".into()),
            request_id: None,
            message: "q".into(),
            requested_schema: schema,
        })
    }
    fn single() -> Value {
        json!({"type":"object","properties":{
            "q1":{"type":"string","title":"Do you prefer RED or BLUE?","oneOf":[{"const":"1","title":"(Recommended) RED"},{"const":"2","title":"BLUE"}]},
            "q1_other":{"type":"string","title":"Other"}}})
    }
    fn two() -> Value {
        json!({"type":"object","properties":{
            "q1":{"type":"array","title":"Which colors?","items":{"anyOf":[{"const":"1","title":"RED"},{"const":"2","title":"GREEN"},{"const":"3","title":"BLUE"}]}},
            "q1_other":{"type":"string","title":"Other"},
            "q2":{"type":"string","title":"Light or dark theme?","oneOf":[{"const":"1","title":"LIGHT"},{"const":"2","title":"DARK"}]},
            "q2_other":{"type":"string","title":"Other"}}})
    }
    fn content(f: &AgyQuestionForm, answers: Value, shown: Option<&[Vec<String>]>) -> Value {
        Value::Object(agy_question_content(&f.questions, Some(&answers), shown))
    }

    #[test]
    fn reads_single_choice() {
        let f = form(single()).unwrap();
        assert_eq!(f.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            f.questions,
            vec![AgyQuestion {
                key: "q1".into(),
                other_key: Some("q1_other".into()),
                question: "Do you prefer RED or BLUE?".into(),
                choices: vec![
                    Choice {
                        value: "1".into(),
                        label: "(Recommended) RED".into()
                    },
                    Choice {
                        value: "2".into(),
                        label: "BLUE".into()
                    }
                ],
                multi_select: false
            }]
        );
        assert_eq!(
            question_card_input(&f.questions),
            json!({"questions":[{"question":"Do you prefer RED or BLUE?","header":null,"options":[{"label":"(Recommended) RED","description":null},{"label":"BLUE","description":null}],"multiSelect":false,"allowFreeText":true}]})
        );
        let mut s = single();
        s["properties"].as_object_mut().unwrap().remove("q1_other");
        assert_eq!(
            question_card_input(&form(s).unwrap().questions)["questions"][0]["allowFreeText"],
            false
        );
    }

    #[test]
    fn reads_several_and_ignores_others() {
        let f = form(two()).unwrap();
        let shape: Vec<_> = f
            .questions
            .iter()
            .map(|q| (q.key.as_str(), q.multi_select, q.choices.len()))
            .collect();
        assert_eq!(shape, vec![("q1", true, 3), ("q2", false, 2)]);
        assert!(form(json!({"type":"object","properties":{"name":{"type":"string"}}})).is_none());
        assert!(form(Value::Null).is_none());
    }

    #[test]
    fn answers() {
        let f = form(single()).unwrap();
        assert_eq!(content(&f, json!({"0":["BLUE"]}), None), json!({"q1":"2"}));
        assert_eq!(
            content(&f, json!({"0":["(Recommended) RED"]}), None),
            json!({"q1":"1"})
        );
        assert_eq!(
            content(&f, json!({"0":["BLUE","  "]}), None),
            json!({"q1":"2"})
        );
        assert_eq!(
            content(&f, json!({"0":["GREEN please"]}), None),
            json!({"q1_other":"GREEN please"})
        );
        assert_eq!(
            content(&f, json!({"0":["BLUE","and a bit of RED"]}), None),
            json!({"q1":"2","q1_other":"and a bit of RED"})
        );
        assert_eq!(
            content(&f, json!({"0":{"answers":["BLUE"]}}), None),
            json!({"q1":"2"})
        );
        assert_eq!(content(&f, json!({"0":"BLUE"}), None), json!({}));
        let f2 = form(two()).unwrap();
        assert_eq!(
            content(&f2, json!({"0":["GREEN","BLUE"],"1":["DARK"]}), None),
            json!({"q1":["2","3"],"q2":"2"})
        );
        assert!(agy_question_content(&f2.questions, None, None).is_empty());
    }

    #[test]
    fn matches_by_position() {
        let f = form(json!({"type":"object","properties":{
            "q1":{"type":"string","title":"Pick","oneOf":[{"const":"1","title":"RED "},{"const":"2","title":"Blue"}]},
            "q1_other":{"type":"string","title":"Other"}}}))
        .unwrap();
        assert_eq!(content(&f, json!({"0":["RED"]}), None), json!({"q1":"1"}));
        let shown = vec![vec!["RED".to_owned(), "BLUE".to_owned()]];
        assert_eq!(
            content(&f, json!({"0":["BLUE"]}), Some(&shown)),
            json!({"q1":"2"})
        );
        let other = vec![vec![
            "GREEN".to_owned(),
            "BLUE".to_owned(),
            "RED".to_owned(),
        ]];
        assert_eq!(
            content(&f, json!({"0":["BLUE"]}), Some(&other)),
            json!({"q1_other":"BLUE"})
        );
    }

    #[test]
    fn tool_call_card() {
        let qs = json!([{"is_multi_select":true,"options":["RED","BLUE"],"question":"Colors?"}]);
        let expected = json!({"questions":[{"question":"Colors?","header":null,"options":[{"label":"RED","description":null},{"label":"BLUE","description":null}],"multiSelect":true}]});
        assert_eq!(
            question_card_input_from_tool_call(&json!({"questions":qs})),
            Some(expected.clone())
        );
        assert_eq!(
            question_card_input_from_tool_call(&json!({"questions":qs.to_string()})),
            Some(expected.clone())
        );
        assert_eq!(
            shown_labels(&expected),
            vec![vec!["RED".to_owned(), "BLUE".to_owned()]]
        );
        assert_eq!(
            question_card_input_from_tool_call(&json!({"questions":"not json"})),
            None
        );
        assert_eq!(question_card_input_from_tool_call(&Value::Null), None);
    }
}
