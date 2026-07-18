use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentContext;
use crate::output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};
use crate::sanitize::{collapse_whitespace, strip_html};

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
        let text = collapse_whitespace(&strip_html(&html));

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
