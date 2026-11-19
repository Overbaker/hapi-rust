use serde_json::{json, Value};
use std::collections::VecDeque;

struct Backfill {
    after: i64,
    live_ids: VecDeque<String>,
}

fn remember(ids: &mut VecDeque<String>, id: &str, capacity: usize) -> bool {
    let index = ids.iter().position(|entry| entry == id);
    if let Some(index) = index {
        let existing = ids.remove(index).unwrap();
        ids.push_back(existing);
    } else {
        ids.push_back(id.to_owned());
        if ids.len() > capacity {
            ids.pop_front();
        }
    }
    index.is_none()
}

pub struct IncomingFilter {
    seen: VecDeque<String>,
    capacity: usize,
    backfill: Option<Backfill>,
    cursor: Option<i64>,
}
impl Default for IncomingFilter {
    fn default() -> Self {
        Self::new(256)
    }
}
impl IncomingFilter {
    pub fn new(capacity: usize) -> Self {
        Self {
            seen: VecDeque::new(),
            capacity,
            backfill: None,
            cursor: None,
        }
    }
    pub fn cursor(&self) -> Option<i64> {
        self.cursor
    }
    pub(super) fn start_backfill(&mut self) -> Option<i64> {
        let cursor = self.cursor?;
        Some(
            self.backfill
                .get_or_insert_with(|| Backfill {
                    after: cursor,
                    live_ids: VecDeque::new(),
                })
                .after,
        )
    }
    pub(super) fn backfill_cursor(&self) -> Option<i64> {
        self.backfill.as_ref().map(|progress| progress.after)
    }
    pub(super) fn page_scanned(&mut self, after: i64) {
        if let Some(progress) = &mut self.backfill {
            progress.after = progress.after.max(after);
        }
    }
    pub(super) fn finish_backfill(&mut self) {
        self.backfill = None;
    }
    pub(super) fn accept_delivery(
        &mut self,
        id: Option<&str>,
        seq: Option<i64>,
        from_page: bool,
    ) -> bool {
        let accepted = self.accept(id, seq);
        let Some(progress) = &mut self.backfill else {
            return accepted;
        };
        let Some(id) = id.filter(|s| !s.is_empty()) else {
            return accepted;
        };
        if from_page {
            if let Some(index) = progress.live_ids.iter().position(|entry| entry == id) {
                progress.live_ids.remove(index);
                return false;
            }
            accepted
        } else if seq.is_some_and(|s| s > progress.after) {
            remember(&mut progress.live_ids, id, self.capacity) && accepted
        } else {
            accepted
        }
    }
    pub fn accept(&mut self, id: Option<&str>, seq: Option<i64>) -> bool {
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            if !remember(&mut self.seen, id, self.capacity) {
                return false;
            }
        } else if seq.zip(self.cursor).is_some_and(|(n, c)| n <= c) {
            return false;
        }
        if let Some(n) = seq {
            self.cursor = Some(n.max(self.cursor.unwrap_or(0)));
        }
        true
    }
}
pub(super) fn user_message(raw: &Value) -> Option<Value> {
    let content = &raw["content"];
    if raw["role"] != "user" || content["type"] != "text" {
        return None;
    }
    let mut out = json!({"role":"user","content":{"type":"text","text":content["text"].as_str()?}});
    if let Some(k) = raw.get("localKey") {
        out["localKey"] = k.as_str()?.into();
    }
    if let Some(items) = content.get("attachments") {
        let mut attachments = Vec::new();
        for item in items.as_array()? {
            let mut a = json!({"size":item["size"].as_number()?});
            for key in ["id", "filename", "mimeType", "path"] {
                a[key] = item[key].as_str()?.into();
            }
            if let Some(s) = item.get("previewUrl") {
                a["previewUrl"] = s.as_str()?.into();
            }
            attachments.push(a);
        }
        out["content"]["attachments"] = attachments.into();
    }
    if let Some(meta) = raw.get("meta") {
        let mut m = json!({});
        for (k, v) in meta.as_object()? {
            let valid = match k.as_str() {
                "sentFrom" => v.is_string(),
                "isNativeQueuedMessage" | "isTranscriptEcho" => v.is_boolean(),
                "deliveryMode" => v == "queue" || v == "steer",
                "fallbackModel" | "customSystemPrompt" | "appendSystemPrompt" => {
                    v.is_null() || v.is_string()
                }
                "allowedTools" | "disallowedTools" => {
                    v.is_null() || v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
                }
                _ => continue,
            };
            if !valid {
                return None;
            }
            m[k] = v.clone();
        }
        out["meta"] = m;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{prelude::*, test_runner::RngSeed};
    #[test]
    fn stalled_backfill_bounds_live_ids_and_refreshes_duplicates() {
        let mut filter = IncomingFilter::new(3);
        assert!(filter.accept(Some("initial"), Some(1)));
        assert_eq!(filter.start_backfill(), Some(1));
        for n in 2..100 {
            assert!(filter.accept_delivery(Some(&n.to_string()), Some(n), false));
            assert!(filter.backfill.as_ref().unwrap().live_ids.len() <= 3);
        }
        assert!(!filter.accept_delivery(Some("97"), Some(97), false));
        assert!(filter.accept_delivery(Some("100"), Some(100), false));
        assert_eq!(
            filter.backfill.as_ref().unwrap().live_ids,
            ["99", "97", "100"]
        );
        for n in 2..5 {
            assert!(filter.accept_delivery(Some(&n.to_string()), Some(n), true));
        }
        assert!(!filter.accept_delivery(Some("97"), Some(97), true));
        assert_eq!(filter.backfill.as_ref().unwrap().live_ids, ["99", "100"]);
    }

    #[test]
    fn mature_and_idless() {
        let mut f = IncomingFilter::default();
        assert!(f.accept(Some("new"), Some(11)));
        assert!(f.accept(Some("mature"), Some(10)));
        assert!(!f.accept(Some("mature"), Some(13)));
        assert_eq!(f.cursor(), Some(11));
        assert!(!f.accept(None, Some(11)));
        assert!(f.accept(None, Some(12)));
        assert!(f.accept(None, None));
    }
    #[test]
    fn refreshes_recency_before_eviction() {
        let mut f = IncomingFilter::new(2);
        for (id, accept) in [
            ("a", true),
            ("b", true),
            ("a", false),
            ("c", true),
            ("a", false),
            ("b", true),
        ] {
            assert_eq!(f.accept(Some(id), Some(1)), accept);
        }
    }
    proptest! {
        #![proptest_config(ProptestConfig{rng_seed:RngSeed::Fixed(2053),cases:128,..Default::default()})]
        #[test]
        fn cursor_and_id_lru(cap in 1usize..9,stream in prop::collection::vec((prop::option::of(0u8..12),prop::option::of(-2i64..40)),1..200)){
            let mut filter=IncomingFilter::new(cap);let mut recency=std::collections::HashMap::new();let mut cursor=None;
            for (step,(id,seq)) in stream.into_iter().enumerate(){
                let expected=if let Some(id)=id{
                    let new=recency.insert(id,step).is_none();
                    if recency.len()>cap{let old=*recency.iter().min_by_key(|(_,n)|**n).unwrap().0;recency.remove(&old);}
                    new
                }else{!seq.zip(cursor).is_some_and(|(n,c)|n<=c)};
                if expected{if let Some(s)=seq{cursor=Some(s.max(cursor.unwrap_or(0)));}}
                prop_assert_eq!(filter.accept(id.map(|i|i.to_string()).as_deref(),seq),expected);
                prop_assert_eq!(filter.cursor(),cursor);
            }
        }
    }
}

#[cfg(test)]
mod schema_tests {
    use super::*;
    #[test]
    fn typed_user_fields_and_unknown_keys() {
        let attachment = json!({"id":"a","filename":"f","mimeType":"t","path":"/f","size":42});
        let mut preview = attachment.clone();
        preview["previewUrl"] = "https://example.invalid".into();
        let expected = json!({"role":"user","localKey":"key","content":{"type":"text","text":"hi","attachments":[attachment,preview]},"meta":{"sentFrom":"web","isNativeQueuedMessage":true,"isTranscriptEcho":false,"deliveryMode":"steer","fallbackModel":"m","customSystemPrompt":"a","appendSystemPrompt":"b","allowedTools":["a"],"disallowedTools":[] }});
        let mut input = expected.clone();
        input["extra"] = 1.into();
        input["content"]["extra"] = true.into();
        input["content"]["attachments"][0]["extra"] = 1.into();
        input["meta"]["extra"] = 1.into();
        assert_eq!(user_message(&input), Some(expected.clone()));
        for key in expected["meta"].as_object().unwrap().keys() {
            let mut bad = expected.clone();
            bad["meta"][key] = 42.into();
            assert!(user_message(&bad).is_none(), "{key}");
            bad["meta"][key] = Value::Null;
            assert_eq!(
                user_message(&bad).is_some(),
                matches!(
                    key.as_str(),
                    "fallbackModel"
                        | "customSystemPrompt"
                        | "appendSystemPrompt"
                        | "allowedTools"
                        | "disallowedTools"
                )
            );
        }
        for val in [json!("later"), Value::Null] {
            let mut bad = expected.clone();
            bad["meta"]["deliveryMode"] = val;
            assert!(user_message(&bad).is_none());
        }
        let mut bad = expected.clone();
        bad["meta"]["allowedTools"] = json!([3]);
        assert!(user_message(&bad).is_none());
        for key in ["id", "filename", "mimeType", "path", "size", "previewUrl"] {
            let mut bad = expected.clone();
            bad["content"]["attachments"][0][key] = Value::Null;
            assert!(user_message(&bad).is_none(), "{key}");
        }
        let mut bad = expected;
        bad["localKey"] = false.into();
        assert!(user_message(&bad).is_none());
    }
}
