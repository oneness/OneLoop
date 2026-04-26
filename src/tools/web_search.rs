use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentContext;
use crate::output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

use super::{Tool, ToolResult};

pub struct WebSearchTool {
    base_url: String,
}

impl WebSearchTool {
    pub fn new() -> Self {
        let base_url = std::env::var("ONELOOP_SEARX_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());
        Self { base_url }
    }
}

#[derive(Debug, Deserialize)]
struct SearchInput {
    query: String,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

fn default_max_results() -> usize {
    8
}

#[derive(Debug, Deserialize)]
struct SearxResponse {
    #[serde(default)]
    results: Vec<SearxResult>,
    #[serde(default)]
    answers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SearxResult {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    engines: Vec<String>,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web using a SearXNG meta search engine. Returns ranked results with titles, URLs, and snippets."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default: 8)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentContext) -> Result<ToolResult> {
        let input: SearchInput = serde_json::from_value(input).context(
            "invalid web_search input; expected { query: string, max_results?: number }",
        )?;

        let url = format!(
            "{}/search?q={}&format=json",
            self.base_url.trim_end_matches('/'),
            urlencoding::encode(&input.query)
        );

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .context("failed to build HTTP client for web search")?;

        let response = client
            .get(&url)
            .header("User-Agent", "oneloop/0.1")
            .send()
            .await
            .with_context(|| format!("web search request failed for: {}", input.query))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(ToolResult {
                content: format!("web search failed ({status}): {body}"),
                is_error: true,
            });
        }

        let searx: SearxResponse = response
            .json()
            .await
            .context("failed to parse SearXNG response JSON")?;

        let mut output = String::new();
        output.push_str(&format!("query: {}\n", input.query));
        output.push_str(&format!(
            "results: {}\n\n",
            searx.results.len().min(input.max_results)
        ));

        if !searx.answers.is_empty() {
            output.push_str("instant answers:\n");
            for answer in &searx.answers {
                output.push_str(&format!("  - {answer}\n"));
            }
            output.push('\n');
        }

        for (i, result) in searx.results.iter().take(input.max_results).enumerate() {
            output.push_str(&format!("{}. {}\n", i + 1, result.title));
            output.push_str(&format!("   url: {}\n", result.url));
            if !result.content.is_empty() {
                output.push_str(&format!("   {}\n", result.content.trim()));
            }
            if !result.engines.is_empty() {
                output.push_str(&format!("   engines: {}\n", result.engines.join(", ")));
            }
            output.push('\n');
        }

        let truncated = truncate_tail(&output, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES);
        let mut final_content = truncated.content;
        if truncated.truncated {
            if !final_content.ends_with('\n') && !final_content.is_empty() {
                final_content.push('\n');
            }
            final_content.push_str(&format!(
                "[output truncated: showing {} of {} lines, {} of {} bytes]",
                truncated.shown_lines,
                truncated.original_lines,
                truncated.shown_bytes,
                truncated.original_bytes
            ));
        }

        Ok(ToolResult {
            content: final_content,
            is_error: false,
        })
    }
}
