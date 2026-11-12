//! One batch of tool calls, and the locks that keep two calls on one file from
//! losing each other.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures::future::join_all;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::messages::ToolCall;
use crate::path::{normalize, resolve_path};
use crate::tools::{AgentContext, ToolRegistry, ToolResult};

/// One tool call: its result, and the span it covered.
///
/// The span is taken here and nowhere else. What the trace prints beside a call
/// and what the ledger records about the batch are both read from these
/// measurements, so a second clock beside them would only be a second number to
/// keep in agreement. It runs from the moment the call's task starts — which
/// includes waiting for another call's lock on the same file, because that wait
/// is part of how long the call took.
pub struct Called {
    pub result: ToolResult,
    started: Instant,
    pub elapsed: Duration,
}

impl Called {
    /// When the call stopped, reconstructed from its own measurement.
    fn ended(&self) -> Instant {
        self.started + self.elapsed
    }
}

/// How long the batch as a whole took: from the first call's start to the last
/// call's end. Calls run at once, so adding their spans together would count
/// the same seconds twice; the span between the ends is what the batch cost.
pub fn span(calls: &[Called]) -> Duration {
    match (
        calls.iter().map(|call| call.started).min(),
        calls.iter().map(Called::ended).max(),
    ) {
        (Some(first), Some(last)) => last.saturating_duration_since(first),
        _ => Duration::ZERO,
    }
}

/// Runs one batch of tool calls. Calls that name the same file take turns —
/// each holds that file's lock for the whole call — so a batch of edits to one
/// file applies one at a time instead of each reading the original and
/// overwriting the rest. Calls on different files, and tools that cannot be
/// reasoned about by path (bash, elisp), still run together.
pub async fn run(tool_registry: ToolRegistry, tool_calls: &[ToolCall], cwd: &Path) -> Vec<Called> {
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
                let started = Instant::now();
                let _held = match lock {
                    Some(lock) => Some(lock.lock_owned().await),
                    None => None,
                };
                // An error rather than an early return: the span has to come
                // back with the outcome, so every path out of this task is the
                // same shape.
                let outcome = match parse_error {
                    Some(err) => Err(anyhow!(
                        "arguments were not valid JSON ({err}). Send the call \
                         again with complete, valid JSON arguments."
                    )),
                    None => registry.execute(&name, arguments, &ctx).await,
                };
                (outcome, started, started.elapsed())
            })
        })
        .collect();

    join_all(handles)
        .await
        .into_iter()
        .map(|res| match res {
            Ok((Ok(result), started, elapsed)) => Called {
                result,
                started,
                elapsed,
            },
            Ok((Err(e), started, elapsed)) => Called {
                result: ToolResult {
                    content: format!("Tool execution failed: {e:#}"),
                    is_error: true,
                },
                started,
                elapsed,
            },
            // The task itself died, so nothing ran to completion and there is
            // no span to keep: it is filed at the point the failure was seen,
            // rather than given a time it did not take.
            Err(join_err) => Called {
                result: ToolResult {
                    content: format!("Tool task failed: {join_err}"),
                    is_error: true,
                },
                started: Instant::now(),
                elapsed: Duration::ZERO,
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

    fn called(started: Instant, elapsed: Duration) -> Called {
        Called {
            result: ToolResult {
                content: String::new(),
                is_error: false,
            },
            started,
            elapsed,
        }
    }

    /// Calls run at once, so the batch's span is the distance between the
    /// earliest start and the latest end — not the sum, which would count the
    /// same seconds once per overlapping call.
    #[test]
    fn the_batch_span_runs_from_the_first_start_to_the_last_end() {
        let base = Instant::now();
        let calls = vec![
            called(base, Duration::from_millis(500)),
            called(
                base + Duration::from_millis(300),
                Duration::from_millis(800),
            ),
        ];

        assert_eq!(span(&calls), Duration::from_millis(1_100));
    }

    /// A batch that ran nothing has no span, rather than a panic or a
    /// borrowed number from somewhere else.
    #[test]
    fn an_empty_batch_has_no_span() {
        assert_eq!(span(&[]), Duration::ZERO);
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

        let results = run(registry, &calls, &dir).await;

        let errors = results.iter().filter(|call| call.result.is_error).count();
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
