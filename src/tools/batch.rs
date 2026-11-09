//! One batch of tool calls, and the locks that keep two calls on one file from
//! losing each other.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use futures::future::join_all;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::messages::ToolCall;
use crate::path::{normalize, resolve_path};
use crate::tools::{AgentContext, ToolRegistry, ToolResult};

/// Runs one batch of tool calls. Calls that name the same file take turns —
/// each holds that file's lock for the whole call — so a batch of edits to one
/// file applies one at a time instead of each reading the original and
/// overwriting the rest. Calls on different files, and tools that cannot be
/// reasoned about by path (bash, elisp), still run together.
pub async fn run_batch(
    tool_registry: ToolRegistry,
    tool_calls: &[ToolCall],
    cwd: &Path,
) -> Vec<ToolResult> {
    let mut locks: HashMap<PathBuf, Arc<Mutex<()>>> = HashMap::new();

    let handles: Vec<_> = tool_calls
        .iter()
        .map(|tc| {
            let name = tc.name.clone();
            let arguments = tc.arguments.clone();
            // A call whose arguments never decoded cannot be run, but it can be
            // answered. Reported as a tool failure it reaches the model as a
            // result it can act on, instead of ending the run.
            let parse_error = tc.parse_error.clone();
            let ctx = AgentContext {
                cwd: cwd.to_path_buf(),
            };
            let registry = tool_registry.clone();
            let lock = file_key(&tool_registry, cwd, &tc.name, &tc.arguments)
                .map(|key| locks.entry(key).or_default().clone());
            tokio::spawn(async move {
                let _held = match lock {
                    Some(lock) => Some(lock.lock_owned().await),
                    None => None,
                };
                match parse_error {
                    Some(err) => bail!(
                        "arguments were not valid JSON ({err}). Send the call \
                         again with complete, valid JSON arguments."
                    ),
                    None => registry.execute(&name, arguments, &ctx).await,
                }
            })
        })
        .collect();

    join_all(handles)
        .await
        .into_iter()
        .map(|res| match res {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => ToolResult {
                content: format!("Tool execution failed: {e:#}"),
                is_error: true,
            },
            Err(join_err) => ToolResult {
                content: format!("Tool task failed: {join_err}"),
                is_error: true,
            },
        })
        .collect()
}

/// The lock key for a tool call: the file it names, resolved and normalized,
/// for a tool that operates on one path. `None` for the rest, and for a name no
/// tool in the registry answers to.
///
/// The tool decides — [`Tool::lock_path`] — because the key has to agree with
/// the argument that tool actually reads. A name-keyed table here would be a
/// second place to teach, and the first place to forget.
pub fn file_key(
    registry: &ToolRegistry,
    cwd: &Path,
    name: &str,
    arguments: &Value,
) -> Option<PathBuf> {
    let field = registry.get(name)?.lock_path()?;
    let raw = arguments.get(field)?.as_str()?;
    Some(normalize(&resolve_path(cwd, raw)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use std::fs;

    fn edit_call(id: &str, path: &str, old: &str, new: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "edit".to_string(),
            arguments: serde_json::json!({ "path": path, "old_text": old, "new_text": new }),
            parse_error: None,
        }
    }

    fn project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-batch-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Two edits to one file in a single batch must take turns: the second
    /// reads what the first wrote. Run at once, each would read the original
    /// and one edit would be silently lost. The two spellings only meet after
    /// normalization, so this also holds the lock's key honest.
    #[tokio::test]
    async fn edits_to_one_file_in_a_batch_do_not_lose_each_other() {
        let dir = project_dir("edit-lock");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("note.txt"), "foo").unwrap();

        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();
        let calls = vec![
            edit_call("call-1", "note.txt", "foo", "bar"),
            edit_call("call-2", "sub/../note.txt", "foo", "baz"),
        ];

        let results = run_batch(registry, &calls, &dir).await;

        let errors = results.iter().filter(|result| result.is_error).count();
        assert_eq!(errors, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The lock key is the tool's own answer: the argument it reads, resolved
    /// against the working directory. A tool that names no file takes no lock,
    /// and a name nothing answers to is not a file either.
    #[test]
    fn the_lock_key_is_the_argument_the_tool_names() {
        let dir = project_dir("lock-key");
        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();

        assert_eq!(
            file_key(
                &registry,
                &dir,
                "read",
                &serde_json::json!({"path": "sub/../a.txt"})
            ),
            Some(dir.join("a.txt"))
        );
        assert!(
            file_key(
                &registry,
                &dir,
                "bash",
                &serde_json::json!({"command": "ls"})
            )
            .is_none()
        );
        assert!(
            file_key(
                &registry,
                &dir,
                "nonesuch",
                &serde_json::json!({"path": "a.txt"})
            )
            .is_none()
        );
        assert_eq!(registry.get("edit").and_then(Tool::lock_path), Some("path"));
        let _ = fs::remove_dir_all(&dir);
    }
}
