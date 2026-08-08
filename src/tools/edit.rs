use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentContext;

use super::{Tool, ToolResult, resolve_path};

pub struct EditTool;

#[derive(Debug, Deserialize)]
struct EditInput {
    path: String,
    old_text: String,
    new_text: String,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> String {
        "Edit file contents".to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit. Relative to the \
                                    working directory unless absolute. `~` is \
                                    expanded; pass it through as-is."
                },
                "old_text": {
                    "type": "string",
                    "description": "Exact text to replace"
                },
                "new_text": {
                    "type": "string",
                    "description": "Replacement text"
                }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: EditInput = serde_json::from_value(input).context(
            "invalid edit input; expected { path: string, old_text: string, new_text: string }",
        )?;

        let path = resolve_path(&ctx.cwd, &input.path);

        if !tokio::fs::try_exists(&path)
            .await
            .with_context(|| format!("failed to check file existence: {}", path.display()))?
        {
            return Err(anyhow!("file does not exist: {}", path.display()));
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read file: {}", path.display()))?;

        let occurrences = content.matches(&input.old_text).count();
        if occurrences == 0 {
            return Err(anyhow!("old_text not found in file: {}", path.display()));
        }
        if occurrences > 1 {
            return Err(anyhow!(
                "old_text is not unique in file: {} ({occurrences} matches)",
                path.display()
            ));
        }

        let updated = content.replacen(&input.old_text, &input.new_text, 1);
        tokio::fs::write(&path, updated)
            .await
            .with_context(|| format!("failed to write edited file: {}", path.display()))?;

        Ok(ToolResult {
            content: format!("edited {}", path.display()),
            is_error: false,
        })
    }
}
