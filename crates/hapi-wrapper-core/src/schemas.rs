use jsonschema::{Keyword, ValidationError, Validator};
use serde_json::Value;
use std::{collections::HashSet, sync::OnceLock};
use url::Url;

pub fn metadata(v: &Value) -> bool {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR
        .get_or_init(|| {
            let schema = serde_json::from_str(include_str!("../golden/metadata_schema.json"))
                .expect("generated metadata schema is JSON");
            jsonschema::options()
                .should_validate_formats(true)
                .with_format("uri", |value| Url::parse(trim_url(value)).is_ok())
                .with_keyword("x-hapi-external-refs", |_, _, _| Ok(Box::new(ExternalRefs)))
                .build(&schema)
                .expect("generated metadata schema compiles")
        })
        .is_valid(v)
}

fn trim_url(value: &str) -> &str {
    value.trim_matches(|ch: char| ch == '\u{feff}' || (ch.is_whitespace() && ch != '\u{0085}'))
}

struct ExternalRefs;

impl<'i> Keyword<'i> for ExternalRefs {
    fn validate(&self, value: &'i Value) -> Result<(), ValidationError<'i>> {
        self.is_valid(value)
            .then_some(())
            .ok_or_else(|| ValidationError::custom("invalid GitHub PR references"))
    }

    fn is_valid(&self, value: &'i Value) -> bool {
        let Some(refs) = value.as_array() else {
            return false;
        };
        let mut primary = false;
        let mut identities = HashSet::new();
        for reference in refs {
            let (Some(repo), Some(number), Some(raw_url)) = (
                reference["repo"].as_str(),
                reference["number"].as_f64(),
                reference["url"].as_str(),
            ) else {
                return false;
            };
            let raw_url = trim_url(raw_url).replace(['\t', '\n', '\r'], "");
            let Ok(url) = Url::parse(&raw_url) else {
                return false;
            };
            let Some(host) = url.host_str() else {
                return false;
            };
            let path = url.path();
            let valid_host = host == "github.com"
                || host
                    .strip_suffix(".ghe.com")
                    .is_some_and(|label| !label.is_empty());
            if url.scheme() != "https"
                || !valid_host
                || path.strip_suffix('/').unwrap_or(path) != format!("/{repo}/pull/{number}")
            {
                return false;
            }
            if reference["role"] == "primary" && std::mem::replace(&mut primary, true) {
                return false;
            }
            let identity_host = if raw_url.contains("://") {
                host
            } else {
                &raw_url
            };
            if !identities.insert((
                identity_host.to_lowercase(),
                repo.to_ascii_lowercase(),
                number as u64,
            )) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_special_cases_match_typescript() {
        let pr = |repo: &str, url: &str, role: &str| json!({"kind":"github_pr","repo":repo,"number":20,"url":url,"role":role});
        let primary = pr("x/y", "https://github.com/x/y/pull/20", "primary");
        let enterprise = pr("x/y", "https://org.ghe.com/x/y/pull/20/", "secondary");
        let cases = [
            (
                json!({"claudeRewind":{"legacy":true},"preferredCopilotAgentMode":"fleet"}),
                true,
            ),
            (
                json!({"hapiMcpUrl":"\u{a0}https:example.com\u{feff}"}),
                true,
            ),
            (json!({"hapiMcpUrl":"https://"}), false),
            (
                json!({"codexRuntime":{"kind":"attached","bindingId":"00000000-0000-0000-0000-000000000000","state":"attached"}}),
                true,
            ),
            (json!({"jobKeyRedirects":{"__proto__":false}}), true),
            (json!({"jobKeyRedirects":{"a":false}}), false),
            (json!({"externalRefs":[primary, enterprise]}), true),
            (
                json!({"externalRefs":[primary, pr("x/y", "https:github.com/x/y/pull/20", "secondary")]}),
                true,
            ),
            (
                json!({"externalRefs":[primary, pr("x/y", "\u{a0}https:/\n/github.com/x/y/pull/20\u{feff}", "secondary")]}),
                false,
            ),
            (
                json!({"externalRefs":[primary, pr("X/Y", "https://github.com/X/Y/pull/20", "secondary")]}),
                false,
            ),
            (
                json!({"externalRefs":[primary, pr("x/y", "https://org.ghe.com/x/y/pull/20", "primary")]}),
                false,
            ),
            (
                json!({"externalRefs":[pr("x/y", "https://github.com/x/y/pull/21", "primary")]}),
                false,
            ),
            (
                json!({"externalRefs":[pr("x/y", "https://notgithub.com/x/y/pull/20", "primary")]}),
                false,
            ),
            (
                json!({"externalRefs":[pr("x/y", "http://github.com/x/y/pull/20", "primary")]}),
                false,
            ),
        ];
        let values: Vec<_> = cases
            .iter()
            .map(|(fields, _)| {
                let mut value = fields.clone();
                value["host"] = "h".into();
                value["path"] = "/".into();
                value
            })
            .collect();
        let script = "import { MetadataSchema } from '@hapi/protocol/schemas'; console.log(JSON.stringify(JSON.parse(process.argv[1]).map(v => MetadataSchema.safeParse(v).success)))";
        let input = serde_json::to_string(&values).unwrap();
        let output = std::process::Command::new("bun")
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
            .args(["-e", script, &input])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let typescript: Vec<bool> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(typescript.len(), cases.len());
        for ((value, (_, expected)), typescript) in values.iter().zip(cases).zip(typescript) {
            assert_eq!(typescript, expected, "TypeScript validation of {value}");
            assert_eq!(metadata(value), expected, "Rust validation of {value}");
        }
    }

    #[test]
    fn opencode_rewind_matches_typescript_schema() {
        let valid = json!({"operationId":"op","nativeSessionId":"n","workerId":"w","workerPid":7,"workerStartMarker":"m","requestedLocalId":"r","nativeMessageId":"nm","cutoffSeq":3,"sourceEpoch":0,"sourceMaxSeq":4,"reservationVersion":2,"sourceTranscriptHash":"a".repeat(64),"phase":"committed","sourceHash":"s","prefixHash":"p","extra":1});
        let mut cases = vec![
            valid.clone(),
            json!(null),
            json!("x"),
            json!([]),
            json!({"operationId":"op","phase":"committed"}),
        ];
        for (key, bad) in [
            ("operationId", json!("")),
            ("nativeSessionId", json!(1)),
            ("workerId", Value::Null),
            ("workerPid", json!(0)),
            ("workerPid", json!(1.5)),
            ("workerPid", Value::Null),
            ("workerStartMarker", json!("")),
            ("cutoffSeq", json!(0)),
            ("cutoffSeq", json!(2.0)),
            ("cutoffSeq", json!(9007199254740992u64)),
            ("sourceEpoch", json!(-1)),
            ("sourceMaxSeq", json!("1")),
            ("reservationVersion", json!(0.5)),
            ("sourceTranscriptHash", json!("A".repeat(64))),
            ("sourceTranscriptHash", json!("a".repeat(63))),
            ("phase", json!("done")),
            ("sourceHash", json!(1)),
            ("prefixHash", Value::Null),
        ] {
            let mut case = valid.clone();
            case[key] = bad;
            cases.push(case);
        }
        for key in ["nativeMessageId", "phase", "sourceTranscriptHash"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().shift_remove(key);
            cases.push(missing);
        }
        let mut optional = valid.clone();
        for key in [
            "workerPid",
            "workerStartMarker",
            "sourceHash",
            "prefixHash",
            "extra",
        ] {
            optional.as_object_mut().unwrap().shift_remove(key);
        }
        cases.push(optional);
        let script = format!(
            "import {{ MetadataSchema }} from '@hapi/protocol/schemas'; console.log(JSON.stringify({}.map(c => MetadataSchema.safeParse({{host:'h',path:'/',opencodeRewind:c}}).success)))",
            serde_json::to_string(&cases).unwrap()
        );
        let output = std::process::Command::new("bun")
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
            .args(["-e", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Vec<bool> = serde_json::from_slice(&output.stdout).unwrap();
        let actual: Vec<_> = cases
            .iter()
            .map(|c| metadata(&json!({"host":"h","path":"/","opencodeRewind":c})))
            .collect();
        assert_eq!(actual, expected);
        let lifecycle = vec![
            json!({"lifecycleState":"archived","lifecycleStateSince":1,"archivedBy":"u","archiveReason":"r"}),
            json!({"lifecycleState":1}),
            json!({"lifecycleState":null}),
            json!({"lifecycleStateSince":"1"}),
            json!({"archivedBy":false}),
            json!({"archiveReason":[]}),
        ];
        let script = format!(
            "import {{ MetadataSchema }} from '@hapi/protocol/schemas'; console.log(JSON.stringify({}.map(c => MetadataSchema.safeParse({{host:'h',path:'/',...c}}).success)))",
            serde_json::to_string(&lifecycle).unwrap()
        );
        let output = std::process::Command::new("bun")
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli"))
            .args(["-e", &script])
            .output()
            .unwrap();
        assert!(output.status.success());
        let expected: Vec<bool> = serde_json::from_slice(&output.stdout).unwrap();
        let actual: Vec<_> = lifecycle
            .iter()
            .map(|c| {
                let mut m = c.clone();
                m["host"] = "h".into();
                m["path"] = "/".into();
                metadata(&m)
            })
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(expected, [true, false, false, false, false, false]);
        assert!(metadata(&json!({"host":"h","path":"/"})));
        assert!(!metadata(&json!({"path":"/"})));
    }
}
