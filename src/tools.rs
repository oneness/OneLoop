pub mod bash;
pub mod edit;
pub mod elisp;
pub mod read;
pub mod skill;
pub mod write;

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use crate::agent::AgentContext;

/// Turn a model-supplied path into a real one. Paths arrive in four shapes —
/// `@`-prefixed (an editor mention), `~`-anchored, absolute, and relative —
/// and only the last of those is the working directory's to resolve.
///
/// Joining the cwd onto a `~` path is the mistake this centralises away:
/// `PathBuf::join` treats the tilde as an ordinary component, so a write of
/// `~/notes.md` would silently create a directory literally named `~` under
/// the cwd rather than touching the home directory.
pub fn resolve_path(cwd: &Path, raw: &str) -> PathBuf {
    resolve_with_home(cwd, raw, env::var("HOME").ok().as_deref())
}

/// The resolution itself, with `HOME` passed in so it can be tested without
/// mutating the process environment out from under every other test.
fn resolve_with_home(cwd: &Path, raw: &str, home: Option<&str>) -> PathBuf {
    let trimmed = raw.trim_start_matches('@');

    if let Some(home) = home {
        if trimmed == "~" {
            return PathBuf::from(home);
        }
        // Only `~/` is ours to expand: `~alice` names another user's home,
        // which we cannot resolve, so let it fail as itself rather than
        // mangling it into `{home}alice`.
        if let Some(rest) = trimmed.strip_prefix("~/") {
            return PathBuf::from(format!("{home}/{rest}"));
        }
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    fn schema(&self) -> Value;
    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult>;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_builtin_tools(cwd: &Path) -> Result<Self> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(read::ReadTool),
            Arc::new(write::WriteTool),
            Arc::new(edit::EditTool),
            Arc::new(bash::BashTool),
            Arc::new(elisp::ElispTool),
        ];
        if let Some(skill_tool) = skill::SkillTool::new(cwd) {
            tools.push(Arc::new(skill_tool));
        }
        Ok(Self { tools })
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

    /// A working directory that is deliberately *not* under the home
    /// directory: the two must never be conflated.
    const CWD: &str = "/home/kasim/.dotfiles";
    const HOME: Option<&str> = Some("/home/kasim");

    fn resolved(raw: &str) -> PathBuf {
        resolve_with_home(Path::new(CWD), raw, HOME)
    }

    /// The regression: a `~` path joined onto the cwd yielded
    /// `{cwd}/~/repos/...`, which `write` would have created as a directory
    /// literally named `~`.
    #[test]
    fn a_tilde_path_is_never_joined_onto_the_working_directory() {
        assert_eq!(
            resolved("~/repos/OneLoop/README.md"),
            PathBuf::from("/home/kasim/repos/OneLoop/README.md")
        );
        assert_eq!(resolved("~"), PathBuf::from("/home/kasim"));
    }

    #[test]
    fn other_path_shapes_keep_their_meaning() {
        assert_eq!(resolved("/etc/hosts"), PathBuf::from("/etc/hosts"));
        assert_eq!(
            resolved("src/main.rs"),
            PathBuf::from("/home/kasim/.dotfiles/src/main.rs")
        );
        assert_eq!(
            resolved("@src/main.rs"),
            PathBuf::from("/home/kasim/.dotfiles/src/main.rs")
        );
        // Another user's home is not ours to resolve.
        assert_eq!(
            resolved("~alice/notes.md"),
            PathBuf::from("/home/kasim/.dotfiles/~alice/notes.md")
        );
    }

    /// Without `HOME` there is nothing to expand to, and inventing one would
    /// be the very guess this module exists to prevent.
    #[test]
    fn an_unknown_home_leaves_the_tilde_alone() {
        assert_eq!(
            resolve_with_home(Path::new(CWD), "~/notes.md", None),
            PathBuf::from("/home/kasim/.dotfiles/~/notes.md")
        );
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
