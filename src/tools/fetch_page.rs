use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentContext;
use crate::output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

use super::{Tool, ToolResult};

const ERROR_BODY_MAX_BYTES: usize = 2 * 1024;
const ERROR_BODY_MAX_LINES: usize = 20;

pub struct FetchPageTool {
    client: reqwest::Client,
}

impl FetchPageTool {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to build fetch page HTTP client")?;
        Ok(Self { client })
    }
}

#[derive(Debug, Deserialize)]
struct FetchPageInput {
    url: String,
}

#[async_trait]
impl Tool for FetchPageTool {
    fn name(&self) -> &'static str {
        "fetch_page"
    }

    fn description(&self) -> String {
        "Fetch a web page and extract its clean text content. Strips HTML, scripts, and styles. Returns the readable text of the page.".to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to fetch"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentContext) -> Result<ToolResult> {
        let input: FetchPageInput = serde_json::from_value(input).context(
            "invalid fetch_page input; expected { url: string }",
        )?;

        let response = self
            .client
            .get(&input.url)
            .header("User-Agent", "Mozilla/5.0 (compatible; oneloop/0.1)")
            .send()
            .await
            .with_context(|| format!("fetch failed for: {}", input.url))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let text = collapse_whitespace(&strip_html(&body));
            let truncated = truncate_tail(&text, ERROR_BODY_MAX_BYTES, ERROR_BODY_MAX_LINES);
            return Ok(ToolResult {
                content: format!("fetch failed ({status}): {truncated}"),
                is_error: true,
            });
        }

        let html = response
            .text()
            .await
            .context("failed to read response body")?;
        let text = strip_html(&html);
        let text = collapse_whitespace(&text);

        let mut output = String::new();
        output.push_str(&format!("url: {}\n", input.url));
        output.push_str(&format!("length: {} bytes\n\n", text.len()));
        output.push_str(&text);

        Ok(ToolResult {
            content: truncate_tail(&output, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES),
            is_error: false,
        })
    }
}

// ── HTML stripping ────────────────────────────────────────────────────

/// Strip HTML tags and decode common entities.
fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Skip <script...>...</script> and <style...>...</style>
        if bytes[i] == b'<'
            && let Some(closing_tag) = try_skip_tag_block(bytes, i, len)
        {
            i = closing_tag;
            continue;
        }

        // Skip any HTML tag
        if bytes[i] == b'<' {
            i += 1;
            // Consume until >, handling quotes inside attributes
            while i < len {
                if bytes[i] == b'>' {
                    i += 1;
                    break;
                }
                // Skip quoted strings so we don't stop on > inside quotes
                if bytes[i] == b'"' || bytes[i] == b'\'' {
                    let quote = bytes[i];
                    i += 1;
                    while i < len && bytes[i] != quote {
                        i += 1;
                    }
                    if i < len {
                        i += 1; // skip closing quote
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }

        // Decode & entities inline
        if bytes[i] == b'&'
            && let Some((decoded, consumed)) = try_decode_entity(bytes, i, len)
        {
            result.push(decoded);
            i = consumed;
            continue;
        }

        // Copy the full UTF-8 sequence; casting a single byte to char would
        // mangle multi-byte characters.
        let Some(ch) = html[i..].chars().next() else {
            break;
        };
        result.push(ch);
        i += ch.len_utf8();
    }

    result
}

/// Try to skip a <script> or <style> block. Returns the index after the closing tag if found.
fn try_skip_tag_block(bytes: &[u8], start: usize, len: usize) -> Option<usize> {
    // Check opening tag prefix (case-insensitive)
    let is_script = start + 7 <= len
        && (bytes[start + 1].eq_ignore_ascii_case(&b's'))
        && (bytes[start + 2].eq_ignore_ascii_case(&b'c'))
        && (bytes[start + 3].eq_ignore_ascii_case(&b'r'))
        && (bytes[start + 4].eq_ignore_ascii_case(&b'i'))
        && (bytes[start + 5].eq_ignore_ascii_case(&b'p'))
        && (bytes[start + 6].eq_ignore_ascii_case(&b't'));

    let is_style = start + 6 <= len
        && (bytes[start + 1].eq_ignore_ascii_case(&b's'))
        && (bytes[start + 2].eq_ignore_ascii_case(&b't'))
        && (bytes[start + 3].eq_ignore_ascii_case(&b'y'))
        && (bytes[start + 4].eq_ignore_ascii_case(&b'l'))
        && (bytes[start + 5].eq_ignore_ascii_case(&b'e'));

    let closing_tag: &[u8] = if is_script {
        b"</script>"
    } else if is_style {
        b"</style>"
    } else {
        return None;
    };

    // First, skip past the opening tag (the >)
    let mut i = start;
    while i < len && bytes[i] != b'>' {
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            while i < len && bytes[i] != quote {
                i += 1;
            }
        }
        i += 1;
    }
    if i < len {
        i += 1; // skip >
    }

    // Now find the closing tag
    while i < len {
        if bytes[i] == b'<' {
            let remaining = len - i;
            if remaining >= closing_tag.len() {
                let mut matches = true;
                for j in 0..closing_tag.len() {
                    if bytes[i + j].eq_ignore_ascii_case(&closing_tag[j]) {
                        continue;
                    }
                    matches = false;
                    break;
                }
                if matches {
                    return Some(i + closing_tag.len());
                }
            }
        }
        i += 1;
    }

    None
}

/// Try to decode an HTML entity at position i. Returns (char, end_index) if found.
fn try_decode_entity(bytes: &[u8], start: usize, len: usize) -> Option<(char, usize)> {
    // Find the closing ;
    let mut end = start + 1;
    while end < len && bytes[end] != b';' {
        end += 1;
    }
    if end >= len {
        return None;
    }
    let entity = std::str::from_utf8(&bytes[start..=end]).ok()?;
    let decoded = decode_named_entity(entity)
        .or_else(|| decode_numeric_entity(entity))?;
    Some((decoded, end + 1))
}

fn decode_named_entity(entity: &str) -> Option<char> {
    match entity {
        "&amp;" => Some('&'),
        "&lt;" => Some('<'),
        "&gt;" => Some('>'),
        "&quot;" => Some('"'),
        "&apos;" => Some('\''),
        "&nbsp;" => Some(' '),
        "&ndash;" => Some('–'),
        "&mdash;" => Some('—'),
        "&hellip;" => Some('…'),
        "&lsquo;" => Some('\''),
        "&rsquo;" => Some('\''),
        "&ldquo;" => Some('"'),
        "&rdquo;" => Some('"'),
        "&bull;" => Some('•'),
        "&copy;" => Some('©'),
        "&reg;" => Some('®'),
        "&trade;" => Some('™'),
        "&euro;" => Some('€'),
        "&pound;" => Some('£'),
        "&yen;" => Some('¥'),
        "&laquo;" => Some('«'),
        "&raquo;" => Some('»'),
        "&middot;" => Some('·'),
        "&deg;" => Some('°'),
        "&plusmn;" => Some('±'),
        "&sup2;" => Some('²'),
        "&sup3;" => Some('³'),
        "&frac12;" => Some('½'),
        "&frac14;" => Some('¼'),
        "&frac34;" => Some('¾'),
        _ => None,
    }
}

fn decode_numeric_entity(entity: &str) -> Option<char> {
    let inner = entity.strip_prefix("&#")?.strip_suffix(';')?;
    let codepoint = if let Some(hex) = inner.strip_prefix('x').or_else(|| inner.strip_prefix('X')) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        inner.parse::<u32>().ok()?
    };
    char::from_u32(codepoint)
}

/// Collapse runs of whitespace and normalize blank lines.
fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());

    let mut prev_blank = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_blank {
                result.push('\n');
                prev_blank = true;
            }
        } else {
            // Collapse internal whitespace in the line
            let mut in_space = false;
            for ch in trimmed.chars() {
                if ch.is_whitespace() {
                    if !in_space {
                        result.push(' ');
                        in_space = true;
                    }
                } else {
                    result.push(ch);
                    in_space = false;
                }
            }
            result.push('\n');
            prev_blank = false;
        }
    }

    // Trim trailing whitespace/newlines
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_decodes_entities() {
        let html = "<p>Tom &amp; Jerry &#x2014; &lt;classic&gt;</p>";
        assert_eq!(strip_html(html), "Tom & Jerry — <classic>");
    }

    #[test]
    fn preserves_multibyte_utf8() {
        let html = "<p>caf\u{e9} — na\u{ef}ve 日本語</p>";
        assert_eq!(strip_html(html), "caf\u{e9} — na\u{ef}ve 日本語");
    }

    #[test]
    fn skips_script_and_style_blocks() {
        let html = "<div>before<script>var x = '<b>hi</b>';</script><style>.a { color: red; }</style>after</div>";
        assert_eq!(strip_html(html), "beforeafter");
    }

    #[test]
    fn collapses_whitespace_runs() {
        let text = "a   b\n\n\n\nc\t\td\n";
        assert_eq!(collapse_whitespace(text), "a b\n\nc d");
    }
}