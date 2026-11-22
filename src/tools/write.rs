use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    path::resolve_path,
    tools::{AgentContext, PATH_ARGUMENT, PATH_DESCRIPTION, path_trace_label, replace_file},
};

use super::{Tool, ToolResult};

pub struct WriteTool;

/// `deny_unknown_fields` is what turns a call aimed at `edit` — `path`,
/// `content`, and an `old_str` to replace — into a refusal instead of a whole
/// file written over the text that model meant to change.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> String {
        "Create or overwrite a whole file with the full content it must hold. To change \
         part of an existing file, use `edit` instead."
            .to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": PATH_DESCRIPTION
                },
                "content": {
                    "type": "string",
                    "description": "Full file content to write"
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    /// The resolved path, spelled the way every other tool in the trace spells
    /// it. A call that arrived without one says so.
    fn trace_label(&self, arguments: &Value, cwd: &Path) -> Option<String> {
        path_trace_label(arguments, cwd)
    }

    fn lock_path(&self) -> Option<&'static str> {
        Some(PATH_ARGUMENT)
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: WriteInput = serde_json::from_value(input)
            .context("invalid write input; expected { path: string, content: string }")?;

        let path = resolve_path(&ctx.cwd, &input.path);

        if let Some(parent) = path.parent()
            && parent != Path::new("")
        {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!(
                    "failed to create parent directories for: {}",
                    path.display()
                )
            })?;
        }

        replace_file(&path, &input.content).await?;

        Ok(ToolResult {
            content: format!("wrote {}", path.display()),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir(name: &str) -> PathBuf {
        crate::config::test_dir(name, None)
    }

    async fn call(cwd: &Path, arguments: Value) -> Result<ToolResult> {
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        WriteTool.execute(arguments, &ctx).await
    }

    #[tokio::test]
    async fn a_whole_file_is_written_and_the_parent_made() {
        let cwd = dir("write-nested");
        let result = call(&cwd, json!({"path": "sub/deep.txt", "content": "a\n"}))
            .await
            .unwrap();

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            std::fs::read_to_string(cwd.join("sub/deep.txt")).unwrap(),
            "a\n"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// The call a model makes when it means `edit`: a replacement it expected to
    /// be applied to the file it named, carried in a field `write` does not name.
    /// Written as a whole file, that call would destroy the text it meant to
    /// change, so it is refused by the field instead.
    #[tokio::test]
    async fn a_field_the_schema_does_not_name_is_refused() {
        let cwd = dir("write-unknown-field");
        std::fs::write(cwd.join("note.txt"), "keep me\n").unwrap();

        let error = call(
            &cwd,
            json!({
                "path": "note.txt",
                "content": "x",
                "edits": [{"old_str": "keep", "new_str": "gone"}]
            }),
        )
        .await
        .expect_err("a call carrying edit's fields must not be written");

        assert!(format!("{error:#}").contains("unknown field"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.txt")).unwrap(),
            "keep me\n"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A blank path names no file. It resolves to the working directory, which
    /// is not one to write into — so nothing is created, and certainly not a
    /// file whose name is the spaces that were sent.
    #[tokio::test]
    async fn a_blank_path_writes_nothing() {
        let cwd = dir("write-blank-path");

        let error = call(&cwd, json!({"path": "  ", "content": "x"}))
            .await
            .expect_err("a blank path names no file to write");

        assert!(
            format!("{error:#}").contains("directory, not a file to write"),
            "{error:#}"
        );
        let entries = std::fs::read_dir(&cwd).unwrap().count();
        assert_eq!(entries, 0, "nothing may be left behind");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A read-only file is one the user protected. Replacing it by rename would
    /// slip past that permission, so it stays a failure and the bytes stay put.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_read_only_file_is_not_replaced() {
        use std::os::unix::fs::PermissionsExt;

        let cwd = dir("write-read-only");
        let path = cwd.join("vendor.txt");
        std::fs::write(&path, "theirs\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let error = call(&cwd, json!({"path": "vendor.txt", "content": "mine\n"}))
            .await
            .expect_err("a read-only file is not ours to replace");

        assert!(format!("{error:#}").contains("read-only"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// The file is replaced rather than truncated, so the mode a reader needs is
    /// still there afterwards — an executable script stays executable.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_replaced_file_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let cwd = dir("write-keeps-mode");
        let path = cwd.join("run.sh");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        call(
            &cwd,
            json!({"path": "run.sh", "content": "#!/bin/sh\necho hi\n"}),
        )
        .await
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
