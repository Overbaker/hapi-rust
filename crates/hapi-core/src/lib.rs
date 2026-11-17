use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Escapes HTML text safely so untrusted content cannot break out of elements.
fn escape_html_text(out: &mut String, input: &str) {
    for byte_idx_char in input.chars() {
        match byte_idx_char {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            ch => out.push(ch),
        }
    }
}

fn is_safe_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.starts_with('#')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
    {
        return true;
    }
    if let Some((scheme, _)) = trimmed.split_once(':') {
        let lower = scheme.to_ascii_lowercase();
        matches!(lower.as_str(), "http" | "https" | "mailto")
    } else {
        true
    }
}

/// Renders Markdown into sanitized HTML without raw script/HTML injection.
pub fn render_markdown_html(markdown: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(markdown, options);
    let mut out = String::with_capacity(markdown.len().saturating_mul(2).max(64));
    let mut in_table_head = false;
    let mut list_stack: Vec<bool> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.push_str("<p>"),
                Tag::Heading { level, .. } => {
                    let lvl = (level as usize).clamp(1, 6);
                    let _ = write!(out, "<h{lvl}>");
                }
                Tag::BlockQuote(_) => out.push_str("<blockquote>"),
                Tag::CodeBlock(kind) => {
                    out.push_str("<pre><code");
                    if let CodeBlockKind::Fenced(lang) = kind {
                        let clean: String = lang
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
                            .collect();
                        if !clean.is_empty() {
                            out.push_str(" class=\"language-");
                            out.push_str(&clean);
                            out.push('"');
                        }
                    }
                    out.push('>');
                }
                Tag::List(first) => {
                    if let Some(start) = first {
                        list_stack.push(true);
                        if start == 1 {
                            out.push_str("<ol>");
                        } else {
                            let _ = write!(out, "<ol start=\"{start}\">");
                        }
                    } else {
                        list_stack.push(false);
                        out.push_str("<ul>");
                    }
                }
                Tag::Item => out.push_str("<li>"),
                Tag::Emphasis => out.push_str("<em>"),
                Tag::Strong => out.push_str("<strong>"),
                Tag::Strikethrough => out.push_str("<del>"),
                Tag::Link { dest_url, title, .. } => {
                    out.push_str("<a href=\"");
                    if is_safe_url(&dest_url) {
                        escape_html_text(&mut out, &dest_url);
                    } else {
                        out.push('#');
                    }
                    out.push('"');
                    if !title.is_empty() {
                        out.push_str(" title=\"");
                        escape_html_text(&mut out, &title);
                        out.push('"');
                    }
                    out.push_str(" rel=\"noopener noreferrer\">");
                }
                Tag::Image { dest_url, title, .. } => {
                    out.push_str("<img src=\"");
                    if is_safe_url(&dest_url) {
                        escape_html_text(&mut out, &dest_url);
                    } else {
                        out.push('#');
                    }
                    out.push('"');
                    if !title.is_empty() {
                        out.push_str(" title=\"");
                        escape_html_text(&mut out, &title);
                        out.push('"');
                    }
                    out.push_str(" alt=\"");
                }
                Tag::Table(_) => out.push_str("<table>"),
                Tag::TableHead => {
                    in_table_head = true;
                    out.push_str("<thead><tr>");
                }
                Tag::TableRow => out.push_str("<tr>"),
                Tag::TableCell => {
                    if in_table_head {
                        out.push_str("<th>");
                    } else {
                        out.push_str("<td>");
                    }
                }
                Tag::HtmlBlock => out.push_str("<div class=\"md-raw-escaped\">"),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push_str("</p>\n"),
                TagEnd::Heading(level) => {
                    let lvl = (level as usize).clamp(1, 6);
                    let _ = write!(out, "</h{lvl}>\n");
                }
                TagEnd::BlockQuote(_) => out.push_str("</blockquote>\n"),
                TagEnd::CodeBlock => out.push_str("</code></pre>\n"),
                TagEnd::List(ordered) => {
                    list_stack.pop();
                    out.push_str(if ordered { "</ol>\n" } else { "</ul>\n" });
                }
                TagEnd::Item => out.push_str("</li>\n"),
                TagEnd::Emphasis => out.push_str("</em>"),
                TagEnd::Strong => out.push_str("</strong>"),
                TagEnd::Strikethrough => out.push_str("</del>"),
                TagEnd::Link => out.push_str("</a>"),
                TagEnd::Image => out.push_str("\" />"),
                TagEnd::Table => out.push_str("</tbody></table>\n"),
                TagEnd::TableHead => {
                    in_table_head = false;
                    out.push_str("</tr></thead><tbody>");
                }
                TagEnd::TableRow => out.push_str("</tr>\n"),
                TagEnd::TableCell => {
                    if in_table_head {
                        out.push_str("</th>");
                    } else {
                        out.push_str("</td>");
                    }
                }
                TagEnd::HtmlBlock => out.push_str("</div>\n"),
                _ => {}
            },
            Event::Text(text) => escape_html_text(&mut out, &text),
            Event::Code(code) => {
                out.push_str("<code>");
                escape_html_text(&mut out, &code);
                out.push_str("</code>");
            }
            Event::Html(raw) | Event::InlineHtml(raw) => escape_html_text(&mut out, &raw),
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push_str("<br />\n"),
            Event::Rule => out.push_str("<hr />\n"),
            Event::TaskListMarker(checked) => {
                if checked {
                    out.push_str("<input type=\"checkbox\" checked disabled /> ");
                } else {
                    out.push_str("<input type=\"checkbox\" disabled /> ");
                }
            }
            Event::FootnoteReference(name) => {
                out.push_str("<sup>[");
                escape_html_text(&mut out, &name);
                out.push_str("]</sup>");
            }
            _ => {}
        }
    }

    out
}

/// Finds the latest safe byte boundary (`\n\n` outside fenced code blocks)
/// so completed top-level blocks stay cached across streaming Markdown chunks.
pub fn find_stable_markdown_boundary(markdown: &str) -> usize {
    let bytes = markdown.as_bytes();
    let mut fence_marker: Option<(u8, usize)> = None;
    let mut last_safe_boundary = 0usize;
    let mut line_start = 0usize;

    while line_start < bytes.len() {
        let mut line_end = line_start;
        while line_end < bytes.len() && bytes[line_end] != b'\n' {
            line_end += 1;
        }
        let line = &markdown[line_start..line_end];
        let trimmed = line.trim_start();
        if let Some(first_byte) = trimmed.as_bytes().first().copied() {
            if first_byte == b'`' || first_byte == b'~' {
                let run_len = trimmed.bytes().take_while(|&b| b == first_byte).count();
                if run_len >= 3 {
                    match fence_marker {
                        None => fence_marker = Some((first_byte, run_len)),
                        Some((m, len)) if m == first_byte && run_len >= len && trimmed[run_len..].trim().is_empty() => {
                            fence_marker = None;
                        }
                        _ => {}
                    }
                }
            }
        }
        let next_pos = if line_end < bytes.len() { line_end + 1 } else { line_end };
        if fence_marker.is_none() && trimmed.is_empty() {
            // Blank line closes preceding paragraph/list cleanly unless immediately followed by link ref/list continuation.
            if next_pos < bytes.len() && !trimmed_next_line_is_reference(markdown[next_pos..].trim_start()) {
                last_safe_boundary = next_pos;
            }
        }
        line_start = next_pos;
    }
    last_safe_boundary
}

fn trimmed_next_line_is_reference(rest: &str) -> bool {
    rest.starts_with('[') && rest.contains("]:")
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IncrementalMarkdownResult {
    pub committed_prefix_bytes: usize,
    pub html: String,
    pub cache_reused: bool,
}

#[derive(Debug, Default)]
pub struct StreamingMarkdownEngine {
    last_markdown: String,
    committed_bytes: usize,
    committed_html: String,
}

impl StreamingMarkdownEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Incrementally renders cumulative streaming Markdown by caching HTML for
    /// finalized leading blocks and re-parsing only the active trailing block.
    pub fn render_cumulative(&mut self, full_markdown: &str) -> IncrementalMarkdownResult {
        let can_reuse = self.committed_bytes > 0
            && full_markdown.len() >= self.committed_bytes
            && self.last_markdown.len() >= self.committed_bytes
            && full_markdown.as_bytes()[..self.committed_bytes]
                == self.last_markdown.as_bytes()[..self.committed_bytes];

        if !can_reuse {
            self.committed_bytes = 0;
            self.committed_html.clear();
        }

        let suffix = &full_markdown[self.committed_bytes..];
        let rel_boundary = find_stable_markdown_boundary(suffix);
        if rel_boundary > 0 {
            let new_committed_slice = &suffix[..rel_boundary];
            self.committed_html.push_str(&render_markdown_html(new_committed_slice));
            self.committed_bytes += rel_boundary;
        }

        let tail = &full_markdown[self.committed_bytes..];
        let tail_html = if tail.is_empty() {
            String::new()
        } else {
            render_markdown_html(tail)
        };

        self.last_markdown.clear();
        self.last_markdown.push_str(full_markdown);

        let mut combined = String::with_capacity(self.committed_html.len() + tail_html.len());
        combined.push_str(&self.committed_html);
        combined.push_str(&tail_html);

        IncrementalMarkdownResult {
            committed_prefix_bytes: self.committed_bytes,
            html: combined,
            cache_reused: can_reuse,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedDiffResult {
    pub unified: String,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub hunk_count: usize,
}

/// Computes a unified diff with line change counts using the Myers algorithm.
pub fn compute_unified_diff(old: &str, new: &str, context_lines: usize) -> UnifiedDiffResult {
    let diff = TextDiff::from_lines(old, new);
    let mut added = 0usize;
    let mut removed = 0usize;
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                ChangeTag::Insert => added += 1,
                ChangeTag::Delete => removed += 1,
                ChangeTag::Equal => {}
            }
        }
    }
    let unified = diff
        .unified_diff()
        .context_radius(context_lines)
        .header("a/file", "b/file")
        .to_string();
    let hunk_count = unified
        .lines()
        .filter(|line| line.starts_with("@@ "))
        .count();

    UnifiedDiffResult {
        unified,
        added_lines: added,
        removed_lines: removed,
        hunk_count,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowMessage {
    pub id: String,
    #[serde(default)]
    pub seq: Option<u64>,
    #[serde(default)]
    pub local_id: Option<String>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub invoked_at: Option<u64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub content: serde_json::Value,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn extract_reasoning_stream_id(content: &serde_json::Value) -> Option<String> {
    let data = content.get("content")?.get("data")?;
    if data.get("type")?.as_str()? == "reasoning" {
        let id = data.get("id")?.as_str()?.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

fn extract_tool_call_id(content: &serde_json::Value) -> Option<String> {
    let data = content.get("content")?.get("data")?;
    if data.get("type")?.as_str()? == "tool-call" {
        let id = data.get("callId")?.as_str()?.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

/// Folds incoming streaming/tail messages into an existing message window
/// in linear time using O(1) hash indices on `id`, `localId`, `reasoning` stream,
/// and `tool-call` `callId`.
pub fn fold_message_window(
    existing: &[WindowMessage],
    incoming: &[WindowMessage],
    max_window: usize,
) -> Vec<WindowMessage> {
    let mut merged: Vec<WindowMessage> = Vec::with_capacity(existing.len() + incoming.len());
    let mut by_id: HashMap<String, usize> = HashMap::with_capacity(existing.len() + incoming.len());
    let mut by_local_id: HashMap<String, usize> = HashMap::with_capacity(existing.len() / 4 + 4);
    let mut by_reasoning: HashMap<String, usize> = HashMap::new();
    let mut by_tool_call: HashMap<String, usize> = HashMap::new();
    let mut tombstones: HashSet<usize> = HashSet::new();

    for msg in existing {
        let idx = merged.len();
        by_id.insert(msg.id.clone(), idx);
        if let Some(lid) = &msg.local_id {
            by_local_id.insert(lid.clone(), idx);
        }
        if let Some(stream_id) = extract_reasoning_stream_id(&msg.content) {
            by_reasoning.insert(stream_id, idx);
        }
        if let Some(call_id) = extract_tool_call_id(&msg.content) {
            by_tool_call.insert(call_id, idx);
        }
        merged.push(msg.clone());
    }

    for msg in incoming {
        if let Some(&idx) = by_id.get(&msg.id) {
            merged[idx] = msg.clone();
            continue;
        }
        if let Some(lid) = &msg.local_id {
            if let Some(&idx) = by_local_id.get(lid) {
                tombstones.insert(idx);
            }
        }
        if let Some(stream_id) = extract_reasoning_stream_id(&msg.content) {
            if let Some(&old_idx) = by_reasoning.get(&stream_id) {
                tombstones.insert(old_idx);
            }
        }
        if let Some(call_id) = extract_tool_call_id(&msg.content) {
            if let Some(&idx) = by_tool_call.get(&call_id) {
                merged[idx] = msg.clone();
                by_id.insert(msg.id.clone(), idx);
                continue;
            }
        }
        let idx = merged.len();
        by_id.insert(msg.id.clone(), idx);
        if let Some(lid) = &msg.local_id {
            by_local_id.insert(lid.clone(), idx);
        }
        if let Some(stream_id) = extract_reasoning_stream_id(&msg.content) {
            by_reasoning.insert(stream_id, idx);
        }
        if let Some(call_id) = extract_tool_call_id(&msg.content) {
            by_tool_call.insert(call_id, idx);
        }
        merged.push(msg.clone());
    }

    let mut compacted: Vec<WindowMessage> = merged
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| !tombstones.contains(idx))
        .map(|(_, msg)| msg)
        .collect();

    if max_window > 0 && compacted.len() > max_window {
        let drop_count = compacted.len() - max_window;
        compacted.drain(..drop_count);
    }
    compacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_markdown_and_escapes_script_tags() {
        let md = "# Title\n\nHello **world** `x < 1`.\n\n<script>alert(1)</script>\n\n[ok](https://example.com) [bad](javascript:alert(1))\n";
        let html = render_markdown_html(md);
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<strong>world</strong>"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains("href=\"#\""));
    }

    #[test]
    fn incremental_markdown_reuses_committed_prefix_and_keeps_code_fences_intact() {
        let mut engine = StreamingMarkdownEngine::new();
        let part1 = "# Section 1\n\nParagraph one.\n\n```rust\nfn main() {\n\n";
        let r1 = engine.render_cumulative(part1);
        assert!(r1.committed_prefix_bytes > 0);
        // Should NOT split inside open ```rust fence after \n\n inside fn main().
        assert!(!part1[..r1.committed_prefix_bytes].contains("```rust"));

        let part2 = format!("{part1}    println!(\"hi\");\n}}\n```\n\nTrailing");
        let r2 = engine.render_cumulative(&part2);
        assert!(r2.cache_reused);
        assert_eq!(r2.html, render_markdown_html(&part2));
    }

    #[test]
    fn unified_diff_reports_counts_and_hunks() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nb-modified\nc\nd\ne\n";
        let res = compute_unified_diff(old, new, 2);
        assert_eq!(res.added_lines, 2);
        assert_eq!(res.removed_lines, 1);
        assert!(res.hunk_count >= 1);
        assert!(res.unified.contains("+b-modified"));
        assert!(res.unified.contains("-b"));
    }

    #[test]
    fn fold_window_replaces_reasoning_and_merges_tool_calls_in_place() {
        let existing = vec![
            WindowMessage {
                id: "m1".into(),
                seq: Some(1),
                local_id: Some("loc-1".into()),
                created_at: 10,
                invoked_at: None,
                status: Some("sending".into()),
                content: json!({ "role": "user", "content": { "type": "text", "text": "go" } }),
                extra: Default::default(),
            },
            WindowMessage {
                id: "r1".into(),
                seq: Some(2),
                local_id: None,
                created_at: 20,
                invoked_at: Some(20),
                status: None,
                content: json!({ "role": "agent", "content": { "type": "codex", "data": { "type": "reasoning", "id": "s1", "message": "step 1" } } }),
                extra: Default::default(),
            },
            WindowMessage {
                id: "t1".into(),
                seq: Some(3),
                local_id: None,
                created_at: 30,
                invoked_at: Some(30),
                status: None,
                content: json!({ "role": "agent", "content": { "type": "codex", "data": { "type": "tool-call", "callId": "c1", "status": "pending" } } }),
                extra: Default::default(),
            },
        ];

        let incoming = vec![
            WindowMessage {
                id: "m1-ack".into(),
                seq: Some(4),
                local_id: Some("loc-1".into()),
                created_at: 10,
                invoked_at: Some(15),
                status: Some("sent".into()),
                content: json!({ "role": "user", "content": { "type": "text", "text": "go" } }),
                extra: Default::default(),
            },
            WindowMessage {
                id: "r2".into(),
                seq: Some(5),
                local_id: None,
                created_at: 25,
                invoked_at: Some(25),
                status: None,
                content: json!({ "role": "agent", "content": { "type": "codex", "data": { "type": "reasoning", "id": "s1", "message": "step 1 + step 2" } } }),
                extra: Default::default(),
            },
            WindowMessage {
                id: "t1-update".into(),
                seq: Some(3),
                local_id: None,
                created_at: 30,
                invoked_at: Some(30),
                status: None,
                content: json!({ "role": "agent", "content": { "type": "codex", "data": { "type": "tool-call", "callId": "c1", "status": "completed" } } }),
                extra: Default::default(),
            },
        ];

        let folded = fold_message_window(&existing, &incoming, 50);
        assert_eq!(folded.len(), 3);
        assert!(folded.iter().any(|m| m.id == "m1-ack"));
        assert!(folded.iter().any(|m| m.id == "r2"));
        assert!(!folded.iter().any(|m| m.id == "r1"));
        let tc = folded.iter().find(|m| extract_tool_call_id(&m.content).as_deref() == Some("c1")).unwrap();
        assert_eq!(tc.content["content"]["data"]["status"], "completed");
    }
}
