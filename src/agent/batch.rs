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
/// overwriting the rest. A call naming several files holds all of their locks,
/// and takes them in one canonical order, so two such calls cannot each hold
/// what the other waits for. Calls on different files, and tools that cannot be
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
            let locks = file_keys(&tool_registry, cwd, &tc.name, &tc.arguments)
                .into_iter()
                .map(|key| locks.entry(key).or_default().clone())
                .collect::<Vec<_>>();
            tokio::spawn(async move {
                let started = Instant::now();
                // Taken in the order `file_keys` sorted them, which every call
                // sorts the same way, so two calls sharing a file wait rather
                // than deadlock.
                let mut held = Vec::with_capacity(locks.len());
                for lock in locks {
                    held.push(lock.lock_owned().await);
                }
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

/// The lock keys for a tool call: the files the tool itself names, already
/// resolved and normalized, sorted and without repeats. Empty for a tool that
/// operates on no path, and for a name no tool in the registry answers to.
///
/// The tool decides — [`Tool::lock_paths`] — because the keys have to be the
/// files it will write: one reading of its arguments, shared with its own
/// execution, instead of a second one here that could disagree. A name-keyed
/// table would be a second place to teach, and the first place to forget.
pub fn file_keys(
    registry: &ToolRegistry,
    cwd: &Path,
    name: &str,
    arguments: &Value,
) -> Vec<PathBuf> {
    let Some(tool) = registry.get(name) else {
        return Vec::new();
    };
    let mut keys = tool.lock_paths(arguments, cwd);
    // One order for every call, so a call holding two of these cannot wait on a
    // call holding the other one.
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project_dir(name: &str) -> PathBuf {
        crate::config::test_dir(name, None)
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

    /// Multiple edits to one file in one tool call share the original snapshot
    /// and are committed together under that file's batch lock.
    #[tokio::test]
    async fn multiple_edits_to_one_file_apply_together() {
        let dir = project_dir("edit-batch-lock");
        fs::write(dir.join("note.txt"), "foo middle end").unwrap();

        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "edit".to_string(),
            arguments: serde_json::json!({
                "path": "note.txt",
                "edits": [
                    {"old_text": "foo", "new_text": "bar"},
                    {"old_text": "end", "new_text": "done"}
                ]
            }),
            parse_error: None,
        };

        let results = run(registry, &[call], &dir).await;

        assert!(!results[0].result.is_error);
        assert_eq!(
            fs::read_to_string(dir.join("note.txt")).unwrap(),
            "bar middle done"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Separate calls in a batch still serialize on the normalized file path;
    /// batching replacements in one call must not weaken the existing lock.
    #[tokio::test]
    async fn separate_edits_to_one_file_still_take_the_file_lock() {
        let dir = project_dir("edit-call-lock");
        fs::write(dir.join("note.txt"), "foo").unwrap();

        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();
        let calls = vec![
            ToolCall {
                id: "call-1".to_string(),
                name: "edit".to_string(),
                arguments: serde_json::json!({
                    "path": "note.txt",
                    "edits": [{"old_text": "foo", "new_text": "bar"}]
                }),
                parse_error: None,
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "edit".to_string(),
                arguments: serde_json::json!({
                    "path": "sub/../note.txt",
                    "edits": [{"old_text": "foo", "new_text": "baz"}]
                }),
                parse_error: None,
            },
        ];

        let results = run(registry, &calls, &dir).await;
        let errors = results.iter().filter(|call| call.result.is_error).count();
        let final_content = fs::read_to_string(dir.join("note.txt")).unwrap();

        assert_eq!(errors, 1);
        assert!(final_content == "bar" || final_content == "baz");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A call that spans files holds every one of their locks, so a call sharing
    /// just one of them still waits. Without that, both would read the shared
    /// file and the later write would drop the earlier change in silence.
    #[tokio::test]
    async fn a_multi_file_edit_holds_every_file_it_names() {
        let dir = project_dir("edit-multi-lock");
        for file in ["a.txt", "b.txt", "c.txt"] {
            fs::write(dir.join(file), "x").unwrap();
        }

        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();
        let call = |id: &str, spanned: [&str; 2]| ToolCall {
            id: id.to_string(),
            name: "edit".to_string(),
            arguments: serde_json::json!({
                "edits": spanned.map(|path| serde_json::json!({
                    "path": path,
                    "old_text": "x",
                    "new_text": "y"
                }))
            }),
            parse_error: None,
        };
        let calls = vec![
            call("call-1", ["a.txt", "b.txt"]),
            call("call-2", ["b.txt", "c.txt"]),
        ];

        let results = run(registry, &calls, &dir).await;
        let errors = results.iter().filter(|call| call.result.is_error).count();

        assert_eq!(errors, 1, "the shared file must make the calls take turns");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The lock keys are the tool's own answer: the arguments it reads, resolved
    /// against the working directory. A tool that names no file takes no lock,
    /// and a name nothing answers to is not a file either.
    #[test]
    fn the_lock_keys_are_the_arguments_the_tool_names() {
        let dir = project_dir("lock-key");
        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();

        assert_eq!(
            file_keys(
                &registry,
                &dir,
                "read",
                &serde_json::json!({"path": "sub/../a.txt"})
            ),
            vec![dir.join("a.txt")]
        );
        assert!(
            file_keys(
                &registry,
                &dir,
                "bash",
                &serde_json::json!({"command": "ls"})
            )
            .is_empty()
        );
        assert!(
            file_keys(
                &registry,
                &dir,
                "nonesuch",
                &serde_json::json!({"path": "a.txt"})
            )
            .is_empty()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A call that edits several files holds a key for each of them, so a second
    /// call naming any one of those files still takes turns behind it. The keys
    /// come back sorted: two calls sharing a file must take it in the same order
    /// or they can each hold what the other waits for.
    #[test]
    fn a_multi_file_edit_locks_every_file_it_names() {
        let dir = project_dir("lock-key-many");
        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();

        let keys = file_keys(
            &registry,
            &dir,
            "edit",
            &serde_json::json!({
                "edits": [
                    {"path": "b.txt", "old_text": "b", "new_text": "B"},
                    {"path": "a.txt", "old_text": "a", "new_text": "A"},
                    {"path": "sub/../b.txt", "old_text": "c", "new_text": "C"}
                ]
            }),
        );

        assert_eq!(keys, vec![dir.join("a.txt"), dir.join("b.txt")]);
        let _ = fs::remove_dir_all(&dir);
    }
}
