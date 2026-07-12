use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentContext;
use crate::output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

use super::{Tool, ToolResult};

pub struct WebSearchTool {
    base_url: String,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new() -> Result<Self> {
        let base_url = std::env::var("ONELOOP_SEARX_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .context("failed to build web search HTTP client")?;
        Ok(Self { base_url, client })
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

const WEB_SEARCH_USER_AGENT: &str = "oneloop/0.1";

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> String {
        "Search the web using a SearXNG meta search engine. Returns ranked results with titles, URLs, and snippets.".to_string()
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

        let url = format!("{}/search", self.base_url.trim_end_matches('/'));

        let response = self
            .client
            .get(url)
            .query(&[("q", input.query.as_str()), ("format", "json")])
            .header("User-Agent", WEB_SEARCH_USER_AGENT)
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

        Ok(ToolResult {
            content: truncate_tail(&output, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES),
            is_error: false,
        })
    }
}
