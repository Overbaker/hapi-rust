use base64::{
    engine::general_purpose::{GeneralPurpose, NO_PAD},
    Engine,
};
use hapi_wrapper_core::rpc::handlers::GeneratedImages;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, path::Path};
#[derive(Default)]
pub struct ToolResultImages {
    tools: VecDeque<(String, String, Option<String>)>,
    pub cache: GeneratedImages,
}
impl ToolResultImages {
    pub fn new(cache: GeneratedImages) -> Self {
        Self {
            cache,
            ..Default::default()
        }
    }
    pub fn send(&mut self, client: &hapi_wrapper_core::session::client::SessionClient, row: Value) {
        let (row, images) = self.process(row);
        client.send_claude_session_message(&row);
        for image in images {
            client.send_agent_message(image, None);
        }
    }
    pub fn process(&mut self, mut row: Value) -> (Value, Vec<Value>) {
        let mut images = Vec::new();
        if row["isSidechain"] == true {
            return (row, images);
        }
        let uuid = row["uuid"].as_str().unwrap_or("undefined").to_owned();
        if row["type"] == "assistant" {
            for b in row["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
            {
                if let (Some(id), Some(name)) = (b["id"].as_str(), b["name"].as_str()) {
                    let path = b["input"]["file_path"]
                        .as_str()
                        .or_else(|| b["input"]["path"].as_str());
                    let entry = (id.to_owned(), name.to_owned(), path.map(str::to_owned));
                    if let Some(old) = self.tools.iter_mut().find(|t| t.0 == id) {
                        *old = entry;
                    } else {
                        self.tools.push_back(entry);
                    }
                    if self.tools.len() > 500 {
                        self.tools.pop_front();
                    }
                }
            }
        } else if row["type"] == "user" {
            for b in row
                .pointer_mut("/message/content")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_result")
            {
                let Some(id) = b["tool_use_id"].as_str().map(str::to_owned) else {
                    continue;
                };
                let tool = self.tools.iter().find(|t| t.0 == id);
                for (i, part) in b["content"]
                    .as_array_mut()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if part["type"] != "image" || part["source"]["type"] != "base64" {
                        continue;
                    }
                    let Some(data) = part["source"]["data"]
                        .as_str()
                        .filter(|s| s.len() <= (25usize << 20).saturating_mul(4).div_ceil(3) + 4)
                    else {
                        continue;
                    };
                    let mut chars: Vec<_> = data
                        .bytes()
                        .take_while(|c| *c != b'=')
                        .filter_map(|c| match c {
                            b'-' => Some(b'+'),
                            b'_' => Some(b'/'),
                            c if c.is_ascii_alphanumeric() || c == b'+' || c == b'/' => Some(c),
                            _ => None,
                        })
                        .collect();
                    if chars.len() % 4 == 1 {
                        chars.pop();
                    }
                    let Ok(bytes) = GeneralPurpose::new(
                        &base64::alphabet::STANDARD,
                        NO_PAD.with_decode_allow_trailing_bits(true),
                    )
                    .decode(chars) else {
                        continue;
                    };
                    let Some(mime) = mime(&bytes) else {
                        continue;
                    };
                    let image_id = Sha256::digest(format!("{uuid}:{id}:{i}"))
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>();
                    let stem = tool
                        .and_then(|t| t.2.as_deref())
                        .and_then(|p| Path::new(p).file_stem())
                        .and_then(|s| s.to_str())
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("image-{}", &image_id[..8]));
                    let name = format!("{stem}.{}", mime[6..].replace("jpeg", "jpg"));
                    if self
                        .cache
                        .insert(image_id.clone(), bytes, mime.into(), name.clone())
                        .is_err()
                    {
                        continue;
                    }
                    let mut source =
                        json!({"ingress":"tool_result","flavor":"claude","toolCallId":id});
                    if let Some(t) = tool {
                        source["toolName"] = json!(t.1);
                    }
                    images.push(json!({"type":"generated-image","imageId":image_id,"id":image_id,"mimeType":mime,"fileName":name,"source":source}));
                    part["source"].as_object_mut().unwrap().remove("data");
                }
            }
            if !images.is_empty() {
                if let Some(v) = row.get_mut("toolUseResult") {
                    strip(v);
                }
            }
        }
        (row, images)
    }
}
fn strip(v: &mut Value) {
    if let Some(a) = v.as_array_mut() {
        a.iter_mut().for_each(strip);
    } else if v["type"] == "image" {
        if let Some(o) = v.get_mut("source").and_then(Value::as_object_mut) {
            o.remove("data");
        }
        if v["file"]["base64"].is_string() {
            v["file"].as_object_mut().unwrap().remove("base64");
        }
    }
}
fn mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[255, 216, 255]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if b.starts_with(&[0, 0, 0])
        && b.get(4..8) == Some(b"ftyp")
        && matches!(b.get(8..12), Some(b"avif" | b"avis"))
    {
        Some("image/avif")
    } else {
        None
    }
}
#[test]
fn images_and_unmodified_results() {
    let mut store = ToolResultImages::default();
    store.process(json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","id":"t","input":{"file_path":"/tmp/shot.jpg"}}]}}));
    let row = json!({"type":"user","uuid":"u","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":[{"type":"image","source":{"type":"base64","data":"iVBORw0KGgo="}}]}]},"toolUseResult":{"type":"image","file":{"base64":"iVBORw0KGgo="}}});
    let (v, images) = store.process(row.clone());
    assert_eq!(images[0]["fileName"], "shot.png");
    assert_eq!(images[0]["source"]["toolName"], "Read");
    assert!(!v.to_string().contains("iVBOR"));
    assert_eq!(store.process(row.clone()).1, images);
    let mut text = row.clone();
    text["message"]["content"][0]["content"] = json!("some text");
    let mut bogus = row.clone();
    bogus["message"]["content"][0]["content"][0]["source"]["data"] = json!("bm90IGFuIGltYWdl");
    let mut side = row;
    side["isSidechain"] = json!(true);
    for bad in [text, bogus, side] {
        assert_eq!(store.process(bad.clone()), (bad, vec![]));
    }
}
