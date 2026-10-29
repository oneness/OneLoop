use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    agent::AgentContext,
    output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_window},
    path::resolve_path,
};

use super::{Tool, ToolResult};

pub struct ReadTool;

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
    /// 1-based first line. Defaults to the start of the file.
    offset: Option<usize>,
    /// Lines to show from `offset`, never more than the cap.
    limit: Option<usize>,
    // `force` is deliberately absent: whether this read is a duplicate is a
    // question only the agent's transcript can answer, so the agent reads the
    // flag out of the raw arguments before dispatching the call. Serde ignores
    // unknown fields, which is what lets the tool accept it and do nothing.
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> String {
        "Read file contents".to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read. Relative to the \
                                    working directory unless absolute. `~` is \
                                    expanded; pass it through as-is."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional. First line to show, 1-based. Use \
                                    this to continue past a truncated read: the \
                                    truncation notice names the offset to resume \
                                    from."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": format!(
                        "Optional. How many lines to show from `offset`. \
                         Defaults to, and is never more than, {DEFAULT_MAX_LINES}."
                    )
                },
                // Read by the agent, not by this tool: only the agent holds the
                // transcript that can say whether the read is a duplicate.
                "force": {
                    "type": "boolean",
                    "description": "Optional. Re-send a result this conversation \
                                    already has. Only matters when the same read \
                                    is already above, which the agent otherwise \
                                    answers with a pointer to it."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: ReadInput = serde_json::from_value(input).context(
            "invalid read input; expected { path: string, offset?: number, limit?: number }",
        )?;

        let path = resolve_path(&ctx.cwd, &input.path);

        if !tokio::fs::try_exists(&path)
            .await
            .with_context(|| format!("failed to check file existence: {}", path.display()))?
        {
            return Ok(ToolResult {
                content: format!(
                    "file not found: {}\nUse `bash` with `find` or `grep` to search for it, e.g.: find . -name \"*{}*\" -type f",
                    path.display(),
                    path.file_name()
                        .map(|f| f.to_string_lossy())
                        .unwrap_or_default()
                ),
                is_error: true,
            });
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read file: {}", path.display()))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if total == 0 {
            return Ok(ToolResult {
                content: String::new(),
                is_error: false,
            });
        }

        // A zero offset names no line, so it means "I did not mean to send
        // this" rather than "start at the beginning" — treat it as absent
        // instead of erroring on a field the model did not need to set.
        let offset = input.offset.unwrap_or(1).max(1);
        if offset > total {
            return Ok(ToolResult {
                content: format!(
                    "{} has {total} lines; offset {offset} is past the end. \
                     Re-read with offset <= {total}, or without one to start at line 1.",
                    path.display()
                ),
                is_error: true,
            });
        }

        // The cap holds even when the model asks for more: a call that carries
        // 5000 lines is one huge message the conversation pays for on every
        // remaining turn, whereas two calls over two turns carry the tail for
        // one turn less. That deferral is what `offset` is for. The schema asks
        // for at least one line, and a zero that arrives anyway is read as
        // "unspecified" for the same reason a zero offset is.
        let limit = match input.limit {
            None | Some(0) => DEFAULT_MAX_LINES,
            Some(limit) => limit.min(DEFAULT_MAX_LINES),
        };

        // `end` is 0-based-exclusive as an index and 1-based-inclusive as a
        // line number, which is how `truncate_window` wants it.
        let end = (offset - 1 + limit).min(total);

        Ok(ToolResult {
            content: truncate_window(&lines, offset, end, DEFAULT_MAX_BYTES),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn numbered_file(name: &str, lines: usize) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-read-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let body: String = (1..=lines).map(|n| format!("line {n}\n")).collect();
        fs::write(dir.join("numbered.txt"), body).unwrap();
        dir
    }

    async fn read(cwd: &Path, arguments: Value) -> ToolResult {
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        ReadTool.execute(arguments, &ctx).await.unwrap()
    }

    /// The window is the thing `offset` buys: the model asks for lines 2-3 of a
    /// file and gets those, in the file's own numbering, plus the offset that
    /// continues from where the window stopped.
    #[tokio::test]
    async fn a_window_is_numbered_in_the_files_own_lines() {
        let cwd = numbered_file("window", 5);
        let result = read(
            &cwd,
            json!({"path": "numbered.txt", "offset": 2, "limit": 2}),
        )
        .await;

        assert!(
            result.content.starts_with("line 2\nline 3\n"),
            "{}",
            result.content
        );
        assert!(
            result
                .content
                .contains("[showing lines 2-3 of 5 · continue with offset=4]"),
            "{}",
            result.content
        );
        assert!(!result.is_error);
        let _ = fs::remove_dir_all(&cwd);
    }

    /// Resuming from the hint must reach the rest of the file, and the last
    /// window must stop offering a further one.
    #[tokio::test]
    async fn the_resume_hint_reaches_the_end_and_then_stops() {
        let cwd = numbered_file("resume", 5);
        let tail = read(&cwd, json!({"path": "numbered.txt", "offset": 4})).await;
        assert!(
            tail.content.starts_with("line 4\nline 5"),
            "{}",
            tail.content
        );
        assert_eq!(tail.content, "line 4\nline 5");
        let _ = fs::remove_dir_all(&cwd);
    }

    /// Asking for the whole file is capped, and the cap says how to get the
    /// rest rather than leaving the model to guess — the failure that cost
    /// 36% of a session's input tokens.
    #[tokio::test]
    async fn the_line_cap_holds_and_names_the_offset_that_continues_it() {
        let cwd = numbered_file("cap", DEFAULT_MAX_LINES + 22);
        let result = read(&cwd, json!({"path": "numbered.txt", "limit": 5000})).await;

        let lines = result.content.lines().count();
        assert_eq!(lines, DEFAULT_MAX_LINES + 1, "body plus one notice line");
        assert!(
            result.content.contains(&format!(
                "[showing lines 1-{DEFAULT_MAX_LINES} of {} · continue with offset={}]",
                DEFAULT_MAX_LINES + 22,
                DEFAULT_MAX_LINES + 1
            )),
            "{}",
            result.content
        );
        let _ = fs::remove_dir_all(&cwd);
    }

    /// An offset past the end is a mistake worth naming precisely: the file's
    /// length is the number the model needs to correct it.
    #[tokio::test]
    async fn an_offset_past_the_end_reports_how_long_the_file_is() {
        let cwd = numbered_file("past-end", 5);
        let result = read(&cwd, json!({"path": "numbered.txt", "offset": 99})).await;

        assert!(result.is_error);
        assert!(result.content.contains("has 5 lines"), "{}", result.content);
        assert!(result.content.contains("offset <= 5"), "{}", result.content);
        let _ = fs::remove_dir_all(&cwd);
    }

    /// A file short enough to fit in one window is still readable the old way,
    /// with no notice and no arguments beyond `path`.
    #[tokio::test]
    async fn a_short_file_reads_whole_with_no_notice() {
        let cwd = numbered_file("short", 3);
        let result = read(&cwd, json!({"path": "numbered.txt"})).await;
        assert_eq!(result.content, "line 1\nline 2\nline 3");
        let _ = fs::remove_dir_all(&cwd);
    }
}
