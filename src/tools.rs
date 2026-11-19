pub mod bash;
pub mod edit;
pub mod elisp;
pub mod read;
pub mod skill;
pub mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::messages::ToolDefinition;
use crate::output::first_line;
use crate::path::display_path;

/// What a tool is given besides its own arguments: where the agent works.
///
/// One field, and it stays one: a tool that needs more captures it when it is
/// constructed, rather than widening the world every other tool is handed.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

/// The argument every path-taking tool names its file in. The four tools that
/// read, write, and edit one file answer both of the trait's path questions
/// from it — the label resolves it and the lock names it — so it is written
/// once here rather than in four `lock_path` bodies. The schemas still spell
/// `"path"` in their own JSON, because a `json!` key cannot be a constant.
pub const PATH_ARGUMENT: &str = "path";

/// How the `path` property is described to the model, by every tool that names
/// one file. One sentence in one place: four tools asking for the same argument
/// must not teach four versions of the rule.
pub const PATH_DESCRIPTION: &str = "Path to the file. Relative to the working \
                                    directory unless absolute. `~` is expanded; \
                                    pass it through as-is.";

/// How many temporary files this process has named. With the pid it makes the
/// name a call picks its own, so the file that call creates is one no other
/// call — or another process — is writing.
static TEMP_FILES: AtomicU64 = AtomicU64::new(0);

/// Write `contents` to `path` by way of a temporary file beside it, so a call
/// that dies mid-write leaves the old bytes instead of half of the new ones.
///
/// The file is replaced, not truncated: its mode is carried over to the
/// replacement, and a symlink is followed so that editing through a link edits
/// the file the link names. A path that is a directory, or a file whose mode
/// forbids writing it, fails here rather than being written into — renaming
/// would otherwise slip past the permission the file carried.
///
/// A process killed between the write and the rename leaves its temporary file
/// behind, and the old bytes intact: that is the trade this makes.
pub async fn replace_file(path: &Path, contents: &str) -> Result<()> {
    // Following a symlink here is what keeps the link itself in place: renaming
    // over the link would replace it with the file it pointed at.
    let target = tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf());
    let existing = tokio::fs::metadata(&target).await.ok();
    if existing.as_ref().is_some_and(std::fs::Metadata::is_dir) {
        bail!(
            "that path is a directory, not a file to write: {}",
            target.display()
        );
    }
    if existing
        .as_ref()
        .is_some_and(|metadata| metadata.permissions().readonly())
    {
        bail!("that file is read-only: {}", target.display());
    }

    let name = target
        .file_name()
        .map_or_else(|| "file".to_string(), |name| name.to_string_lossy().into());
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TEMP_FILES.fetch_add(1, Ordering::Relaxed)
    ));

    let replaced = write_and_rename(&temp, &target, contents, existing.as_ref()).await;
    if replaced.is_err() {
        // A failed write or rename leaves nothing behind: the old file is
        // intact, and a temporary file beside it would be one more surprise.
        let _ = tokio::fs::remove_file(&temp).await;
    }
    replaced.with_context(|| format!("failed to write file: {}", target.display()))
}

/// The write itself, kept apart so every failure leaves the temporary file for
/// the caller to remove.
async fn write_and_rename(
    temp: &Path,
    target: &Path,
    contents: &str,
    existing: Option<&std::fs::Metadata>,
) -> Result<()> {
    // Created new: whatever already sits at this path is not this call's to
    // truncate, and clobbering it is the one outcome this dance exists to avoid.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .await?;
    file.write_all(contents.as_bytes()).await?;
    // Closed before the rename, so the path is not swapped under an open handle.
    drop(file);
    if let Some(existing) = existing {
        tokio::fs::set_permissions(temp, existing.permissions()).await?;
    }
    tokio::fs::rename(temp, target).await?;
    Ok(())
}

/// The trace label and lock path shared by every tool that takes one file.
///
/// A tool whose argument is a path answers both questions the same way — the
/// label resolves it against the working directory, and the lock names the
/// argument it came from — so the rule lives here instead of being copied into
/// each tool. [`Tool::trace_label`] and [`Tool::lock_path`] are the only
/// per-tool knobs, and a tool that wants neither keeps the defaults.
pub fn path_trace_label(arguments: &Value, cwd: &Path) -> Option<String> {
    Some(
        arguments
            .get(PATH_ARGUMENT)
            .and_then(Value::as_str)
            .map_or_else(
                || "?".to_string(),
                |path| first_line(&display_path(cwd, path)),
            ),
    )
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    fn schema(&self) -> Value;

    /// The body of this call's trace line, after `name: `. Rendered by the tool
    /// because only it knows how its argument should read — a command is
    /// elided, a path is resolved, a skill name is looked up on disk. `None`
    /// for a tool that names no single argument, which traces bare.
    ///
    /// A tool *with* a rule still gets here with the argument missing, which is
    /// why the builtins answer `Some("?")` rather than `None`: a known tool that
    /// forgot its argument should say so rather than look like one that has
    /// nothing to say.
    fn trace_label(&self, _arguments: &Value, _cwd: &Path) -> Option<String> {
        None
    }

    /// The argument that names one file, for the batch lock. `None` for a tool
    /// that cannot be reasoned about by path — bash and elisp can touch
    /// anything, so no single path stands for them.
    fn lock_path(&self) -> Option<&'static str> {
        None
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult>;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// A set given outright. Without this an agent can only ever have the
    /// builtin seven, and a domain tool set is the whole point.
    pub fn from_tools(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }

    pub fn with_builtin_tools(cwd: &Path) -> Result<Self> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(read::ReadTool),
            Arc::new(write::WriteTool),
            Arc::new(edit::EditTool),
            Arc::new(edit::MultiEditTool),
            Arc::new(bash::BashTool),
            Arc::new(elisp::ElispTool),
        ];
        if let Some(skill_tool) = skill::SkillTool::new(cwd) {
            tools.push(Arc::new(skill_tool));
        }
        Ok(Self::from_tools(tools))
    }

    /// The tool answering to this name, for the two questions asked of it
    /// before it runs: how its call should trace, and which file it locks.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(Arc::as_ref)
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description(),
                schema: tool.schema(),
            })
            .collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        input: Value,
        ctx: &AgentContext,
    ) -> Result<ToolResult> {
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == name) else {
            // Models sometimes invent tool names from project instructions
            // (e.g. a CLI mentioned in AGENTS.md) — steer them back.
            bail!(
                "unknown tool: {name}. available tools: {}. \
                 CLI programs are run with the bash tool, not as tool calls.",
                self.names().join(", ")
            );
        };
        tool.execute(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The temporary file is created, never reused: whatever already sits at that
    /// name is not this call's to truncate, and the failure must leave it alone.
    #[tokio::test]
    async fn a_file_already_at_the_temporary_name_is_not_truncated() {
        let dir = crate::config::test_dir("replace-file-taken", None);
        let temp = dir.join(".note.txt.1.tmp");
        std::fs::write(&temp, "theirs").unwrap();

        let error = write_and_rename(&temp, &dir.join("note.txt"), "mine", None)
            .await
            .expect_err("a taken name must not be written into");

        assert!(format!("{error}").contains("exists"), "{error}");
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), "theirs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_tool_error_lists_available_tools() {
        let registry = ToolRegistry::with_builtin_tools(Path::new(".")).unwrap();
        let ctx = AgentContext {
            cwd: PathBuf::from("."),
        };

        let err = registry
            .execute("semble_search", serde_json::json!({}), &ctx)
            .await
            .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("unknown tool: semble_search"));
        assert!(msg.contains("read"));
        assert!(msg.contains("bash tool"));
    }
}
